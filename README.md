# wordninja-rs

voxtype 的后处理过滤器：给 ASR 输出的**连写英文**重新分词。

```
stdin : myfellowamericansasknotwhatyourcountrycandoforyou
stdout: my fellow americans ask not what your country can do for you
```

## 为什么需要它

voxtype 当前的识别引擎 Paraformer 是字符级模型，英文会输出成一整串、没有空格：

```
Paraformer 原始: andsomyfellowamericansasknotwhatyourcountrycandoforyou
```

voxtype 自身不提供英文分词，只提供 `[output.post_process]` 这个通用的
stdin → stdout 管道，所以分词逻辑需要自己接。

## 来源

从 `~/bin/voice-input` 迁移过来的逻辑，行为保持一致：

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
cargo install --path . --root ~/.local    # 安装到 ~/.local/bin/wordninja-rs
```

只要产物的话：`cargo build --release` → `target/release/wordninja-rs`（约 1.4 MB）。

词表在编译期由 `build.rs` 解压并烘焙进二进制
（`data/wordninja_words.txt.gz`，126136 个词，538 KB）。

## 接入 voxtype

它由 voxtype 的 `[output.post_process]` 调用，但**不属于** voxtype 的部署
（那份配置在另一个 dotfiles 仓库里）。装到 `~/.local/bin` 之后：

```toml
[output.post_process]
command = "~/.local/bin/wordninja-rs"
timeout_ms = 5000
```

命令失败、超时或输出为空时，voxtype 会自动回退到原文，不会卡住输入。

## 行为验证

与 Python 版（`~/bin/voice-input/src/voxtype_post.py`）逐字节对拍：

| 样本 | 结果 |
| --- | --- |
| 手工样本（中英混说、数字、撇号、大小写、空串、短串） | 14/14 一致 |
| 随机拼词（从词表随机抽 2–7 个词拼接，300 条） | 300/300 一致 |

## 已知代价

词频分词不认识专有名词，可能误切：

```
Kubernetes → Ku berne tes
```

这是算法的固有局限（Python 版同样如此），不是移植引入的。

## 许可

- 本项目代码：MIT
- 词表：wordninja 的 `wordninja_words.txt.gz`，见 `data/wordninja-LICENSE`
