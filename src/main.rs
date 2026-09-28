//! Re-segments concatenated English ASR output: "myfellowamericansasknot" ->
//! "my fellow americans ask not".
//!
//! A stdin -> stdout filter for voxtype's `[output.post_process]`. It is a Rust
//! port of voice-input's `english_spacing.py` plus the vendored wordninja 2.0.0
//! word-frequency splitter (MIT, see `data/wordninja-LICENSE`). Both word lists
//! are baked into the binary by `build.rs`, so the result has no runtime
//! dependencies whatsoever — no Python, no data files.
//!
//! On top of the frequency list sits a proper-noun list (`data/proper_nouns.txt`
//! plus an optional local one). Terms in it are handled in two ways: they take
//! part in the split as if they were words, and they are restored to their
//! published spelling on the way out.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;

mod wordlist {
    include!(concat!(env!("OUT_DIR"), "/wordlist.rs"));
}

/// The shipped proper-noun list, embedded so the binary stays self-contained.
/// A user's own list is read from disk at run time; see `Model::new`.
const BAKED_WORDS: &str = include_str!("../data/proper_nouns.txt");

/// `english_spacing.py` only re-segments runs of 7+ ASCII letters; shorter runs
/// (ordinary words) are passed through untouched.
const MIN_RUN: usize = 7;

/// wordninja uses the literal `9e999` for out-of-vocabulary tokens, which
/// float-parses to +inf — so an unknown token is effectively unreachable.
const UNKNOWN_COST: f64 = f64::INFINITY;

/// Proper nouns are priced as if they sat this far down the frequency list:
/// dearer than every real word, yet far cheaper than the two or three ordinary
/// words a failed split spells them out of (`ku berne tes` costs 35.4 nats,
/// while this rank costs about 16.3). Terms that decompose into two very common
/// words are the exception — `redis` = `red` + `is` costs 16.0 — and those are
/// caught by the whole-run match instead of by the split.
const PROPER_NOUN_RANK: f64 = 1_000_000.0;

struct Model {
    /// token -> `ln((rank + 1) * ln(N))`, exactly as wordninja computes it.
    costs: HashMap<&'static str, f64>,
    /// proper-noun lookup key (lowercase, punctuation stripped) -> published
    /// spelling. Owned, because a user's list is read at run time.
    proper: HashMap<String, String>,
    max_len: usize,
    /// The price a proper noun is charged in the split.
    proper_cost: f64,
}

/// Where a user's own word list is looked for. `WORDSEG_WORDS` (colon-separated)
/// wins outright; otherwise `~/.config/wordseg-rs/words.txt`. A missing or
/// unreadable file is not an error — the baked-in list still applies.
fn user_word_files() -> Vec<PathBuf> {
    if let Ok(paths) = env::var("WORDSEG_WORDS") {
        return paths
            .split(':')
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .collect();
    }
    match env::var_os("HOME") {
        Some(home) => vec![PathBuf::from(home).join(".config/wordseg-rs/words.txt")],
        None => Vec::new(),
    }
}

/// Read one word list into `into`. A line that is not a usable term is skipped
/// rather than fatal: a typo in a user's list must never make the filter fail,
/// because voxtype only falls back to the raw text when the command *fails*.
///
/// Punctuation a human would write (`-`, `_`, spaces) is dropped from the key, so
/// "wordseg-rs" matches the letters the splitter actually sees, while the line is
/// still emitted verbatim. Later entries win, which is how a private list
/// overrides the shipped spelling.
fn load_words(text: &str, into: &mut HashMap<String, String>) {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut key = String::with_capacity(line.len());
        let mut usable = true;
        for c in line.chars() {
            if matches!(c, ' ' | '-' | '_') {
                continue;
            }
            if !c.is_ascii_alphanumeric() {
                usable = false;
                break;
            }
            key.push(c.to_ascii_lowercase());
        }
        if usable && !key.is_empty() {
            into.insert(key, line.to_owned());
        }
    }
}

impl Model {
    fn new() -> Self {
        let words: Vec<&'static str> = wordlist::WORDS_BLOB.lines().collect();
        let ln_n = (words.len() as f64).ln();
        let mut costs = HashMap::with_capacity(words.len());
        for (rank, word) in words.into_iter().enumerate() {
            costs.insert(word, (((rank + 1) as f64) * ln_n).ln());
        }

        let mut proper = HashMap::new();
        load_words(BAKED_WORDS, &mut proper);
        for path in user_word_files() {
            if let Ok(text) = fs::read_to_string(&path) {
                load_words(&text, &mut proper);
            }
        }
        let max_proper_len = proper.keys().map(|k| k.chars().count()).max().unwrap_or(0);

        Self {
            costs,
            proper,
            max_len: wordlist::MAX_WORD_LEN.max(max_proper_len),
            proper_cost: ((PROPER_NOUN_RANK + 1.0) * ln_n).ln(),
        }
    }

