//! Configuration schema, dual-format loading (TOML/YAML), sidecar resolution,
//! path resolution, and validation.

use std::collections::BTreeMap;
use std::env;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Context as _;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};

// ---------------------------------------------------------------------------
// Duration strings (WinSW-style suffixes; bare number = milliseconds)
// ---------------------------------------------------------------------------

/// Parses `"5 sec"`, `"1 day"`, `"500"` (bare number = milliseconds) into milliseconds.
pub fn parse_duration_ms(s: &str) -> Result<u64, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty duration".into());
    }
    let (num, unit) = s.split_once(' ').unwrap_or((s, ""));
    let num: u64 = num
        .parse()
        .map_err(|_| format!("invalid duration `{s}`: expected a number"))?;
    let unit = unit.trim().to_ascii_lowercase();
    let mult: u64 = match unit.as_str() {
        "" | "ms" | "millis" | "millisecond" | "milliseconds" => 1,
        "sec" | "secs" | "second" | "seconds" => 1_000,
        "min" | "mins" | "minute" | "minutes" => 60_000,
        "hr" | "hrs" | "hour" | "hours" => 3_600_000,
        "day" | "days" => 86_400_000,
        _ => {
            return Err(format!(
                "unknown duration unit in `{s}` (supported: ms, sec, min, hr, day)"
            ));
        }
    };
    Ok(num.saturating_mul(mult))
}

fn duration_ms_from_str<'de, D: Deserializer<'de>>(d: D) -> Result<u64, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        N(u64),
        S(String),
    }
    match V::deserialize(d)? {
        V::N(ms) => Ok(ms),
        V::S(s) => parse_duration_ms(&s).map_err(D::Error::custom),
    }
}

fn serde_bool_str<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        B(bool),
        N(i64),
        S(String),
    }
    match V::deserialize(d)? {
        V::B(b) => Ok(b),
        V::N(n) => Ok(n != 0),
        V::S(s) => match s.to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" => Ok(true),
            "false" | "0" | "no" => Ok(false),
            other => Err(D::Error::custom(format!("invalid boolean `{other}`"))),
        },
    }
}

