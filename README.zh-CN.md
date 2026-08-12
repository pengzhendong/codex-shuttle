<div align="center">

# 🚀 Codex Shuttle

**在 Mac 上保留 Codex 桌面体验，把实际工作放到 Linux 服务器执行。**

[English](README.md) · [架构](docs/architecture.md) · [故障排查](docs/troubleshooting.md) · [版本发布](https://github.com/pengzhendong/codex-shuttle/releases)

[![CI](https://github.com/pengzhendong/codex-shuttle/actions/workflows/ci.yml/badge.svg)](https://github.com/pengzhendong/codex-shuttle/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/pengzhendong/codex-shuttle?display_name=tag)](https://github.com/pengzhendong/codex-shuttle/releases)
[![License](https://img.shields.io/github/license/pengzhendong/codex-shuttle)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-1.85%2B-dea584?logo=rust)](rust-toolchain.toml)
[![macOS](https://img.shields.io/badge/macOS-arm64%20%7C%20x86__64-000000?logo=apple)](#环境要求)
[![Linux](https://img.shields.io/badge/远程%20Linux-arm64%20%7C%20x86__64-fcc624?logo=linux&logoColor=black)](#环境要求)

> [!WARNING]
> 这是非官方且仍在积极开发的项目，依赖版本敏感的 Codex App Server 和实验性的 Exec Server 接口。

</div>

Codex Shuttle（`cxs`）把 Codex 桌面 App 连接到已有的 Linux SSH 主机。会话、账号状态和界面留在 Mac；Shell、文件读写、搜索、Git 探测、PTY、测试和沙箱进程在 Linux 上运行。

它复用现有 OpenSSH 配置，通过一条普通 SSH stdio 连接工作；不替换 SSH 服务、不需要 TCP 转发，也不需要把 Codex 登录凭据放到服务器。

## 为什么使用 Shuttle？

- **原生桌面体验**：继续使用 Codex App、本地账号和本地会话库。
- **真正的远程路径**：直接浏览和打开 Linux 目录，不再显示伪装的 Mac 路径。
- **远程执行**：命令、终端、文件操作、搜索、Git 元数据和沙箱都在 Linux 上运行。
- **精简远程运行时**：只安装版本匹配的 App Server/Exec Server，不安装完整 CLI/TUI。
- **单条 SSH 连接**：Yamux 在一条 SSH stdin/stdout 上承载 App、Exec、Host 三类通道。
- **会话迁移**：把服务器创建的 session 拉回 Mac，并修复 Provider 导致的不可见问题。
- **版本绑定发布**：每小时检测官方稳定源码版本，同时永久保留旧版 Release。

## 环境要求

| Mac 本地 | Linux 远程 |
| --- | --- |
| Apple Silicon 或 Intel macOS | arm64 或 x86_64 Linux |
| Codex 桌面 App / 匹配的 Codex 二进制 | 支持免交互密钥登录的 OpenSSH |
| OpenSSH 客户端 | `sh`、`curl`、`tar`、`sha256sum` |

Shuttle Release 必须匹配本机 Codex 的公开源码基线。例如桌面版显示 `0.147.0-alpha.6.5`，应选择 Codex `0.147.0` 对应的 Shuttle Release。

## 快速开始

### 1. 安装 `cxs`

安装器会自动识别 Mac 架构和本机 Codex 版本、校验 Release 文件，并把 `cxs` 安装到 `~/.local/bin`：

```bash
curl -fsSL https://raw.githubusercontent.com/pengzhendong/codex-shuttle/master/install.sh | sh
```

也可以使用 `wget`：

```bash
wget -qO- https://raw.githubusercontent.com/pengzhendong/codex-shuttle/master/install.sh | sh
```

如需明确指定 Codex 基线，可使用 `sh -s -- --version 0.147.0`。也可以前往 [Releases](https://github.com/pengzhendong/codex-shuttle/releases) 手动下载并校验文件。

### 2. 准备 SSH

在 `~/.ssh/config` 中创建或复用普通主机别名，并确认密钥登录无需输入密码：

```bash
ssh -o BatchMode=yes my-linux-host true
```

### 3. 添加并安装主机

```bash
cxs add my-linux-host --name devbox
cxs install devbox
cxs doctor devbox
```

Shuttle 会生成给 App 使用的 SSH 别名 `cxs-devbox`。在 Codex 桌面 App 中选择该主机，再打开 `/home/me/project` 这样的 Linux 路径。

默认由服务器下载匹配的 runtime。如果希望先在 Mac 下载再通过 SSH 上传：

```bash
cxs install devbox --local-download
```

## 常用命令

| 命令 | 用途 |
| --- | --- |
| `cxs add <ssh-host> [--name <profile>]` | 根据 `ssh -G` 创建或刷新配置 |
| `cxs install <profile>` | 安装匹配的远程 runtime 和 shim |
| `cxs update <profile>` | 按当前本地 Codex 更新远程组件 |
| `cxs up <profile>` / `cxs down <profile>` | 启动或停止本地桥接器 |
| `cxs doctor <profile>` | 检查 Codex、SSH、runtime、远程文件与 Linux 命令执行 |
| `cxs list` / `cxs status <profile>` | 查看配置和状态 |
| `cxs config <profile>` | 输出生成的 SSH Host 配置 |
| `cxs rollback <profile>` | 回退到上一份远程 Release |
| `cxs sync <profile>` | 导入服务器 session，且不覆盖本地 thread |
| `cxs repair` | 备份并修复本地 Provider/session 元数据 |
| `cxs remove <profile> [--remote]` | 删除本地状态，并可选删除远程 Shuttle 状态 |

所有选项可通过 `cxs <command> --help` 查看。

## Session 同步

正常情况下，session 由 Mac 持有。如果之前直接在 Linux 上运行过 Codex，可以导入服务器的 rollout：

```bash
cxs sync devbox
# 服务器使用自定义 CODEX_HOME：
cxs sync devbox --remote-home /srv/codex-home
```

`sync` 根据 thread ID 去重，不覆盖本地 session，也不会用远程 SQLite 替换 Mac 数据库。

如果切换 `model_provider` 后本地 session 消失，关闭 Codex 后运行：

```bash
cxs repair
```

`repair` 会先备份受影响的 rollout 和 SQLite，再修复 Provider 与工作目录元数据。Rust 实现参考了 MIT 许可的 [`codex-provider-sync`](https://github.com/Dailin521/codex-provider-sync) 核心思路，版权说明保留在 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。

## 工作原理

```text
Codex 桌面 App
        │  通过生成的 SSH 别名执行 app-server proxy
        ▼
 remote cxs-shim ═══ 单条 SSH stdio / Yamux ═══ local cxs-bridge
        │                                             │
        ├── 匹配版本的 Codex Exec Server ◄── exec ───┤
        └── 受限 Linux App Server       ◄── host ────┤
                                                      └── Mac App Server
                                                           持有 session
```

Mac App Server 仍然负责会话和账号状态。Shuttle 注册 Linux 执行环境，并把文件系统、进程等 Host RPC 路由到受限的 Linux App Server。远程 runtime 从匹配的 [OpenAI Codex](https://github.com/openai/codex) 公开 `rust-vX.Y.Z` 源码构建，只保留 Shuttle 需要的 App Server 和 Exec Server 入口。

协议细节见[架构文档](docs/architecture.md)，模块复用边界见[依赖边界](docs/dependency-boundaries.md)。

## 发布与兼容性

GitHub Actions 每小时检查一次 OpenAI 稳定 `rust-vX.Y.Z` 标签，并按顺序补齐每个尚未发布的版本。只有 workspace 测试、两种 Mac CLI、两种 Linux shim 和两种 Linux runtime 烟测全部通过，才会发布绑定版本的 Shuttle Release。这些门禁证明构建兼容；针对具体 Mac 和服务器的端到端检查仍以 `cxs doctor` 为准。构建失败不会生成半成品，并会在下一轮自动重试。

每个已发布的 Mac CLI 都内置完整 Release 标签，只从同一份不可变 Release 下载 shim/runtime。详见 [Runtime 发布流程](docs/runtime-release.md)和[兼容性说明](docs/compatibility.md)。

## 安全边界

- OpenSSH 继续负责加密、主机密钥、身份文件、Agent 和跳板机。
- 不会把 Codex 登录凭据复制到 Linux。
- 远程 Exec 与 Host App Server 使用相互隔离的私有 `CODEX_HOME`。
- runtime 和 shim 激活前必须通过 SHA-256 校验。
- 本地桥接器使用私有 Unix socket 和每个 profile 独立的随机令牌。
- 远程 Release 不可变，并保留上一份已验证 Release 用于回退。
- Session 导入会拒绝不安全的归档路径，且不覆盖已有 thread ID。

安全问题请参阅 [SECURITY.md](SECURITY.md)，常见安装问题见[故障排查](docs/troubleshooting.md)。

## 开发

```bash
cargo build --workspace
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

绑定 Codex 版本的远程 runtime 有意放在普通 Cargo workspace 之外。修改协议或发布逻辑前请阅读 [CONTRIBUTING.md](CONTRIBUTING.md)和 [Runtime 发布流程](docs/runtime-release.md)。

## 项目状态

Codex Shuttle 是独立社区项目，与 OpenAI 没有关联，也未得到 OpenAI 官方认可。版本敏感的上游接口发生变化时，可能需要同步调整兼容性。

## 许可证

[Apache License 2.0](LICENSE)。第三方版权说明见 [THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md)。
