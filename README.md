# rsw — Legacy build（Windows 7 / Server 2008 R2）

> 本分支不承载新代码：源码与 [`v0.1.2`](https://github.com/corolin/winsvc-wrapper/releases/tag/v0.1.2) 完全一致，仅含构建配方（钉死工具链 + 发布 workflow）。legacy 发布永远为 pre-release。主线（stable，Win10 基线）见 [main README](https://github.com/corolin/winsvc-wrapper/tree/main)。

## 版本兼容性

| 系统 | 内核 | 状态 |
|---|---|---|
| Windows 7 SP1 x64 | 6.1 | ✅ 支持（工具链基线） |
| Windows Server 2008 R2 / SP1 x64 | 6.1 | ✅ 支持，真机实测通过 |
| Windows Server 2008（非 R2）/ Vista x64 | 6.0 | ❌ 不支持，见下 |
| 32 位 x86 | 任意 | ❌ 不构建 |

**实测记录（2008 R2 SP1 x64 真机）**：validate → install → start → SCM RUNNING → 子进程存活于 Session 0 → stop（干净树杀，退出码 0）→ uninstall（服务消失，sc 退出 1060）。全部断言基于数字退出码 / SCM 状态，不做本地化文本匹配。

**6.0 为何不支持**：std 的 win7 回退路径静态导入了 `kernel32!TryAcquireSRWLockExclusive`——SRW 锁本体 Vista 就有，但 Try 变体 Win7 才加入，6.0 加载器在任何代码执行前即中止（`STATUS_ENTRYPOINT_NOT_FOUND`）。修复等于自维护 std 补丁，超出本工程范围。已在 2008 SP2 x64 真机复现并归因。

## 为什么只发 pre-release

- **工具链换挡**：默认 `x86_64-pc-windows-msvc` 目标是 Win10 基线，std 静态导入 `api-ms-win-crt-*`、`api-ms-win-core-synch-l1-2-0`、`bcryptprimitives`，在 6.1 内核上加载即死。legacy 改用 tier-3 `x86_64-win7-windows-msvc` 目标从源码重编 std（走 6.1 安全回退）并静态链 CRT——零源码改动，但工具链从 stable 换成钉死的官方 nightly（`nightly-2026-09-08`，dated snapshot，与 stable 同源、非 fork；升钉显式自愿，见 [`rust-toolchain.toml`](rust-toolchain.toml)）。
- **验证矩阵更窄**：主线跑全量 e2e；legacy 目前仅 2008 R2 SP1 x64 真机全回合（未覆盖项见下节）。
- **结论**：依赖与主线共用同一份 `Cargo.lock`（`cargo audit` 门禁在主线 CI），产物同样是单文件静态 CRT exe——差异只在工具链与验证范围，所以永远标 pre-release、不占 Latest 位，这是对验证范围的诚实标注，不是缺陷警告。

## 验证方式与人工验收

Windows 服务的创建、账户和 DACL 操作天然需要管理员令牌；它们在本分支中是**发布前人工验收项**，不是要求普通开发者或 CI 模拟提权的自动化步骤。常规 `cargo test` 会跳过这些测试，因此“测试通过”不等同于已经完成服务安装路径的验收。

| 层级 | 命令 / 操作 | 覆盖范围 |
|---|---|---|
| 常规回归 | `cargo test --all-targets` | 配置、日志、下载、转换与前台 `run` 端到端场景；不需要管理员。 |
| 管理员 SCM 回归 | 在**提升权限的 PowerShell** 中执行下文的 Win7-target `cargo test … --ignored` 命令 | install / start / stop / refresh / uninstall、失败恢复动作、服务账户、`allow_start_stop` DACL。测试会创建并清理临时服务；账户用例还会创建并删除临时本地用户。 |
| 可读的生命周期验收 | 在提升权限的开发机 PowerShell 中执行 `powershell -ExecutionPolicy Bypass -File scripts\test-service.ps1` | 对固定测试服务输出逐步断言，包括 SCM 状态、日志、优雅 Ctrl+C 停止和 refresh。先执行 `cargo build --bins`；该脚本的默认 target 不用于 Win7 真机验收。 |
| 旧系统真机验收 | 使用本分支发布的 `x86_64-win7-windows-msvc` 包，在目标机器按 `validate → install → start → status → stop → uninstall` 运行实际配置 | 验证产物可由旧系统加载，以及真实 Session 0 服务路径。应记录系统版本、包的 SHA256 和每一步退出码。 |

人工验收通过的最低标准是：`status` 在启动后为 `Running`、停止后为 `Stopped`；服务日志与子进程日志均可写入；`uninstall` 后服务不再存在（`sc query <id>` 返回 1060）。如配置启用了 `stop_signal = "ctrl-c"`，还应在子进程日志中确认其清理标记，证明信号由服务进程自身投递，而非依赖交互式控制台。

在可构建测试二进制的验证环境中，旧系统回归应显式使用 Win7 target，避免默认 MSVC target 生成 Win10 基线的测试程序：

```text
cargo test --target x86_64-win7-windows-msvc -Zbuild-std=std --test service_scm -- --ignored
```

该命令需要 `rust-toolchain.toml` 中的 pinned nightly 与 `rust-src`。只验证发布包时则不需要在旧系统安装 Rust；按上表的生命周期命令人工验收即可。

## 已知未覆盖

服务生命周期（install/start/stop/uninstall）已在 2008 R2 SP1 x64 全回合验证；该平台上尚未覆盖：开机自启（重启后 `start_type = "auto"`）、日志子系统、优雅停止（ctrl-c）路径。
