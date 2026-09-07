# rsw — Rust Service Wrapper

[![CI](https://github.com/corolin/winsvc-wrapper/actions/workflows/ci.yml/badge.svg)](https://github.com/corolin/winsvc-wrapper/actions/workflows/ci.yml)
[License: MIT](https://github.com/corolin/winsvc-wrapper/blob/main/LICENSE)

**WinSW-style declarative configuration + a native Rust runtime.** `rsw` wraps any
executable as a Windows service: one small static binary, a TOML (or YAML) config
file next to it, and nothing else to install.

```toml
# myapp.toml — relative paths resolve against this file's directory
[service]
id = "myapp"
name = "My Awesome App"
description = "Rust driven backend service"

[process]
executable = "app.exe"
arguments = ["--port", "8080"]

[env]
DATABASE_URL = "postgres://user:pass@localhost/db"

[logging]
mode = "roll-by-size"   # rotate at 10 MB, keep 8 files (defaults shown)
```

```text
> rsw install myapp.toml    # elevated prompt
> rsw start myapp.toml
```

Grab a prebuilt `rsw.exe` from the
[releases page](https://github.com/corolin/winsvc-wrapper/releases), or build
it yourself with `cargo build --release --bin rsw`.

## Highlights

- **Zero runtime dependencies** — no .NET, no JVM; a single ~3 MB `rsw.exe`.
- **Declarative & sidecar-friendly** — rename `rsw.exe` to `app.exe` and it picks
  up `app.toml` / `app.yaml` / `app.yml` automatically.
- **Supervised children** — stdout/stderr capture through pipes (no locked log
  files), size/time-based rotation with retention, in-wrapper restart with
  exponential backoff *plus* SCM-native failure actions.
- **Graceful stop ladder** — stop helper command → console Ctrl event
  (reaching even separate/hidden consoles) → close-window → timeout →
  TerminateProcess → job-object tree kill. Grandchildren never survive.
- **WinSW feature parity** — start types incl. delayed, dependencies, service
  accounts with automatic *SeServiceLogonRight* grant, `on_failure` recovery
  actions, pre/post hooks, startup downloads, drive mapping, SDDL security
  descriptors, `refresh` without reinstall.
- **Both config languages** — TOML and YAML, same schema, detected by extension.

## CLI

| Command | Effect |
|---|---|
| `rsw install [config]` | Install the service (triggers one UAC prompt when needed) |
| `rsw uninstall [config]` | Stop if needed, then delete the service |
| `rsw start / stop / restart [config]` | Lifecycle |
| `rsw status [config]` | Print state; exit 0 running/stopped, 1 transitional, 1060 not installed |
| `rsw refresh [config]` | Re-read config and update service properties in place |
| `rsw run [config]` | Run in the foreground for debugging (no SCM, no admin, Ctrl+C stops) |
| `rsw validate [config]` | Parse + validate and print the resolved config |

`[config]` is optional when using the sidecar rename convention.

Admin commands (`install`/`uninstall`/`start`/`stop`/`restart`/`refresh`) run
from a non-elevated terminal trigger **one UAC prompt** and complete in place;
the elevated output is echoed back to your terminal. Pass `--no-elevate` to
disable this and fail with a hint instead.

## Configuration reference

Full schema (TOML shown; YAML is identical). Everything except `service.id`
and `process.executable` is optional — the values below are the defaults.

```toml
[service]
id = "myapp"                      # REQUIRED: SCM service id
name = "My App"                   # display name (default: id)
description = ""
start_type = "auto"               # auto | delayed | manual | disabled | boot | system
dependencies = []                 # other service ids that must start first
failure_reset_after = "1 day"     # SCM failure-counter reset window
preshutdown = false               # accept PRESHUTDOWN notifications
preshutdown_timeout_secs = 180
# security_descriptor = "D:P(A;;GA;;;BA)(A;;GR;;;AU)"   # SDDL

# [service.account]               # omit for LocalSystem
# username = ".\\svc_user"        # or DOMAIN\\user or NT AUTHORITY\\NetworkService
# password = "..."                # empty for virtual accounts
# allow_logon_as_service = true   # grant SeServiceLogonRight at install

[process]
executable = "app.exe"            # REQUIRED; bare names resolve via PATH
arguments = []
start_arguments = []              # if non-empty, used at start instead (WinSW)
working_dir = ""                  # default: config dir
stop_executable = ""              # stop helper command (replaces signaling)
stop_arguments = []
stop_signal = "kill"              # kill (immediate tree kill) | ctrl-c | ctrl-break
stop_timeout_secs = 15
priority = "normal"               # idle|below_normal|normal|above_normal|high|realtime
hide_window = true
success_exit_codes = [0]          # mapped to service NO_ERROR

[process.restart]                 # in-wrapper supervision (milliseconds-fast)
policy = "on-failure"             # on-failure | always | never | custom
restart_if = []                   # custom: restart only for these codes
restart_if_not = []               # custom: restart for all but these codes
delay_secs = 5
backoff = true                    # 5→10→20… capped at 60s; resets after 60s stable
max_retries = 0                   # 0 = unlimited; on limit, hand over to on_failure

[env]                             # injected into the child; SERVICE_ID is automatic
KEY = "value"                     # %BASE% and %VAR% expand everywhere

[[on_failure]]                    # SCM-native recovery (survives rsw itself dying)
action = "restart"                # restart | reboot | none
delay = "5 sec"                   # ms/sec/min/hr/day suffixes; bare number = ms

[logging]
dir = "logs"                      # default: config dir
basename = ""                     # default: config file stem
wrapper_log = true                # wrapper events → <base>.wrapper.log
mode = "roll-by-size"             # append|reset|none|roll|roll-by-size|roll-by-time|roll-by-size-time
size_threshold_mb = 10
time_pattern = "yyyyMMdd"         # tokens: yyyy yy MM dd HH mm ss
auto_roll_at = "00:00:00"         # roll-by-size-time only
keep_files = 8                    # -1 keeps everything
merge_stderr = false

[[download]]                      # fetched at every service start, before hooks
from = "https://example.com/agent.jar"
to = "agent.jar"
fail_on_error = false
# proxy = "http://user:pass@host:port"
# auth = { kind = "basic", user = "u", password = "p" }

[[map_drive]]                     # WNetAddConnection2 before the child starts
label = "N:"
unc_path = "\\\\fileserver\\share"

[hooks]                           # each: executable + arguments + stdout/stderr paths
# pre_start  = { executable = "prep.cmd", arguments = [], stdout = "NUL", stderr = "NUL", timeout_secs = 60 }
# post_start = { ... }  pre_stop = { ... }  post_stop = { ... }
```

### Log file naming

- Child stdout → `<base>.out.log`, stderr → `<base>.err.log` (or merged).
- Wrapper events → `<base>.wrapper.log` (append).
- Size rotation renames to `.log.1`, `.log.2`, … (newest first), pruned to `keep_files`.
- Time rotation writes `<base>.<period>.out.log` directly, deleting the oldest
  beyond `keep_files`.

### The two restart layers

**By default — no `[process.restart]` section — every child exit (any exit
code) exits the service with that code and is reported straight to the SCM:
that is WinSW's behavior, and `[[on_failure]]` recovery takes over
immediately.**

With an explicit `[process.restart]` section you get in-wrapper restarts
(millisecond-fast, pipes and logs stay attached): `policy = "on-failure"`
retries by exit code, `max_retries` bounds the attempts, and once exhausted
the service exits with the child's code and hands over to `[[on_failure]]`.
The two layers combine exactly as configured: retries happen when you ask
for them, failures are reported when you don't. Even if the rsw process
itself dies, the job object terminates the whole child tree.

## Migrating from WinSW

| WinSW XML | rsw TOML |
|---|---|
| `<id>` | `[service] id` |
| `<name>` / `<description>` | `name` / `description` |
| `<executable>` / `<arguments>` | `[process] executable` / `arguments` |
| `<startarguments>` | `start_arguments` |
| `<stopexecutable>` / `<stoparguments>` | `stop_executable` / `stop_arguments` |
| `<workingdirectory>` | `working_dir` |
| `<env name="X" value="Y"/>` | `[env] X = "Y"` |
| `<startmode>` + `<delayedAutoStart>` | `start_type = "delayed"` |
| `<depend>` | `dependencies = [...]` |
| `<serviceaccount>` (+ `allowservicelogon`) | `[service.account]` (+ `allow_logon_as_service`) |
| `<onfailure action="restart" delay="5 sec"/>` + `<resetfailure>` | `[[on_failure]]` + `failure_reset_after` |
| `<stoptimeout>` | `stop_timeout_secs` |
| `<log mode="roll-by-size">` `sizeThreshold` `keepFiles` | `mode = "roll-by-size"`, `size_threshold_mb`, `keep_files` |
| `<log mode="roll-by-time"> pattern` | `mode = "roll-by-time"`, `time_pattern` |
| `%BASE%` | `%BASE%` (same) |
| `<download>` | `[[download]]` |
| `<sharedDirectoryMapping>` | `[[map_drive]]` |
| `<preshutdown>` / `<preshutdownTimeout>` | `preshutdown` / `preshutdown_timeout_secs` |
| `refresh` | `refresh` |

Not ported: zip log archiving (broken in WinSW), `beeponshutdown`,
`customize`/`dev` commands, interactive services, the v2 extension model.

## Building & testing

```text
cargo build --release --bin rsw
cargo test                                   # unit + foreground e2e (no admin needed)
cargo test --test service_scm -- --ignored   # SCM roundtrip (elevated prompt)
scripts\test-service.ps1                     # SCM regression (elevated PowerShell)
scripts\test-runtimes.ps1                    # node/python/java smoke (no admin)
```

Prebuilt Windows binaries are attached to each
[release](https://github.com/corolin/winsvc-wrapper/releases) (~3 MB, static,
x86_64).

## License

MIT — see [LICENSE](LICENSE).

## Design notes

- **Signals in session 0**: services have no console, so rsw allocates a hidden
  one. When a child lives in a different console (`hide_window`), rsw attaches
  to the child's console to deliver the Ctrl event, then re-attaches its own.
  Processes created via `CREATE_NEW_PROCESS_GROUP` start with ctrl-c disabled;
  rsw re-enables it explicitly, both for itself and via `ctrl_break` targeting.
- **Orphan protection**: every child joins a kill-on-close job object, so if
  rsw dies for any reason the kernel terminates the whole tree.
- **All-synchronous threading**: no async runtime; reader threads pump child
  output line-by-line so rotation never fights the child over file handles.

## VibeCoding With ZCode + GLM-5.3

This project went from an empty folder to a tagged release in a single day,
built entirely through [ZCode](https://z.ai/) running
[GLM-5.3](https://open.bigmodel.cn/): schema design, the full SCM lifecycle,
a cross-audit of the shawl and WinSW sources, hunting down Windows
console-signal races, converting and deploying a real Node.js service, and
the docs you just read — ~6,100 lines of Rust, 40+ tests, all green.

No code was written by hand. The human's contributions: picking the battles,
supplying the real-world deployment (and keeping its passwords), and asking
"why" at exactly the right moments.

## Migrating checklist

Switching from WinSW right now:

1. `rsw convert <yours>.xml` — generates the TOML next to your XML
2. swap the WinSW exe for `rsw.exe`, renamed to match the TOML stem
3. `rsw install <yours>.toml` — one UAC prompt
4. accounts: declare `[service.account]` for fleet/template deployments
   (log-on-as-a-service is granted automatically; passwords can come from
   env vars so they never sit in the file), or leave it out and set the
   account in `services.msc` — `rsw refresh` keeps whatever Windows already
   stores
