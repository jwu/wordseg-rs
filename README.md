# wordseg-rs

给**连写英文**重新分词的 stdin/stdout 过滤器。

```
stdin : myfellowamericansasknotwhatyourcountrycandoforyou
stdout: my fellow americans ask not what your country can do for you
```

最初是为 voxtype 的 `[output.post_process]` 写的，但它和 voxtype 没有耦合：任何需要把连写英文重新分开的地方，都能把它当命令行过滤器用。

## 为什么需要它

voxtype 当前的识别引擎 Paraformer 是字符级模型，英文会输出成一整串、没有空格：

```
Paraformer 原始: andsomyfellowamericansasknotwhatyourcountrycandoforyou
```

voxtype 自身不提供英文分词，只提供 `[output.post_process]` 这个通用的
stdin → stdout 管道，所以分词逻辑需要自己接。

## 来源

逻辑迁移自已归档的 [jwu/voice-input](https://github.com/jwu/voice-input)，行为保持一致：

- **外层规则**（原 `english_spacing.py`）：只处理 **7 个以上连续 ASCII 字母**，
  中文原样透传；随后做 `iam → I am`、孤立 `i → I` 的大小写修正
- **词频分词**（vendored **wordninja 2.0.0**，MIT）：词表按词频降序排列，
  每个词的代价为 `ln((rank + 1) · ln(N))`，用动态规划求整体代价最小的切分，
  并保留 wordninja 对撇号和数字的重接规则

改成 Rust 只为一件事：**去掉解释器**。现在是单一静态二进制、零运行时依赖，
启动约 7 ms（Python 版约 60 ms）。

## 构建与安装

```bash
cargo test --release                      # 单元测试
cargo install --path . --root ~/.local    # 安装到 ~/.local/bin/wordseg-rs
```

只要产物的话：`cargo build --release` → `target/release/wordseg-rs`（约 1.4 MB）。

词频表在编译期由 `build.rs` 解压并烘焙进二进制（`data/wordninja_words.txt.gz`，
126136 个词，538 KB）。专有名词表用 `include_str!` 嵌入，你自己那份在运行时读取
—— 见「专有名词」。

## 接入 voxtype

它由 voxtype 的 `[output.post_process]` 调用，但**不属于** voxtype 的部署
（那份配置在另一个 dotfiles 仓库里）。装到 `~/.local/bin` 之后：

```toml
[output.post_process]
command = "~/.local/bin/wordseg-rs"
timeout_ms = 5000
```

命令失败、超时或输出为空时，voxtype 会自动回退到原文，不会卡住输入。

## 行为验证

与迁移前的 Python 版逐字节对拍过（基准脚本随 voice-input 一起归档）：

| 样本 | 结果 |
| --- | --- |
| 手工样本（中英混说、数字、撇号、大小写、空串、短串） | 14/14 一致 |
| 随机拼词（从词表随机抽 2–7 个词拼接，300 条） | 300/300 一致 |
| 加入专有名词表后的回归（同样的 300 条 + 常见句子，共 314 条） | 314/314 与加表前逐字节一致 |

## 专有名词

词频表来自语料，因此缺少现代技术词和产品名。wordninja 给词表外的
 token 记 `+∞` 代价，于是 `kubernetes` 只能被拆成 `ku berne tes`。

`data/proper_nouns.txt` 补上这一层。每个词条按 rank 1_000_000 计价：
比任何真实词都贵，但比「两三个普通词拼出来」便宜得多（`ku berne tes` 要
35.4 nats，这个档位约 16.3）。词条同时决定输出拼写，所以 `kubernetes`
出来就是 `Kubernetes`。

词条通过两个入口起作用：

1. **整体匹配** —— 一个字母串恰好等于词条时直接替换，不看长度。这是短词唯一的
   修复途径：`github` 只有 6 个字母，永远进不了分词器。
2. **参与分词** —— 词条进 DP 的代价表，因此嵌在长串里的也能切对：
   `deploykubernetesnow` → `deploy Kubernetes now`。

收录的边界是「wordninja 自己切不对」。普通英文词一律不收 —— 否则 `a fish`
会被改写成 `a Fish`。

### 加自己的词

放一个词表到 `~/.config/wordseg-rs/words.txt`，**存盘即生效** —— 不用重编译：

```bash
mkdir -p ~/.config/wordseg-rs
cat >> ~/.config/wordseg-rs/words.txt <<'EOF'
wordseg-rs
voxtype
aicanvas
EOF
```

格式与 `data/proper_nouns.txt` 相同（一行一个词，`#` 开头是注释）。词条里的
`-`、`_` 和空格在匹配时会被忽略，显示形式则原样输出 —— 所以写 `wordseg-rs`
就能把 `wordsegrs` 还原成 `wordseg-rs`。后面的定义覆盖前面的，因此你的词表
可以改掉内置词条的拼写。

也可以用环境变量指向别处，或指向多个文件（冒号分隔）：

```bash
WORDSEG_WORDS=~/my-words.txt wordseg-rs
```

文件不存在、没权限、或某行写坏了，都只是被跳过 —— 过滤器不会因此失败，
内置那份继续生效。

读一个几百行的词表带来的启动开销落在噪声里（实测 100 次：1.163 s 无词表，
1.149 s 带 500 行词表），所以没理由为它牺牲「改完即生效」。

### 已知限制

少数词条恰好能拆成两个极常见的词，拆开反而更便宜，于是只有整体匹配能救它们：

```
andthenredisandmysql → and then red is and mysql
```

这是代价函数的固有取舍：把 `redis` 压到能赢过 `red is` 的位置，普通英文里的
`red is` 就会被粘成 `Redis`。目前选择不误伤普通文本。

## 许可

- 本项目代码：MIT
- 词表：wordninja 的 `wordninja_words.txt.gz`，见 `data/wordninja-LICENSE`
