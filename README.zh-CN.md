# rsw — Rust 服务包装器

[![CI](https://github.com/corolin/winsvc-wrapper/actions/workflows/ci.yml/badge.svg)](https://github.com/corolin/winsvc-wrapper/actions/workflows/ci.yml)
[![zread](https://img.shields.io/badge/Ask_Zread-_.svg?color=00b0aa&labelColor=000000&logo=data%3Aimage%2Fsvg%2Bxml%3Bbase64%2CPHN2ZyB3aWR0aD0iMTYiIGhlaWdodD0iMTYiIHZpZXdCb3g9IjAgMCAxNiAxNiIgZmlsbD0ibm9uZSIgeG1sbnM9Imh0dHA6Ly93d3cudzMub3JnLzIwMDAvc3ZnIj4KPHBhdGggZD0iTTQuOTYxNTYgMS42MDAxSDIuMjQxNTZDMS44ODgxIDEuNjAwMSAxLjYwMTU2IDEuODg2NjQgMS42MDE1NiAyLjI0MDFWNC45NjAxQzEuNjAxNTYgNS4zMTM1NiAxLjg4ODEgNS42MDAxIDIuMjQxNTYgNS42MDFINC45NjE1NkM1LjMxNTAyIDUuNjAwMSA1LjYwMTU2IDUuMzEzNTYgNS42MDE1NiA0Ljk2MDFWMi4yNDAxQzUuNjAxNTYgMS44ODY2NCA1LjMxNTAyIDEuNjAwMSA0Ljk2MTU2IDEuNjAwMXoiIGZpbGw9IiNmZmYiLz4KPHBhdGggZD0iTTQuOTYxNTYgMTAuMzk5OUgyLjI0MTU2QzEuODg4MSAxMC4zOTk5IDEuNjAxNTYgMTAuNjg2NCAxLjYwMTU2IDExLjAzOTlWMTMuNzU5OUMxLjYwMTU2IDE0LjExMzQgMS44ODgxIDE0LjM5OTkgMi4yNDE1NiAxNC4zOTk5SDQuOTYxNTZDNS4zMTUwMiAxNC4zOTk5IDUuNjAxNTYgMTQuMTEzNCA1LjYwMTU2IDEzLjc1OTlWMTEuMDM5OUM1LjYwMTU2IDEwLjY4NjQgNS4zMTUwMiAxMC4zOTk5IDQuOTYxNTYgMTAuMzk5OVoiIGZpbGw9IiNmZmYiLz4KPHBhdGggZD0iTTEzLjc1ODQgMS42MDAxSDExLjAzODRDMTAuNjg1IDEuNjAwMSAxMC4zOTg0IDEuODg2NjQgMTAuMzk4NCAyLjI0MDFWNC45NjAxQzEwLjM5ODQgNS4zMTM1NiAxMC42ODUgNS42MDAxIDExLjAzODQgNS42MDAxSDEzLjc1ODRDMTQuMTExOSA1LjYwMDEgMTQuMzk4NCA1LjMxMzU2IDE0LjM5ODQgNC45NjAxVjIuMjQwMUMxNC4zOTg0IDEuODg2NjQgMTQuMTExOSAxLjYwMDEgMTMuNzU4NCAxLjYwMDFaIiBmaWxsPSIjZmZmIi8%2BCjxwYXRoIGQ9Ik00IDEyTDEyIDRMNCAxMloiIGZpbGw9IiNmZmYiLz4KPHBhdGggZD0iTTQgMTJMMTIgNCIgc3Ryb2tlPSIjZmZmIiBzdHJva2Utd2lkdGg9IjEuNSIgc3Ryb2tlLWxpbmVjYXA9InJvdW5kIi8%2BPC9zdmc%2B&logoColor=ffffff)](https://zread.ai/corolin/winsvc-wrapper)
[许可证：MIT](https://github.com/corolin/winsvc-wrapper/blob/main/LICENSE)

**WinSW 的声明式配置体验 + Shawl 的 Rust 原生运行时。** `rsw` 把任意可执行文件包装成
Windows 服务：一个小巧的静态二进制、一个放在旁边的 TOML（或 YAML）配置文件，仅此而已。

```toml
# myapp.toml — 相对路径一律基于本文件所在目录
[service]
id = "myapp"
name = "我的应用"
description = "Rust 驱动的后端服务"

[process]
executable = "app.exe"
arguments = ["--port", "8080"]

[env]
DATABASE_URL = "postgres://user:pass@localhost/db"

[logging]
mode = "roll-by-size"   # 超过 10 MB 轮转，保留 8 份（均为默认值）
```

```text
> rsw install myapp.toml    # 需管理员终端
> rsw start myapp.toml
```

预编译的 `rsw.exe`（静态链接，x86_64）可在
[Releases 页面](https://github.com/corolin/winsvc-wrapper/releases)下载，
也可自行 `cargo build --release --bin rsw` 构建。Release 附两种变体
（`rsw --version` 可辨认），校验和见 `SHA256SUMS.txt`：

- `…-full.zip`（约 3 MB）— 全功能；
- `…-lite.zip`（约 1.6 MB）— 完全离线的精简 wrapper，仅去掉 `[[download]]`
  （自行构建：`cargo build --release --no-default-features --bin rsw`）。
  lite 二进制遇到含 `[[download]]` 的配置：每条记警告后跳过；声明了
  `fail_on_error = true` 的条目会中止启动；`rsw validate` 只提示不报错。

## 特性

- **零运行时依赖** — 不需要 .NET / JVM，单个约 3 MB 的 `rsw.exe`
  （另有约 1.6 MB 的离线 **lite** 变体）。
- **声明式 + sidecar 约定** — 把 `rsw.exe` 改名为 `app.exe`，它自动读取
  `app.toml` / `app.yaml` / `app.yml`。
- **子进程监督** — 管道捕获 stdout/stderr（子进程从不直接持有日志句柄，杜绝
  文件锁死）、按大小/时间轮转并清理、包装器内毫秒级重启（指数退避），
  外加 SCM 原生失败恢复动作。
- **优雅停止阶梯** — 停止辅助命令 → 控制台 Ctrl 事件（可达独立/隐藏控制台）
  → 关闭窗口 → 超时 → TerminateProcess → Job Object 树杀。孙进程绝不残留。
- **对齐 WinSW 功能面** — 启动类型（含延迟启动）、依赖服务、服务账户
  （自动授予 *SeServiceLogonRight*）、`on_failure` 恢复动作、pre/post 钩子、
  启动前下载（支持自定义 CA 与 mTLS PEM——证书验证永不关闭）、网络盘映射、
  SDDL 安全描述符、免重装的 `refresh`。
- **TOML 与 YAML 双格式** — 同一 schema，按扩展名识别。

## 命令行

| 命令 | 作用 |
|---|---|
| `rsw install [配置]` | 安装服务（需要时自动弹一次 UAC） |
| `rsw uninstall [配置]` | 先停止（如运行中）再删除服务 |
| `rsw start / stop / restart [配置]` | 生命周期 |
| `rsw status [配置]` | 打印状态；退出码 0 运行/停止、1 过渡态、1060 未安装 |
| `rsw refresh [配置]` | 重读配置并原地更新服务属性 |
| `rsw run [配置]` | 前台调试运行（不进 SCM、无需管理员，Ctrl+C 触发停止阶梯） |
| `rsw validate [配置]` | 校验并打印解析后的配置 |
| `rsw convert winsw.xml` | 把 WinSW XML 服务定义转换成 rsw TOML |

使用改名约定时 `[配置]` 可省略。完整字段参考见
[README.md](README.md#configuration-reference)（英文，含全部默认值）。

管理类命令（`install`/`uninstall`/`start`/`stop`/`restart`/`refresh`）在普通
终端执行时会**自动弹一次 UAC** 并原地完成，提权进程的输出会回显到当前终端；
加 `--no-elevate` 可禁用自动提权（恢复为报错并提示）。

### 日志文件命名

- 子进程 stdout → `<base>.out.log`，stderr → `<base>.err.log`（或合并）。
- 包装器自身事件 → `<base>.wrapper.log`（追加）。
- 大小轮转改名为 `.log.1`、`.log.2`…（最新在前），按 `keep_files` 清理。
- 时间轮转直接写入 `<base>.<周期>.out.log`，超出 `keep_files` 删除最旧的。

### 双层重启模型

**默认不配置 `[process.restart]` 时：子进程退出（无论退出码）服务即随之退出并
把退出码上报 SCM —— 也就是 WinSW 的行为，`[[on_failure]]` 恢复动作立即接管。**

显式配置 `[process.restart]` 后启用包装器**内部**重启（毫秒级、管道与日志不断
连）：`policy = "on-failure"` 按退出码重试、`max_retries` 限制次数，超限后服务
退出并把退出码上报 SCM，交给 `[[on_failure]]`。两层如何配合完全由配置决定：
配置了重试就按配置重试，没配置就直接报错。即使 rsw 进程死亡，Job Object 也会
终结整棵子进程树。

## 从 WinSW 迁移

字段对照表见 [README.md](README.md#migrating-from-winsw)。未移植：zip 归档
（WinSW 官方承认已损坏）、`beeponshutdown`、`customize`/`dev` 子命令、
交互式服务、v2 扩展模型。`%BASE%` 变量语义与 WinSW 相同。

## 构建与测试

```text
cargo build --release --bin rsw
cargo test                                   # 单元测试 + 前台端到端（无需管理员）
cargo test --test service_scm -- --ignored   # SCM 回归（需提权终端）
scripts\test-service.ps1                     # SCM 回归（需管理员 PowerShell）
scripts\test-runtimes.ps1                    # node/python/java 冒烟（免管理员）
```

## 许可证

MIT — 见 [LICENSE](LICENSE)。

## 设计要点

- **Session 0 信号投递**：服务没有控制台，rsw 分配一个隐藏控制台；子进程在
  独立控制台（`hide_window`）时，rsw 先附着到子进程的控制台发信号再切回来。
  `CREATE_NEW_PROCESS_GROUP` 创建的进程默认禁用 ctrl-c，rsw 会显式重新启用，
  `ctrl_break` 则按进程组精准投递。
- **孤儿防护**：每个子进程都加入 kill-on-close 的 Job Object，rsw 因任何原因
  死亡时内核都会终结整棵进程树。
- **全同步线程模型**：无异步运行时；reader 线程逐行搬运子进程输出，轮转与
  子进程之间不会争抢文件句柄。

## VibeCoding With ZCode + GLM-5.3

本项目从空目录到打上 tag 发布，只用了一天，全程由
[ZCode](https://z.ai/) 驱动 [GLM-5.3](https://open.bigmodel.cn/) 完成：
schema 设计、完整 SCM 生命周期、对 shawl 与 WinSW 源码的交叉审计、
Windows 控制台信号竞态的排查、真实 Node.js 服务的转换部署，以及你正在
读的这些文档——约 6100 行 Rust，40+ 个测试，全部通过。

没有一行代码是人手写的。人类的贡献：选定战场、提供真实部署环境
（以及只属于他自己的密码）、并在恰到好处的时机问出那几个"为什么"。

## 迁移清单

现在就从 WinSW 切换过来：

1. `rsw convert <你的>.xml` —— 在 XML 旁边生成对应的 TOML
2. 用 `rsw.exe`（按 TOML 同名改名）替换 WinSW 的 exe
3. `rsw install <你的>.toml` —— 一次 UAC 确认
4. 账户两种姿势：机群/模板部署在 `[service.account]` 里声明（自动授予
   “作为服务登录”权限；密码可经环境变量注入，不落盘），单机手改则留空
   段落去 `services.msc` 设置——`rsw refresh` 保留 Windows 已存储的密码