// ---------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub service: ServiceConfig,
    pub process: ProcessConfig,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub on_failure: Vec<OnFailureAction>,
    #[serde(default)]
    pub logging: LoggingConfig,
    #[serde(default)]
    pub download: Vec<DownloadConfig>,
    #[serde(default)]
    pub map_drive: Vec<MapDriveConfig>,
    #[serde(default)]
    pub hooks: HooksConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    /// SCM service ID (required, unique on the machine).
    pub id: String,
    /// Display name; defaults to the id.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub start_type: StartType,
    #[serde(default)]
    pub dependencies: Vec<String>,
    /// SCM failure-counter reset window (WinSW `resetfailure`), default "1 day".
    #[serde(default = "one_day_ms")]
    #[serde(deserialize_with = "duration_ms_from_str")]
    pub failure_reset_after: u64,
    #[serde(default)]
    pub preshutdown: bool,
    #[serde(default = "default_preshutdown_timeout")]
    pub preshutdown_timeout_secs: u64,
    /// Optional SDDL security descriptor applied to the service.
    #[serde(default)]
    pub security_descriptor: Option<String>,
    /// Optional service account; absent = LocalSystem.
    #[serde(default)]
    pub account: Option<AccountConfig>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StartType {
    #[default]
    Auto,
    Delayed,
    Manual,
    Disabled,
    Boot,
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    /// e.g. `.\svc_user`, `DOMAIN\svc_user`, or `NT AUTHORITY\NetworkService`.
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// Grant the account SeServiceLogonRight at install time (default true).
    #[serde(default = "default_true")]
    #[serde(deserialize_with = "serde_bool_str")]
    pub allow_logon_as_service: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessConfig {
    /// Executable to wrap (required). Relative paths resolve against the config dir.
    pub executable: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    /// When non-empty, replaces `arguments` when starting (WinSW compatibility).
    #[serde(default)]
    pub start_arguments: Vec<String>,
    /// Working directory; defaults to the config dir.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Optional stop helper command run instead of sending a signal.
    #[serde(default)]
    pub stop_executable: Option<String>,
    #[serde(default)]
    pub stop_arguments: Vec<String>,
    #[serde(default)]
    pub stop_signal: StopSignal,
    /// Graceful stop window before force kill (default 15). Ignored when
    /// stop_signal = "kill", which terminates the tree immediately.
    #[serde(default = "default_stop_timeout")]
    pub stop_timeout_secs: u64,
    #[serde(default)]
    pub priority: Priority,
    /// Accepted for WinSW compatibility only. rsw children always share the
    /// wrapper's hidden console (a separate `CREATE_NO_WINDOW` console would
    /// make ctrl events undeliverable), so this flag has no effect.
    #[serde(default = "default_true")]
    #[serde(deserialize_with = "serde_bool_str")]
    pub hide_window: bool,
    /// Exit codes treated as success (mapped to NO_ERROR); default [0].
    #[serde(default = "default_success_exit_codes")]
    pub success_exit_codes: Vec<u32>,
    #[serde(default)]
    pub restart: RestartConfig,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StopSignal {
    #[serde(alias = "ctrl_c")]
    CtrlC,
    #[serde(alias = "ctrl_break")]
    CtrlBreak,
    /// Default: skip graceful signals entirely — stop means TerminateProcess
    /// plus a job-object tree kill, immediately.
    #[default]
    Kill,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Priority {
    Idle,
    #[serde(alias = "below_normal")]
    BelowNormal,
    #[default]
    Normal,
    #[serde(alias = "above_normal")]
    AboveNormal,
    High,
    Realtime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RestartConfig {
    #[serde(default)]
    pub policy: RestartPolicy,
    /// Only used with `policy = "custom"` (Shawl semantics).
    #[serde(default)]
    pub restart_if: Vec<u32>,
    #[serde(default)]
    pub restart_if_not: Vec<u32>,
    #[serde(default = "default_restart_delay")]
    pub delay_secs: u64,
    /// Exponential backoff: delay * 2^n capped at 60s; resets after 60s of stable uptime.
    #[serde(default = "default_true")]
    #[serde(deserialize_with = "serde_bool_str")]
    pub backoff: bool,
    /// Consecutive restart attempts before rsw gives up and EXITS THE SERVICE
    /// with the child's exit code, so the SCM failure-count picks it up and
    /// `[[on_failure]]` actions fire. 0 = unlimited.
    #[serde(default)]
    pub max_retries: u32,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    #[serde(alias = "on_failure")]
    OnFailure,
    #[default]
    Never,
    Always,
    Custom,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnFailureAction {
    pub action: FailureAction,
    /// e.g. "5 sec" (WinSW suffixes; bare number = ms).
    #[serde(default)]
    #[serde(deserialize_with = "duration_ms_from_str")]
    pub delay: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FailureAction {
    Restart,
    Reboot,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoggingConfig {
    /// Log directory; defaults to the config dir.
    #[serde(default)]
    pub dir: Option<String>,
    /// Log basename; defaults to the config file stem.
    #[serde(default)]
    pub basename: Option<String>,
    /// Wrapper's own event log (`<base>.wrapper.log`).
    #[serde(default = "default_true")]
    #[serde(deserialize_with = "serde_bool_str")]
    pub wrapper_log: bool,
    #[serde(default = "default_log_mode")]
    pub mode: LogMode,
    /// Size threshold for roll-by-size(-time) modes, default 10 MB.
    #[serde(default = "default_size_threshold")]
    pub size_threshold_mb: u64,
    /// .NET-style date pattern for roll-by-time(-time): yyyy, MM, dd, HH, mm, ss tokens.
    #[serde(default = "default_time_pattern")]
    pub time_pattern: String,
    /// Time-of-day for roll-by-size-time, e.g. "00:00:00".
    #[serde(default)]
    pub auto_roll_at: Option<String>,
    /// Rotated files to keep; -1 keeps everything.
    #[serde(default = "default_keep_files")]
    pub keep_files: i64,
    /// Merge stderr into the stdout log.
    #[serde(default)]
    #[serde(deserialize_with = "serde_bool_str")]
    pub merge_stderr: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogMode {
    /// Append forever.
    Append,
    /// Truncate on each service start.
    Reset,
    /// Discard child output entirely.
    None,
    /// Rename current logs to `.old` on each service start.
    Roll,
    /// Rotate when the live file exceeds `size_threshold_mb`.
    #[serde(alias = "roll_by_size")]
    RollBySize,
    /// Write directly into a file named by `time_pattern` period.
    #[serde(alias = "roll_by_time")]
    RollByTime,
    /// Rotate on size threshold, period change, or `auto_roll_at`.
    #[serde(alias = "roll_by_size_time")]
    RollBySizeTime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadConfig {
    pub from: String,
    pub to: String,
    #[serde(default)]
    #[serde(deserialize_with = "serde_bool_str")]
    pub fail_on_error: bool,
    #[serde(default)]
    pub proxy: Option<String>,
    #[serde(default)]
    pub auth: Option<AuthConfig>,
    #[serde(default)]
    pub tls: Option<DownloadTlsConfig>,
    /// Whole-request budget (connect + headers + body) in seconds, default
    /// 120. A stalled server must not wedge the service start: the SCM only
    /// waits as long as rsw keeps reporting progress.
    #[serde(default = "default_download_timeout")]
    pub timeout_secs: u64,
}

/// TLS knobs for a single `[[download]]` entry. `ca` replaces the roots used
/// to verify the *server* certificate (verification is never disabled);
/// `client_cert` + `client_key` enable mTLS. Paths support `%BASE%`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadTlsConfig {
    #[serde(default)]
    pub ca: Option<String>,
    #[serde(default)]
    pub client_cert: Option<String>,
    #[serde(default)]
    pub client_key: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthConfig {
    #[serde(default)]
    pub kind: AuthKind,
    pub user: String,
    #[serde(default)]
    pub password: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthKind {
    #[default]
    Basic,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MapDriveConfig {
    /// Drive letter label, e.g. "N:".
    pub label: String,
    /// UNC path, e.g. `\\fileserver\share`.
    pub unc_path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HooksConfig {
    #[serde(default)]
    pub pre_start: Option<Hook>,
    #[serde(default)]
    pub post_start: Option<Hook>,
    #[serde(default)]
    pub pre_stop: Option<Hook>,
    #[serde(default)]
    pub post_stop: Option<Hook>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hook {
    pub executable: String,
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Output redirect target; "NUL" discards (default).
    #[serde(default = "default_nul")]
    pub stdout: String,
    #[serde(default = "default_nul")]
    pub stderr: String,
    /// Hook runtime bound before force kill (default 60).
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
}

impl Default for RestartConfig {
    fn default() -> Self {
        RestartConfig {
            policy: RestartPolicy::default(),
            restart_if: Vec::new(),
            restart_if_not: Vec::new(),
            delay_secs: default_restart_delay(),
            backoff: true,
            max_retries: 0,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        LoggingConfig {
            dir: None,
            basename: None,
            wrapper_log: true,
            mode: default_log_mode(),
            size_threshold_mb: default_size_threshold(),
            time_pattern: default_time_pattern(),
            auto_roll_at: None,
            keep_files: default_keep_files(),
            merge_stderr: false,
        }
    }
}

fn one_day_ms() -> u64 {
    86_400_000
}
fn default_preshutdown_timeout() -> u64 {
    180
}
fn default_stop_timeout() -> u64 {
    15
}
fn default_success_exit_codes() -> Vec<u32> {
    vec![0]
}
fn default_restart_delay() -> u64 {
    5
}
fn default_true() -> bool {
    true
}
fn default_log_mode() -> LogMode {
    LogMode::RollBySize
}
fn default_size_threshold() -> u64 {
    10
}
fn default_time_pattern() -> String {
    "yyyyMMdd".into()
}
fn default_keep_files() -> i64 {
    8
}
fn default_nul() -> String {
    "NUL".into()
}
fn default_hook_timeout() -> u64 {
    60
}
fn default_download_timeout() -> u64 {
    120
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("failed to read config file `{path}`: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid TOML in `{path}`:\n{message}")]
    Toml { path: PathBuf, message: String },
    #[error("invalid YAML in `{path}`:\n{message}")]
    Yaml { path: PathBuf, message: String },
    #[error(
        "unsupported config file extension `{0}` (expected .toml, .yaml, or .yml; note that WinSW XML configs must be converted)"
    )]
    UnsupportedExtension(String),
    #[error(transparent)]
    Validation(#[from] ValidationError),
}

#[derive(Debug, Default, thiserror::Error)]
pub struct ValidationError {
    pub errors: Vec<String>,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "configuration is invalid:")?;
        for e in &self.errors {
            write!(f, "\n  - {e}")?;
        }
        Ok(())
    }
}

impl Config {
    pub fn parse(format: ConfigFormat, text: &str, path: &Path) -> Result<Config, ConfigError> {
        let cfg = match format {
            ConfigFormat::Toml => toml::from_str(text).map_err(|e| ConfigError::Toml {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?,
            ConfigFormat::Yaml => serde_yaml_ng::from_str(text).map_err(|e| ConfigError::Yaml {
                path: path.to_path_buf(),
                message: e.to_string(),
            })?,
        };
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ValidationError> {
        let mut errors = Vec::new();
        let svc = &self.service;
        if svc.id.trim().is_empty() {
            errors.push("[service] id must not be empty".into());
        }
        if svc.id.contains(['/', '\\']) || svc.id.len() > 256 {
            errors.push(
                "[service] id must not contain '/' or '\\' and must be at most 256 chars".into(),
            );
        }
        if let Some(acct) = &svc.account {
            let virtual_account = acct
                .username
                .eq_ignore_ascii_case("NT AUTHORITY\\LocalService")
                || acct
                    .username
                    .eq_ignore_ascii_case("NT AUTHORITY\\NetworkService");
            if acct.password.is_empty() && !virtual_account {
                errors.push(format!(
                    "[service.account] password is required for account `{}` (virtual accounts like NT AUTHORITY\\NetworkService are exempt)",
                    acct.username
                ));
            }
        }
        let proc = &self.process;
        if proc.executable.trim().is_empty() {
            errors.push("[process] executable must not be empty".into());
        }
        if proc.stop_timeout_secs == 0 {
            errors.push("[process] stop_timeout_secs must be > 0".into());
        }
        let r = &proc.restart;
        if r.policy == RestartPolicy::Custom
            && !r.restart_if.is_empty()
            && !r.restart_if_not.is_empty()
        {
            errors.push(
                "[process.restart] restart_if and restart_if_not are mutually exclusive".into(),
            );
        }
        if r.policy == RestartPolicy::Custom
            && r.restart_if.is_empty()
            && r.restart_if_not.is_empty()
        {
            errors.push(
                "[process.restart] policy = \"custom\" requires restart_if or restart_if_not to be set"
                    .into(),
            );
        }
        let log = &self.logging;
        if matches!(log.mode, LogMode::RollBySize | LogMode::RollBySizeTime)
            && log.size_threshold_mb == 0
        {
            errors.push("[logging] size_threshold_mb must be > 0 in size-based modes".into());
        }
        if matches!(log.mode, LogMode::RollByTime | LogMode::RollBySizeTime)
            && translate_time_pattern(&log.time_pattern).is_none()
        {
            errors.push(format!(
                "[logging] time_pattern `{}` uses unsupported tokens (supported: yyyy, MM, dd, HH, mm, ss)",
                log.time_pattern
            ));
        }
        if let Some(at) = &log.auto_roll_at
            && chrono::NaiveTime::parse_from_str(at, "%H:%M:%S").is_err()
        {
            errors.push(format!("[logging] auto_roll_at `{at}` must be HH:MM:SS"));
        }
        if log.keep_files < -1 || log.keep_files == 0 {
            errors.push("[logging] keep_files must be >= 1, or -1 to keep everything".into());
        }
        for (k, v) in &self.env {
            if k.is_empty() || k.contains('=') || k.contains('\0') {
                errors.push(format!("[env] invalid variable name `{k}`"));
            }
            if v.contains('\0') {
                errors.push(format!("[env] invalid value for `{k}`"));
            }
        }
        for d in &self.download {
            if d.timeout_secs == 0 {
                errors.push(format!(
                    "[download] timeout_secs must be > 0 (entry {})",
                    d.from
                ));
            }
            let Some(tls) = &d.tls else { continue };
            if tls.client_cert.is_some() != tls.client_key.is_some() {
                errors.push(
                    "[download] tls.client_cert and tls.client_key must be set together".into(),
                );
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationError { errors })
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigFormat {
    Toml,
    Yaml,
}

impl ConfigFormat {
    pub fn from_path(path: &Path) -> Result<ConfigFormat, ConfigError> {
        match path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref()
        {
            Some("toml") => Ok(ConfigFormat::Toml),
            Some("yaml") | Some("yml") => Ok(ConfigFormat::Yaml),
            Some(other) => Err(ConfigError::UnsupportedExtension(other.to_string())),
            None => Err(ConfigError::UnsupportedExtension(String::new())),
        }
    }
}

/// Finds the config file: explicit path, else the sidecar convention
/// (`rsw.exe` renamed to `app.exe` picks up `app.toml` / `app.yaml` / `app.yml`
/// next to the executable).
pub fn resolve_config_path(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(p) = explicit {
        let p = if p.is_absolute() {
            p.to_path_buf()
        } else {
            env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(p)
        };
        if !p.is_file() {
            anyhow::bail!("config file not found: {}", p.display());
        }
        return Ok(p);
    }
    let exe = env::current_exe().context("cannot locate the rsw executable")?;
    let stem = exe
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("rsw")
        .to_string();
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    find_sidecar(dir, &stem).ok_or_else(|| {
        anyhow::anyhow!(
            "no config file found next to {} (tried {stem}.toml, {stem}.yaml, {stem}.yml);\n\
             pass one explicitly: rsw <command> <path-to-config>",
            exe.display()
        )
    })
}

/// Sidecar lookup: `<stem>.toml` → `<stem>.yaml` → `<stem>.yml` next to the
/// executable. Returns None when no candidate exists.
fn find_sidecar(dir: &Path, stem: &str) -> Option<PathBuf> {
    ["toml", "yaml", "yml"]
        .into_iter()
        .map(|ext| dir.join(format!("{stem}.{ext}")))
        .find(|candidate| candidate.is_file())
}

// ---------------------------------------------------------------------------
// Resolution: variable expansion + path absolutization
// ---------------------------------------------------------------------------

/// A validated config with every relative path resolved against the config
/// directory and `%BASE%` / `%VAR%` expansion applied.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub config_path: PathBuf,
    /// Canonicalized directory containing the config file (expands to %BASE%).
    pub base_dir: PathBuf,
    pub cfg: Config,
}

/// Expands `%VAR%` from the environment and `%BASE%` to the config directory.
/// Unresolvable names are left untouched (matching WinSW behavior).
pub fn expand_vars(input: &str, base_dir: &Path) -> String {
    let base = base_dir.to_string_lossy();
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        rest = &rest[start + 1..];
        match rest.find('%') {
            Some(end) => {
                let name = &rest[..end];
                if name.eq_ignore_ascii_case("BASE") {
                    out.push_str(&base);
                } else if let Ok(v) = env::var(name) {
                    out.push_str(&v);
                } else {
                    out.push('%');
                    out.push_str(name);
                    out.push('%');
                }
                rest = &rest[end + 1..];
            }
            None => {
                out.push('%');
            }
        }
    }
    out.push_str(rest);
    out
}

fn resolve_path_field(p: &str, base_dir: &Path) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        base_dir.join(p)
    }
}

/// Resolves an executable reference at spawn time.
///
/// * Values containing a path separator anchor against `base_dir` (or stay
///   absolute as given).
/// * Bare names prefer a same-named file next to the config; otherwise they
///   are returned unchanged so `CreateProcess` performs the PATH lookup.
pub fn resolve_program(raw: &str, base_dir: &Path) -> String {
    let raw = expand_vars(raw, base_dir);
    let p = Path::new(&raw);
    let has_sep = raw.contains('/') || raw.contains('\\');
    if has_sep || p.is_absolute() {
        if p.is_absolute() {
            return raw;
        }
        return base_dir.join(p).to_string_lossy().into_owned();
    }
    let local = base_dir.join(&raw);
    if local.is_file() {
        return local.to_string_lossy().into_owned();
    }
    raw // bare command name: let CreateProcess search PATH
}

impl Resolved {
    pub fn load(path: &Path) -> Result<Resolved, ConfigError> {
        let format = ConfigFormat::from_path(path)?;
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let cfg = Config::parse(format, &text, path)?;
        cfg.validate()?;
        let base_dir = dunce::canonicalize(path.parent().unwrap_or_else(|| Path::new(".")))
            .unwrap_or_else(|_| {
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .to_path_buf()
            });
        let mut r = Resolved {
            config_path: path.to_path_buf(),
            base_dir,
            cfg,
        };
        r.absolutize();
        Ok(r)
    }

    fn absolutize(&mut self) {
        let base = self.base_dir.clone();
        let process = &mut self.cfg.process;
        // NOTE: `executable` and `stop_executable` are resolved lazily at spawn
        // time by [`resolve_program`] so bare command names (cmd, python, ...)
        // keep working through PATH lookup.
        if let Some(wd) = &process.working_dir {
            process.working_dir = Some(
                resolve_path_field(&expand_vars(wd, &base), &base)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        // WinSW expands %VAR% inside every argument (XmlServiceConfig reads all
        // elements through ExpandEnvironmentVariables) — match that.
        for a in &mut process.arguments {
            *a = expand_vars(a, &base);
        }
        for a in &mut process.start_arguments {
            *a = expand_vars(a, &base);
        }
        for a in &mut process.stop_arguments {
            *a = expand_vars(a, &base);
        }

        let logging = &mut self.cfg.logging;
        if let Some(dir) = &logging.dir {
            logging.dir = Some(
                resolve_path_field(&expand_vars(dir, &base), &base)
                    .to_string_lossy()
                    .into_owned(),
            );
        }
        if let Some(basename) = &logging.basename {
            logging.basename = Some(expand_vars(basename, &base));
        }

        for d in &mut self.cfg.download {
            d.to = resolve_path_field(&expand_vars(&d.to, &base), &base)
                .to_string_lossy()
                .into_owned();
            d.from = expand_vars(&d.from, &base);
            if let Some(tls) = &mut d.tls {
                for field in [
                    tls.ca.as_mut(),
                    tls.client_cert.as_mut(),
                    tls.client_key.as_mut(),
                ]
                .into_iter()
                .flatten()
                {
                    *field = resolve_path_field(&expand_vars(field, &base), &base)
                        .to_string_lossy()
                        .into_owned();
                }
            }
        }

        let hooks = &mut self.cfg.hooks;
        let hook_list = [
            &mut hooks.pre_start,
            &mut hooks.post_start,
            &mut hooks.pre_stop,
            &mut hooks.post_stop,
        ];
        for h in hook_list.into_iter().flatten() {
            for a in &mut h.arguments {
                *a = expand_vars(a, &base);
            }
            for out in [&mut h.stdout, &mut h.stderr] {
                if !out.eq_ignore_ascii_case("NUL") {
                    *out = resolve_path_field(&expand_vars(out, &base), &base)
                        .to_string_lossy()
                        .into_owned();
                }
            }
        }

        if let Some(sddl) = &mut self.cfg.service.security_descriptor {
            *sddl = expand_vars(sddl, &base);
        }
        // Fleet deployments inject the service-account password from the
        // environment (e.g. password = "%DSV_SVC_PASS%") so it never lands
        // in the config file.
        if let Some(acct) = &mut self.cfg.service.account {
            acct.username = expand_vars(&acct.username, &base);
            acct.password = expand_vars(&acct.password, &base);
        }
        for v in self.cfg.env.values_mut() {
            *v = expand_vars(v, &base);
        }
        for d in &mut self.cfg.service.dependencies {
            *d = expand_vars(d, &base);
        }
        for m in &mut self.cfg.map_drive {
            m.unc_path = expand_vars(&m.unc_path, &base);
        }
    }

    // -- convenience accessors -----------------------------------------------

    pub fn service_id(&self) -> &str {
        &self.cfg.service.id
    }

    pub fn display_name(&self) -> &str {
        self.cfg
            .service
            .name
            .as_deref()
            .unwrap_or(&self.cfg.service.id)
    }

    pub fn working_dir(&self) -> &Path {
        self.cfg
            .process
            .working_dir
            .as_deref()
            .map(Path::new)
            .unwrap_or(&self.base_dir)
    }

    /// Arguments used at start: `start_arguments` when set, else `arguments`.
    pub fn start_args(&self) -> &[String] {
        let p = &self.cfg.process;
        if p.start_arguments.is_empty() {
            &p.arguments
        } else {
            &p.start_arguments
        }
    }

    pub fn log_dir(&self) -> &Path {
        self.cfg
            .logging
            .dir
            .as_deref()
            .map(Path::new)
            .unwrap_or(&self.base_dir)
    }

    pub fn log_basename(&self) -> String {
        self.cfg.logging.basename.clone().unwrap_or_else(|| {
            self.config_path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
    }

    /// Environment for the child: inherited vars + configured `[env]` +
    /// service identity vars (WinSW-compatible names included, so scripts
    /// migrated from WinSW keep working).
    pub fn child_env(&self) -> BTreeMap<String, String> {
        let mut vars: BTreeMap<String, String> = env::vars().collect();
        for (k, v) in &self.cfg.env {
            vars.insert(k.clone(), v.clone());
        }
        let id = self.cfg.service.id.clone();
        vars.insert("BASE".into(), self.base_dir.to_string_lossy().into_owned());
        vars.insert("SERVICE_ID".into(), id.clone());
        vars.insert("RSW_SERVICE_ID".into(), id.clone());
        vars.insert("WINSW_SERVICE_ID".into(), id);
        if let Ok(exe) = env::current_exe() {
            let exe = exe.to_string_lossy().into_owned();
            vars.insert("RSW_EXECUTABLE".into(), exe.clone());
            vars.insert("WINSW_EXECUTABLE".into(), exe);
        }
        vars
    }
}

/// Masks the password in `scheme://user:pass@host` URLs (proxy settings);
/// anything else is returned unchanged.
pub fn redact_url_password(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    // rsplit: an unescaped '@' in the password would otherwise split early
    // and leak the password's tail into the "host" part.
    let Some((userinfo, host)) = rest.rsplit_once('@') else {
        return url.to_string();
    };
    match userinfo.split_once(':') {
        Some((user, _pass)) => format!("{scheme}://{user}:********@{host}"),
        None => url.to_string(),
    }
}

/// Translates a .NET-style DateTime pattern (yyyy, MM, dd, HH, mm, ss tokens)
/// into a chrono format string. Returns None when unsupported tokens appear.
pub fn translate_time_pattern(pattern: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = pattern.chars().peekable();
    while let Some(&c) = chars.peek() {
        let run: String = {
            let mut r = String::new();
            while chars.peek() == Some(&c) {
                r.push(c);
                chars.next();
            }
            r
        };
        let len = run.chars().count();
        let piece: &str = match (c, len) {
            ('y', 4) => "%Y",
            ('y', 2) => "%y",
            ('M', 2) => "%m",
            ('d', 2) => "%d",
            ('H', 2) => "%H",
            ('m', 2) => "%M",
            ('s', 2) => "%S",
            (ch, _) if ch.is_ascii_alphanumeric() => return None,
            _ => run.as_str(),
        };
        out.push_str(piece);
    }
    Some(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_suffixes() {
        assert_eq!(parse_duration_ms("0"), Ok(0));
        assert_eq!(parse_duration_ms("500"), Ok(500));
        assert_eq!(parse_duration_ms("500 ms"), Ok(500));
        assert_eq!(parse_duration_ms("5 sec"), Ok(5_000));
        assert_eq!(parse_duration_ms("10 SECS"), Ok(10_000));
        assert_eq!(parse_duration_ms("1 min"), Ok(60_000));
        assert_eq!(parse_duration_ms("2 mins"), Ok(120_000));
        assert_eq!(parse_duration_ms("1 hr"), Ok(3_600_000));
        assert_eq!(parse_duration_ms("1 hour"), Ok(3_600_000));
        assert_eq!(parse_duration_ms("1 day"), Ok(86_400_000));
        assert!(parse_duration_ms("2 days ").is_ok());
        assert!(parse_duration_ms("").is_err());
        assert!(parse_duration_ms("abc").is_err());
        assert!(parse_duration_ms("5 fortnights").is_err());
    }

    #[test]
    fn toml_minimal_and_defaults() {
        let text = r#"
[service]
id = "myapp"

[process]
executable = "app.exe"
"#;
        let cfg: Config = Config::parse(ConfigFormat::Toml, text, Path::new("app.toml")).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.service.id, "myapp");
        assert_eq!(cfg.process.stop_timeout_secs, 15);
        assert_eq!(cfg.process.stop_signal, StopSignal::Kill);
        assert!(cfg.process.hide_window);
        assert_eq!(cfg.process.priority, Priority::Normal);
        assert_eq!(cfg.process.success_exit_codes, vec![0]);
        assert_eq!(cfg.process.restart.policy, RestartPolicy::Never);
        assert_eq!(cfg.process.restart.max_retries, 0);
        assert_eq!(cfg.process.restart.delay_secs, 5);
        assert!(cfg.process.restart.backoff);
        assert_eq!(cfg.logging.mode, LogMode::RollBySize);
        assert_eq!(cfg.logging.size_threshold_mb, 10);
        assert_eq!(cfg.logging.keep_files, 8);
        assert_eq!(cfg.service.failure_reset_after, 86_400_000);
        assert_eq!(cfg.on_failure.len(), 0);
    }

    #[test]
    fn yaml_same_schema() {
        let text = r#"
service:
  id: myapp
  start_type: delayed
  dependencies: [Redis]
process:
  executable: app.exe
  arguments: [--port, "8080"]
  restart:
    policy: always
on_failure:
  - action: restart
    delay: 5 sec
"#;
        let cfg: Config = Config::parse(ConfigFormat::Yaml, text, Path::new("app.yaml")).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.service.start_type, StartType::Delayed);
        assert_eq!(cfg.service.dependencies, vec!["Redis"]);
        assert_eq!(cfg.process.restart.policy, RestartPolicy::Always);
        assert_eq!(cfg.on_failure[0].action, FailureAction::Restart);
        assert_eq!(cfg.on_failure[0].delay, 5_000);
    }

    #[test]
    fn download_tls_parsing_and_pair_validation() {
        let base = r#"
[service]
id = "myapp"

[process]
executable = "app.exe"

[[download]]
from = "https://example.com/app.jar"
to = "app.jar"
"#;
        let full = format!(
            "{base}\n[download.tls]\nca = \"root.pem\"\nclient_cert = \"client.pem\"\nclient_key = \"client-key.pem\"\n"
        );
        let cfg: Config = Config::parse(ConfigFormat::Toml, &full, Path::new("app.toml")).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.download[0].timeout_secs, 120);
        let tls = cfg.download[0].tls.as_ref().unwrap();
        assert_eq!(tls.ca.as_deref(), Some("root.pem"));
        assert_eq!(tls.client_cert.as_deref(), Some("client.pem"));
        assert_eq!(tls.client_key.as_deref(), Some("client-key.pem"));

        for incomplete in [
            format!("{base}\n[download.tls]\nclient_cert = \"client.pem\"\n"),
            format!("{base}\n[download.tls]\nclient_key = \"client-key.pem\"\n"),
        ] {
            let cfg: Config =
                Config::parse(ConfigFormat::Toml, &incomplete, Path::new("app.toml")).unwrap();
            let err = cfg.validate().unwrap_err().to_string();
            assert!(
                err.contains("client_cert and tls.client_key must be set together"),
                "unexpected error: {err}"
            );
        }
    }

    #[test]
    fn download_tls_unknown_field_rejected() {
        let text = r#"
[service]
id = "myapp"

[process]
executable = "app.exe"

[[download]]
from = "https://example.com/app.jar"
to = "app.jar"

[download.tls]
insecure = true
"#;
        let err = Config::parse(ConfigFormat::Toml, text, Path::new("app.toml"))
            .expect_err("deny_unknown_fields must reject tls.insecure");
        assert!(
            err.to_string().contains("insecure"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn unknown_field_is_rejected() {
        let text = r#"
[service]
id = "myapp"
bogus = 1

[process]
executable = "app.exe"
"#;
        let err = Config::parse(ConfigFormat::Toml, text, Path::new("a.toml")).unwrap_err();
        assert!(
            err.to_string().contains("bogus"),
            "error should mention the field: {err}"
        );
    }

    #[test]
    fn validation_catches_problems() {
        let base = r#"
[service]
id = "x"
[process]
executable = "e"
"#;
        // missing password for a real account
        let cfg: Config =
            toml::from_str(&format!("{base}\n[service.account]\nusername = '.\\\\u'\n")).unwrap();
        let err = cfg.validate().unwrap_err();
        assert!(err.errors.iter().any(|e| e.contains("password")));

        // bad auto_roll_at
        let cfg: Config =
            toml::from_str(&format!("{base}\n[logging]\nauto_roll_at = \"9am\"\n")).unwrap();
        assert!(
            cfg.validate()
                .unwrap_err()
                .errors
                .iter()
                .any(|e| e.contains("auto_roll_at"))
        );

        // custom policy with both lists
        let cfg: Config = toml::from_str(
            &format!("{base}\n[process.restart]\npolicy = \"custom\"\nrestart_if = [1]\nrestart_if_not = [2]\n"),
        )
        .unwrap();
        assert!(
            cfg.validate()
                .unwrap_err()
                .errors
                .iter()
                .any(|e| e.contains("mutually exclusive"))
        );

        // custom policy with neither list
        let cfg: Config =
            toml::from_str(&format!("{base}\n[process.restart]\npolicy = \"custom\"\n")).unwrap();
        assert!(
            cfg.validate()
                .unwrap_err()
                .errors
                .iter()
                .any(|e| e.contains("requires restart_if"))
        );

        // empty id
        let cfg: Config =
            toml::from_str("[service]\nid = \"\"\n[process]\nexecutable = \"e\"\n").unwrap();
        assert!(
            cfg.validate()
                .unwrap_err()
                .errors
                .iter()
                .any(|e| e.contains("id"))
        );
    }

    #[test]
    fn expansion() {
        unsafe { std::env::set_var("RSW_TEST_VAR", "hello") };
        assert_eq!(
            expand_vars("%RSW_TEST_VAR% world", Path::new("C:/b")),
            "hello world"
        );
        assert_eq!(expand_vars("%BASE%/logs", Path::new("C:/b")), "C:/b/logs");
        assert_eq!(
            expand_vars("%UNKNOWN_XYZ% stays", Path::new("C:/b")),
            "%UNKNOWN_XYZ% stays"
        );
        assert_eq!(expand_vars("no vars", Path::new("C:/b")), "no vars");
        assert_eq!(expand_vars("50% done", Path::new("C:/b")), "50% done");
    }

    #[test]
    fn url_password_redaction() {
        assert_eq!(
            redact_url_password("http://user:s3cret@proxy:8080"),
            "http://user:********@proxy:8080"
        );
        // an unescaped '@' inside the password must not leak its tail
        assert_eq!(
            redact_url_password("http://user:p@ss@proxy:8080"),
            "http://user:********@proxy:8080"
        );
        assert_eq!(
            redact_url_password("http://user@proxy:8080"),
            "http://user@proxy:8080"
        );
        assert_eq!(
            redact_url_password("http://proxy:8080"),
            "http://proxy:8080"
        );
        assert_eq!(redact_url_password("not a url"), "not a url");
    }

    #[test]
    fn time_pattern_translation() {
        assert_eq!(
            translate_time_pattern("yyyyMMdd").as_deref(),
            Some("%Y%m%d")
        );
        assert_eq!(
            translate_time_pattern("yyyyMMddHHmmss").as_deref(),
            Some("%Y%m%d%H%M%S")
        );
        assert_eq!(translate_time_pattern("yy_MM").as_deref(), Some("%y_%m"));
        assert!(translate_time_pattern("dddd").is_none()); // day-name token unsupported
    }

    #[test]
    fn format_detection() {
        assert_eq!(
            ConfigFormat::from_path(Path::new("a/b.TOML")).unwrap(),
            ConfigFormat::Toml
        );
        assert_eq!(
            ConfigFormat::from_path(Path::new("a/b.Yml")).unwrap(),
            ConfigFormat::Yaml
        );
        assert!(matches!(
            ConfigFormat::from_path(Path::new("a/b.xml")).unwrap_err(),
            ConfigError::UnsupportedExtension(_)
        ));
    }

    #[test]
    fn resolved_paths_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("app.toml");
        std::fs::write(
            &cfg_path,
            r#"
[service]
id = "myapp"
[process]
executable = "bin/app.exe"
working_dir = "data"
[logging]
dir = "logs"
[env]
DATABASE_URL = "%BASE%/db.sqlite"
"#,
        )
        .unwrap();
        let r = Resolved::load(&cfg_path).unwrap();
        assert_eq!(r.base_dir, dunce::canonicalize(dir.path()).unwrap());
        // `executable` is resolved lazily at spawn time.
        assert_eq!(r.cfg.process.executable, "bin/app.exe");
        let resolved = resolve_program(&r.cfg.process.executable, &r.base_dir).replace('\\', "/");
        assert!(resolved.ends_with("/bin/app.exe"), "got {resolved}");
        // Bare names prefer a local file when it exists, else fall back to PATH.
        assert_eq!(
            resolve_program("definitely-not-here.exe", &r.base_dir),
            "definitely-not-here.exe"
        );
        std::fs::write(dir.path().join("local-tool.exe"), b"").unwrap();
        assert!(resolve_program("local-tool.exe", &r.base_dir).ends_with("local-tool.exe"));
        assert_eq!(resolve_program("cmd", &r.base_dir), "cmd");
        assert!(
            r.cfg
                .logging
                .dir
                .as_deref()
                .unwrap()
                .replace('\\', "/")
                .ends_with("/logs")
        );
        assert_eq!(
            r.cfg.env["DATABASE_URL"],
            format!("{}/db.sqlite", r.base_dir.to_string_lossy())
        );
        assert_eq!(
            r.child_env().get("SERVICE_ID").map(String::as_str),
            Some("myapp")
        );
        assert_eq!(r.log_basename(), "app");
    }

    #[test]
    fn sidecar_resolution_priority() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(find_sidecar(dir.path(), "app"), None);
        std::fs::write(dir.path().join("app.yml"), "").unwrap();
        assert_eq!(
            find_sidecar(dir.path(), "app"),
            Some(dir.path().join("app.yml"))
        );
        std::fs::write(dir.path().join("app.yaml"), "").unwrap();
        assert_eq!(
            find_sidecar(dir.path(), "app"),
            Some(dir.path().join("app.yaml"))
        );
        std::fs::write(dir.path().join("app.toml"), "").unwrap();
        assert_eq!(
            find_sidecar(dir.path(), "app"),
            Some(dir.path().join("app.toml"))
        );
        // a matching directory does not count as a config file
        let sub = dir.path().join("app.toml");
        std::fs::remove_file(&sub).unwrap();
        std::fs::create_dir(&sub).unwrap();
        assert_eq!(
            find_sidecar(dir.path(), "app"),
            Some(dir.path().join("app.yaml"))
        );
    }

    #[test]
    fn winsw_compat_expansion_and_env() {
        let dir = tempfile::tempdir().unwrap();
        let cfg_path = dir.path().join("app.toml");
        std::fs::write(
            &cfg_path,
            r#"
[service]
id = "myapp"
[process]
executable = "app.exe"
arguments = ["-c", "%BASE%/app.conf"]
stop_arguments = ["%BASE%/stop.conf"]
[hooks]
pre_start = { executable = "prep.cmd", arguments = ["--root", "%BASE%"] }
"#,
        )
        .unwrap();
        let r = Resolved::load(&cfg_path).unwrap();
        let base = r.base_dir.to_string_lossy().into_owned();
        // WinSW expands %VAR% inside every argument — rsw must too.
        assert_eq!(
            r.cfg.process.arguments,
            vec!["-c".to_string(), format!("{base}/app.conf")]
        );
        assert_eq!(
            r.cfg.process.stop_arguments,
            vec![format!("{base}/stop.conf")]
        );
        assert_eq!(
            r.cfg.hooks.pre_start.as_ref().unwrap().arguments,
            vec!["--root".to_string(), base.clone()]
        );
        // Children inherit the service identity vars, including the WinSW names.
        let env = r.child_env();
        assert_eq!(env["BASE"], base);
        assert_eq!(env["WINSW_SERVICE_ID"], "myapp");
        unsafe { std::env::set_var("RSW_ACCT_PASS", "s3cret") };
        let cfg2_path = dir.path().join("acct.toml");
        std::fs::write(
            &cfg2_path,
            r#"
[service]
id = "acct"
[service.account]
username = '.\svc_user'
password = "%RSW_ACCT_PASS%"
[process]
executable = "app.exe"
"#,
        )
        .unwrap();
        let r2 = Resolved::load(&cfg2_path).unwrap();
        assert_eq!(r2.cfg.service.account.as_ref().unwrap().password, "s3cret");
        assert_eq!(env["RSW_SERVICE_ID"], "myapp");
        assert!(env.contains_key("WINSW_EXECUTABLE"));
    }
}
