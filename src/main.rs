//! Re-segments concatenated English ASR output: "myfellowamericansasknot" ->
//! "my fellow americans ask not".
//!
//! A stdin -> stdout filter for voxtype's `[output.post_process]`. It is a Rust
//! port of voice-input's `english_spacing.py` plus the vendored wordninja 2.0.0
//! word-frequency splitter (MIT, see `data/wordninja-LICENSE`). The word list is
//! baked into the binary by `build.rs`, so the result has no runtime
//! dependencies whatsoever — no Python, no data files.

use std::collections::HashMap;
use std::io::{self, Read, Write};

#[allow(dead_code)]
mod wordlist {
    include!(concat!(env!("OUT_DIR"), "/wordlist.rs"));
}

/// `english_spacing.py` only re-segments runs of 7+ ASCII letters; shorter runs
/// (ordinary words) are passed through untouched.
const MIN_RUN: usize = 7;

/// wordninja uses the literal `9e999` for out-of-vocabulary tokens, which
/// float-parses to +inf — so an unknown token is effectively unreachable.
const UNKNOWN_COST: f64 = f64::INFINITY;

struct Model {
    /// token -> `ln((rank + 1) * ln(N))`, exactly as wordninja computes it.
    costs: HashMap<&'static str, f64>,
    max_len: usize,
}

impl Model {
    fn new() -> Self {
        let words: Vec<&'static str> = wordlist::WORDS_BLOB.lines().collect();
        let ln_n = (words.len() as f64).ln();
        let mut costs = HashMap::with_capacity(words.len());
        for (rank, word) in words.into_iter().enumerate() {
            costs.insert(word, (((rank + 1) as f64) * ln_n).ln());
        }
        Self {
            costs,
            max_len: wordlist::MAX_WORD_LEN,
        }
    }

    fn cost_of(&self, token: &[char]) -> f64 {
        let lowered: String = token.iter().collect::<String>().to_ascii_lowercase();
        self.costs
            .get(lowered.as_str())
            .copied()
            .unwrap_or(UNKNOWN_COST)
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

        // Only long runs are candidates, and only if the split is worth it.
        if run.len() >= MIN_RUN {
            let parts = model.split_segment(run);
            if parts.len() > 1 {
                spaced.push_str(&parts.join(" "));
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
}