    fn cost_of(&self, token: &[char]) -> f64 {
        let lowered: String = token.iter().collect::<String>().to_ascii_lowercase();
        if let Some(&cost) = self.costs.get(lowered.as_str()) {
            return cost;
        }
        if self.proper.contains_key(lowered.as_str()) {
            return self.proper_cost;
        }
        UNKNOWN_COST
    }

    /// The published spelling of an already-split token, if it is a known term.
    fn restore(&self, token: &str) -> Option<&str> {
        if let Some(display) = self.proper.get(token) {
            return Some(display);
        }
        let lowered = token.to_ascii_lowercase();
        self.proper.get(lowered.as_str()).map(String::as_str)
    }

    /// A whole run that is a proper noun, regardless of `MIN_RUN`. This is what
    /// keeps short terms right: "github" (6 letters) and "systemd" (7) never
    /// reach the splitter, so the split cannot be what fixes them.
    fn lookup(&self, run: &str) -> Option<&str> {
        if run.chars().count() > self.max_len {
            return None;
        }
        self.restore(run)
    }

    /// wordninja `LanguageModel._split`'s `best_match`: walk the window
    /// backwards and keep the cheapest (cost, length) pair. Python compares
    /// `(cost, k + 1)` tuples, so ties break toward the shorter token.
    fn best_match(&self, chars: &[char], cost: &[f64], i: usize) -> (f64, usize) {
        let lo = i.saturating_sub(self.max_len);
        let mut best: Option<(f64, usize)> = None;
        for (k, &prefix_cost) in cost[lo..i].iter().rev().enumerate() {
            let candidate = (prefix_cost + self.cost_of(&chars[i - k - 1..i]), k + 1);
            best = Some(match best {
                Some(current) if current <= candidate => current,
                _ => candidate,
            });
        }
        best.unwrap_or((0.0, 0))
    }

    /// Minimal-cost segmentation: forward DP to build the cost table, then
    /// backtrack to recover the tokens (including wordninja's apostrophe and
    /// digit re-attachment rules).
    fn split_segment(&self, chars: &[char]) -> Vec<String> {
        let len = chars.len();
        let mut cost = vec![0.0_f64; len + 1];
        for i in 1..=len {
            cost[i] = self.best_match(chars, &cost, i).0;
        }

        let mut out: Vec<String> = Vec::new();
        let mut i = len;
        while i > 0 {
            let (_, k) = self.best_match(chars, &cost, i);
            let token: String = chars[i - k..i].iter().collect();
            let mut new_token = true;
            if token != "'" {
                if let Some(last) = out.last_mut() {
                    let last_starts_with_digit =
                        last.chars().next().is_some_and(|c| c.is_ascii_digit());
                    if last == "'s" || (chars[i - 1].is_ascii_digit() && last_starts_with_digit) {
                        *last = format!("{token}{last}");
                        new_token = false;
                    }
                }
            }
            if new_token {
                out.push(token);
            }
            i -= k;
        }
        out.reverse();
        out
    }
}

/// `\b` in Python's regex — a word character is `[A-Za-z0-9_]`.
fn is_word_char(c: Option<char>) -> bool {
    matches!(c, Some(c) if c.is_ascii_alphanumeric() || c == '_')
}

/// `english_spacing.add_english_spaces`
fn add_english_spaces(text: &str, model: &Model) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut spaced = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if !chars[i].is_ascii_alphabetic() {
            spaced.push(chars[i]);
            i += 1;
            continue;
        }

        let start = i;
        while i < chars.len() && chars[i].is_ascii_alphabetic() {
            i += 1;
        }
        let run = &chars[start..i];
        let run_str: String = run.iter().collect();

        // A run that *is* a term: rewrite it whatever its length, because
        // MIN_RUN would otherwise leave short ones verbatim and lowercase.
        if let Some(display) = model.lookup(&run_str) {
            spaced.push_str(display);
            continue;
        }

        // Only long runs are candidates, and only if the split is worth it.
        if run.len() >= MIN_RUN {
            let parts = model.split_segment(run);
            if parts.len() > 1 {
                let restored: Vec<&str> = parts
                    .iter()
                    .map(|part| model.restore(part).unwrap_or(part.as_str()))
                    .collect();
                spaced.push_str(&restored.join(" "));
                continue;
            }
        }
        spaced.extend(run.iter());
    }

    let spaced = collapse_iam(&spaced);
    capitalize_lone_i(&spaced)
}

