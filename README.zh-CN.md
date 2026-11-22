# CZI — Codex 会话跳转工具

[English](README.md) | 简体中文

CZI 是一个运行在 Linux 终端中的 Codex 会话选择器。它只读扫描本地 Codex 历史，帮助你搜索、浏览并恢复不同项目中的原生 Codex 会话。

选择会话后，CZI 会在会话原来的工作目录运行 `codex resume <完整会话 ID>`。启用 Fish 或 Bash 集成后，Codex 退出时，当前 Shell 也会停留在该项目目录。

- 搜索最近会话，查看会话名称、任务预览、项目路径和更新时间。
- 使用内联终端界面浏览，不覆盖之前的终端输出。
- 按项目浏览会话，或快速恢复上次使用 CZI 启动的会话。
- 支持 Fish 和 Bash；保留原生 Codex 会话 ID 和工作目录。

## 安装与快速开始

CZI 支持 x86-64 Linux 和 WSL2，以及 Bash 和 Fish。使用前请先安装并配置 Codex CLI；浏览和恢复会话需要本机已有可发现的 Codex 历史。CZI 目前仅支持 Codex。

### 方式一：下载 Release

Beta 发布后，可从 [GitHub Releases](https://github.com/boxijiekexun/czi/releases) 下载压缩包与校验文件。x86-64 GNU 版本需要 glibc 2.35 或更新版本。以下以 v0.3.4 为例：

```bash
curl -fLO https://github.com/boxijiekexun/czi/releases/download/v0.3.4/czi-v0.3.4-x86_64-unknown-linux-gnu.tar.gz
curl -fLO https://github.com/boxijiekexun/czi/releases/download/v0.3.4/SHA256SUMS
sha256sum -c SHA256SUMS
tar -xzf czi-v0.3.4-x86_64-unknown-linux-gnu.tar.gz
install -Dm755 czi/czi "$HOME/.local/bin/czi"
```

确保 `~/.local/bin` 在 `PATH` 中，然后运行 `czi doctor` 检查环境。使用预编译程序无需安装 Rust，也不需要 sudo。

### 方式二：从源码安装

安装 Rust 和 Cargo 后运行：

```bash
cargo install --git https://github.com/boxijiekexun/czi.git --locked
```

也可以克隆并构建：

```bash
git clone https://github.com/boxijiekexun/czi.git
cd czi
cargo build --release --locked
install -Dm755 target/release/czi "$HOME/.local/bin/czi"
```

### 启用 Shell 集成

在当前 Bash 中运行：

```bash
eval "$(czi init bash)"
```

或在当前 Fish 中运行：

```fish
czi init fish | source
```

然后运行 `czi` 浏览会话。若希望新 Shell 自动加载集成，可以将对应命令加入自己的 Shell 配置。CZI 不会自动修改配置文件。

Shell 集成会在恢复会话前切换**当前 Shell** 的目录，因此退出 Codex 后仍处于目标项目。直接运行独立二进制也能恢复会话，但不能改变父 Shell 的目录。升级 CZI 后，请在已有 Shell 中重新执行上述初始化命令。

## 命令

```text
czi             浏览所有项目的最近会话
czi .           浏览记录的工作目录为当前目录的会话
czi -p          以项目视图启动
czi last        恢复上次由 CZI 成功启动的会话
czi doctor      检查 Codex、CODEX_HOME 和发现的会话
czi init bash   输出 Bash 集成代码
czi init fish   输出 Fish 集成代码
```

默认显示最近会话，按 `Tab` 可切换项目视图。宽终端中显示 Session、Preview、Path 和 Updated 列。会话标题优先使用 Codex 的 `thread_name`；没有有效名称时显示 `Session · <短 ID>`。首条用户任务单独显示在 Preview 列。长文本会按终端显示宽度截断，支持中文和 emoji。

```text
┌─ czi · Recent · Browsing ─────────────────────────────────────────────────┐
│ / Search                                                                   │
│ Session                 Preview              Path                Updated   │
│ > Fix API auth         repair token flow     ~/work/backend       11-21 ...│
│   Session · a1b2c3d4  —                    ~/work/czi            —         │
│ ↑↓/jk move  Enter resume  PgUp/PgDn scroll  Tab view  / search  Esc back    │
│ 1/16                                                                        │
└───────────────────────────────────────────────────────────────────────────┘
```

## 键盘操作

- **浏览模式**：`↑`/`↓` 或 `j`/`k` 移动；`PageUp`/`PageDown` 翻页；`Home`/`End` 跳到边界；`/` 搜索；`Enter` 恢复；`Tab` 切换视图；`Esc` 或 `Ctrl-C` 退出。
- **搜索模式**：输入文本或使用 Backspace 编辑；`Enter` 确认筛选但不立即恢复；`Esc` 取消并恢复之前的选择；`Ctrl-C` 退出。
- **筛选后的浏览模式**：方向键、`j`/`k`、翻页键和 `Home`/`End` 用于导航，`Enter` 恢复，`Esc` 清除筛选。

选择器通过控制终端 `/dev/tty` 绘制和接收输入，并使用 Ratatui 内联视口。程序需要控制终端；没有控制终端时会报告错误。内部 `--shell-select` 接口在 stdout 输出 NUL 分隔的工作目录和完整会话 ID；取消时不输出选择结果。Shell 集成则使用私有结果文件。

启用集成后，`czi`、`czi .`、`czi -p` 和 `czi last` 会在启动 Codex 前切换当前 Shell 的目录；取消选择不会改变目录。`czi doctor`、`czi --version` 等其他命令交给独立程序执行。

## 测试

构建 Release 程序后，可运行 PTY 测试，检查终端与输出通道分离以及 Bash/Fish 集成：

```bash
python3 tests/tty_channel.py target/release/czi
python3 tests/stability_regressions.py target/release/czi
```

这些测试使用合成会话和假 Codex；需要 Python 3 和 Fish。`cargo test` 在相关程序可用时也会运行 PTY 检查。

多轮稳定性测试使用隔离的 HOME、`CODEX_HOME`、`XDG_STATE_HOME` 和临时目录，不读取真实 Codex 历史，也不运行真实 Codex。需要 Bash、Fish、Python 3 和 `stty`：

```bash
python3 tests/czi_stress.py --mode quick                  # 10 轮，适合日常检查
python3 tests/czi_stress.py --mode full                   # 50 轮，完整回归
python3 tests/czi_stress.py --mode stress                 # 200 轮，发布前检查
python3 tests/czi_stress.py --mode stress --rounds 200 --seed 2026
python3 tests/czi_stress.py --mode full --case navigation --seed 2026
```

轮数分别应用于长期运行的 Shell 中的取消、选择和 `last` 循环。Full 和 Stress 还覆盖非零退出码、`.` 和 `-p`；导航测试检查合成会话的 PTY 屏幕、固定种子操作和多种终端尺寸。资源指标在系统支持时报告。这些模拟测试不能代替真实终端与 Codex 环境中的人工验证。

## 数据与隐私

CZI 默认从 `$CODEX_HOME` 或 `~/.codex` 读取 `sessions/**/*.jsonl` 和可选的 `session_index.jsonl`，不会写入该目录，也不会通过网络发送数据。CZI 自身只在 `$XDG_STATE_HOME/czi/state.json`（或 `~/.local/state/czi/state.json`）记录上次成功启动的会话。

缺失或损坏的记录会被保守跳过。若记录的工作目录不存在，CZI 会拒绝恢复，而不会悄悄启动新会话。

## 兼容性与许可证

Codex 本地文件格式属于其实现细节，版本升级可能需要调整解析逻辑。详见 [兼容性说明](docs/compatibility.md)。

CZI 使用 [MIT 许可证](LICENSE.txt)。