/// `re.sub(r"(?i)\biam\b", "I am", text)`
fn collapse_iam(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let is_iam = i + 3 <= chars.len()
            && chars[i..i + 3]
                .iter()
                .collect::<String>()
                .eq_ignore_ascii_case("iam");
        let at_word_boundary = !is_word_char(i.checked_sub(1).and_then(|p| chars.get(p)).copied())
            && !is_word_char(chars.get(i + 3).copied());
        if is_iam && at_word_boundary {
            out.push_str("I am");
            i += 3;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// `re.sub(r"(?i)\bi\b", "I", text)`
fn capitalize_lone_i(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    for (i, &c) in chars.iter().enumerate() {
        let at_word_boundary = !is_word_char(i.checked_sub(1).and_then(|p| chars.get(p)).copied())
            && !is_word_char(chars.get(i + 1).copied());
        if c.eq_ignore_ascii_case(&'i') && at_word_boundary {
            out.push('I');
        } else {
            out.push(c);
        }
    }
    out
}

fn main() {
    let model = Model::new();

    let mut input = String::new();
    if io::stdin().read_to_string(&mut input).is_err() {
        std::process::exit(1);
    }

    let output = add_english_spaces(&input, &model);

    let mut stdout = io::stdout().lock();
    if stdout.write_all(output.as_bytes()).is_err() || stdout.flush().is_err() {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spaced(text: &str) -> String {
        add_english_spaces(text, &Model::new())
    }

    #[test]
    fn splits_concatenated_english() {
        assert_eq!(
            spaced("myfellowamericansasknotwhatyourcountrycandoforyou"),
            "my fellow americans ask not what your country can do for you"
        );
    }

    #[test]
    fn leaves_cjk_untouched() {
        let cjk = "欢迎大家来体验达摩院推出的语音识别模型";
        assert_eq!(spaced(cjk), cjk);
    }

    #[test]
    fn keeps_short_runs_verbatim() {
        assert_eq!(spaced("say hello world"), "say hello world");
    }

    #[test]
    fn mixed_chinese_and_english() {
        // The Chinese half must survive byte-for-byte.
        let out = spaced("我用了dashboard");
        assert!(out.starts_with("我用了"), "got {out:?}");
    }

    #[test]
    fn capitalizes_lone_i() {
        assert_eq!(spaced("i think iam fine"), "I think I am fine");
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert_eq!(spaced(""), "");
    }

    #[test]
    fn shipped_word_list_is_well_formed() {
        let mut proper = HashMap::new();
        load_words(BAKED_WORDS, &mut proper);
        let entries = BAKED_WORDS
            .lines()
            .filter(|l| {
                let l = l.trim();
                !l.is_empty() && !l.starts_with('#')
            })
            .count();
        assert_eq!(
            proper.len(),
            entries,
            "data/proper_nouns.txt has a line that is not a usable term"
        );
    }

    #[test]
    fn malformed_user_words_are_skipped_not_fatal() {
        let mut proper = HashMap::new();
        load_words("good term\n!!bad!!\n中文\n\n# comment\n", &mut proper);
        assert_eq!(
            proper.get("goodterm").map(String::as_str),
            Some("good term")
        );
        assert_eq!(proper.len(), 1);
    }

    #[test]
    fn last_definition_wins() {
        let mut proper = HashMap::new();
        load_words("HuggingFace\n", &mut proper);
        load_words("hugging face\n", &mut proper);
        assert_eq!(
            proper.get("huggingface").map(String::as_str),
            Some("hugging face")
        );
    }

    #[test]
    fn splits_terms_the_word_list_does_not_know() {
        // The bug this list exists for: wordninja spells it out of real words.
        assert_eq!(spaced("kubernetes"), "Kubernetes");
        assert_eq!(spaced("pytorch"), "PyTorch");
        assert_eq!(spaced("huggingface"), "HuggingFace");
    }

    #[test]
    fn splits_terms_embedded_in_a_long_run() {
        assert_eq!(spaced("deploykubernetesnow"), "deploy Kubernetes now");
    }

    #[test]
    fn leaves_ordinary_english_alone() {
        // Terms that are also ordinary words are deliberately absent, so plain
        // prose must come out untouched.
        assert_eq!(spaced("a red fish and a bun"), "a red fish and a bun");
    }
}
