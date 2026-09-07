#![allow(deprecated)] // quick-xml 0.42 deprecates unescape_value in favor of a private-enum API
//! WinSW XML → rsw TOML conversion (`rsw convert`).
//!
//! Parses a WinSW service definition (both v2 and v3 element shapes, element
//! and attribute names matched case-insensitively) and emits an equivalent
//! rsw TOML file. Anything rsw intentionally does not support (zip log
//! archiving, log `period` multipliers, SSPI download auth, ...) becomes a
//! migration note embedded as a comment in the output and printed to stderr.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use quick_xml::Reader;
use quick_xml::events::Event;

/// One top-level `<service>` child collected from the XML.
#[derive(Debug, Default)]
struct WinswConfig {
    elements: BTreeMap<String, String>, // lowercase name -> inner text
    /// Repeated text elements (e.g. multiple <depend>) in document order.
    repeated: Vec<(String, String)>,
    /// Repeatable attribute elements: env/onfailure/download (lowercase name).
    attr_elements: Vec<(String, BTreeMap<String, String>)>,
    /// `<log>` element: attributes plus child text elements.
    log_attrs: BTreeMap<String, String>,
    log_children: BTreeMap<String, String>,
    has_log: bool,
}

impl WinswConfig {
    fn text(&self, name: &str) -> Option<&str> {
        self.elements
            .get(name)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
    }

    fn text_lower(&self, name: &str) -> Option<String> {
        self.text(name).map(|s| s.to_ascii_lowercase())
    }

    fn log_child(&self, name: &str) -> Option<&str> {
        self.log_children
            .get(name)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
    }
}

fn parse_winsw_xml(path: &Path) -> anyhow::Result<WinswConfig> {
    let mut reader =
        Reader::from_file(path).with_context(|| format!("reading {}", path.display()))?;
    reader.config_mut().trim_text(true);

    let mut cfg = WinswConfig::default();
    let mut buf = Vec::new();
    let mut stack: Vec<String> = Vec::new(); // lowercase element-name stack
    let mut current_text: Option<(String, String)> = None; // (lowercase name, accumulated text)
    let mut in_log_child: Option<String> = None;

    loop {
        buf.clear();
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(e)) => {
                let name = e
                    .name()
                    .into_inner()
                    .to_ascii_lowercase()
                    .to_ascii_lowercase();
                match name.as_str() {
                    "env" | "onfailure" | "download" | "map" | "prestart" | "poststart"
                    | "prestop" | "poststop" => {
                        let mut attrs = BTreeMap::new();
                        for a in e.attributes().flatten() {
                            let key = a.key.into_inner().to_ascii_lowercase();
                            if let Ok(v) = a.unescape_value() {
                                attrs.insert(key, v.into_owned());
                            }
                        }
                        cfg.attr_elements.push((name.clone(), attrs));
                        stack.push(name); // popped at End; Empty events skip this
                    }
                    "log" => {
                        cfg.has_log = true;
                        for a in e.attributes().flatten() {
                            let key = a.key.into_inner().to_ascii_lowercase();
                            if let Ok(v) = a.unescape_value() {
                                cfg.log_attrs.insert(key, v.into_owned());
                            }
                        }
                        stack.push(name);
                    }
                    "serviceaccount" => {
                        for a in e.attributes().flatten() {
                            let key = a.key.into_inner().to_ascii_lowercase();
                            if let Ok(v) = a.unescape_value() {
                                cfg.log_attrs.insert(format!("sa_{key}"), v.into_owned());
                            }
                        }
                        stack.push(name);
                    }
                    _ => {
                        if let Some(parent) = stack.last()
                            && parent == "log"
                        {
                            in_log_child = Some(name.clone());
                        }
                        current_text = Some((name.clone(), String::new()));
                        stack.push(name);
                    }
                }
            }
            Ok(Event::Empty(e)) => {
                let name = e
                    .name()
                    .into_inner()
                    .to_ascii_lowercase()
                    .to_ascii_lowercase();
                match name.as_str() {
                    "env" | "onfailure" | "download" | "map" | "prestart" | "poststart"
                    | "prestop" | "poststop" => {
                        let mut attrs = BTreeMap::new();
                        for a in e.attributes().flatten() {
                            let key = a.key.into_inner().to_ascii_lowercase();
                            if let Ok(v) = a.unescape_value() {
                                attrs.insert(key, v.into_owned());
                            }
                        }
                        cfg.attr_elements.push((name, attrs));
                    }
                    "log" => {
                        cfg.has_log = true;
                        for a in e.attributes().flatten() {
                            let key = a.key.into_inner().to_ascii_lowercase();
                            if let Ok(v) = a.unescape_value() {
                                cfg.log_attrs.insert(key, v.into_owned());
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(t)) => {
                let text = t.xml10_content().into_owned();
                if !text.is_empty() {
                    if let Some((_, acc)) = &mut current_text {
                        acc.push_str(&text);
                    } else if let Some(child) = &in_log_child {
                        cfg.log_children
                            .entry(child.clone())
                            .or_default()
                            .push_str(&text);
                    }
                }
            }
            Ok(Event::End(e)) => {
                let name = e
                    .name()
                    .into_inner()
                    .to_ascii_lowercase()
                    .to_ascii_lowercase();
                in_log_child = None;
                if stack.last().map(|s| s.as_str()) == Some(name.as_str()) {
                    stack.pop();
                    if let Some((tname, text)) = current_text.take()
                        && tname == name
                        && !stack.is_empty()
                    {
                        // Nested text elements are grouped by their
                        // container (<log> settings, <serviceaccount>
                        // fields); <depend> keeps every occurrence;
                        // the rest are single service fields.
                        match stack.last().map(|s| s.as_str()) {
                            Some("log") => {
                                cfg.log_children.insert(tname, text);
                            }
                            Some("serviceaccount") => {
                                cfg.log_attrs.insert(format!("sa_{tname}"), text);
                            }
                            _ if tname == "depend" => {
                                cfg.repeated.push((tname, text));
                            }
                            _ => {
                                cfg.elements.insert(tname, text);
                            }
                        }
                    }
                } else {
                    // mismatched nesting guard; keep stack consistent
                    stack.pop();
                }
                if name == "service" && stack.is_empty() {
                    break;
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => bail!("XML parse error in {}: {e}", path.display()),
            _ => {}
        }
    }

    if cfg.text("id").is_none() {
        bail!(
            "{}: not a WinSW service definition (no <id> element)",
            path.display()
        );
    }
    Ok(cfg)
}

fn toml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", toml_escape(s))
}

fn toml_array(items: &[String]) -> String {
    let list: Vec<String> = items.iter().map(|s| toml_string(s)).collect();
    format!("[{}]", list.join(", "))
}

struct Emitter {
    text: String,
    warnings: Vec<String>,
}

impl Emitter {
    fn new() -> Emitter {
        Emitter {
            text: String::new(),
            warnings: Vec::new(),
        }
    }

    fn warn(&mut self, note: &str) {
        self.warnings.push(note.to_string());
    }

    fn comment(&mut self, line: &str) {
        self.text.push_str("# ");
        self.text.push_str(line);
        self.text.push('\n');
    }

    fn line(&mut self, line: &str) {
        self.text.push_str(line);
        self.text.push('\n');
    }
}

/// Generates the rsw TOML text from the parsed WinSW definition.
fn generate(w: &WinswConfig, source: &Path) -> String {
    let mut e = Emitter::new();
    e.comment(&format!(
        "rsw service definition — converted from WinSW {}",
        source.display()
    ));
    e.comment("%BASE% refers to this file's directory, matching WinSW semantics.");

    // --- [service] ---------------------------------------------------------
    e.line("");
    e.line("[service]");
    e.line(&format!(
        "id = {}",
        toml_string(w.text("id").unwrap_or_default())
    ));
    if let Some(name) = w.text("name") {
        e.line(&format!("name = {}", toml_string(name)));
    }
    if let Some(d) = w.text("description") {
        e.line(&format!("description = {}", toml_string(d)));
    }

    // startmode + delayedAutoStart
    let startmode = w
        .text_lower("startmode")
        .unwrap_or_else(|| "automatic".into());
    let delayed = w
        .text_lower("delayedautostart")
        .map(|v| v == "true")
        .unwrap_or(false);
    let start_type = match startmode.as_str() {
        "automatic" if delayed => "delayed".to_string(),
        "automatic" => "auto".to_string(),
        "manual" => "manual".to_string(),
        "disabled" => "disabled".to_string(),
        "boot" => "boot".to_string(),
        "system" => "system".to_string(),
        other => {
            e.warn(&format!("unknown startmode `{other}`; using auto"));
            "auto".to_string()
        }
    };
    e.line(&format!("start_type = \"{start_type}\""));

    // depend: repeated <depend>Name</depend>
    let deps_text: Vec<String> = w
        .repeated
        .iter()
        .filter(|(n, _)| n == "depend")
        .map(|(_, v)| v.clone())
        .collect();
    if !deps_text.is_empty() {
        e.line(&format!("dependencies = {}", toml_array(&deps_text)));
    }

    if let Some(rf) = w.text("resetfailure") {
        e.line(&format!("failure_reset_after = {}", toml_string(rf)));
    }

    if let Some(v) = w.text_lower("preshutdown")
        && v == "true"
    {
        e.line("preshutdown = true");
    }
    if let Some(t) = w.text("preshutdownTimeout") {
        e.line(&format!(
            "preshutdown_timeout_secs = {}",
            parse_duration_secs(t)
        ));
    }

    // securityDescriptor (v3 element name; also accept camelCase collapse)
    if let Some(sd) = w.text("securitydescriptor") {
        e.line(&format!("security_descriptor = {}", toml_string(sd)));
    }

    // --- [service.account] -------------------------------------------------
    let sa_user = w.log_attrs.get("sa_username").cloned();
    if let Some(user) = sa_user {
        e.line("");
        e.line("[service.account]");
        e.line(&format!("username = {}", toml_string(&user)));
        if let Some(pw) = w.log_attrs.get("sa_password").filter(|p| !p.is_empty()) {
            e.warn("serviceaccount password was copied verbatim into the TOML — consider setting the account in services.msc instead (passwords in files are plaintext).");
            e.line(&format!("password = {}", toml_string(pw)));
        }
        e.line("allow_logon_as_service = true");
    }

    // --- [process] ---------------------------------------------------------
    e.line("");
    e.line("[process]");
    let exe = w.text("executable").unwrap_or_default();
    if exe.contains('\\') || exe.contains('/') {
        e.line(&format!("executable = {}", toml_string(exe)));
    } else {
        e.line(&format!(
            "executable = {}   # bare name; consider an absolute path (service PATH != user PATH)",
            toml_string(exe)
        ));
    }

    if let Some(raw) = w.text("arguments") {
        let args = crate::binpath::parse(raw);
        e.line(&format!("arguments = {}", toml_array(&args)));
    }
    if let Some(raw) = w.text("startarguments") {
        let args = crate::binpath::parse(raw);
        e.line(&format!("start_arguments = {}", toml_array(&args)));
    }
    if let Some(wd) = w.text("workingdirectory") {
        // WinSW uses backslashes; rsw TOML prefers forward slashes.
        e.line(&format!(
            "working_dir = {}",
            toml_string(&wd.replace('\\', "/"))
        ));
    }
    if let Some(se) = w.text("stopexecutable") {
        e.line(&format!("stop_executable = {}", toml_string(se)));
    }
    if let Some(raw) = w.text("stoparguments") {
        let args = crate::binpath::parse(raw);
        e.line(&format!("stop_arguments = {}", toml_array(&args)));
    }
    if let Some(t) = w.text("stoptimeout") {
        e.line(&format!("stop_timeout_secs = {}", parse_duration_secs(t)));
    }
    if let Some(p) = w.text_lower("priority") {
        let mapped = match p.as_str() {
            "normal" | "idle" | "high" | "realtime" => p.clone(),
            "belownormal" => "below-normal".to_string(),
            "abovenormal" => "above-normal".to_string(),
            other => {
                e.warn(&format!("unknown priority `{other}`; using normal"));
                "normal".to_string()
            }
        };
        if mapped != "normal" {
            e.line(&format!("priority = \"{mapped}\""));
        }
    }
    if let Some(v) = w.text_lower("hidewindow")
        && v == "true"
    {
        e.line("hide_window = true");
    }

    // --- [env] -------------------------------------------------------------
    let envs: Vec<&BTreeMap<String, String>> = w
        .attr_elements
        .iter()
        .filter(|(n, _)| n == "env")
        .map(|(_, a)| a)
        .collect();
    if !envs.is_empty() {
        e.line("");
        e.line("[env]");
        for a in &envs {
            if let (Some(k), Some(v)) = (a.get("name"), a.get("value")) {
                let bare = !k.is_empty()
                    && k.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
                let key = if bare { k.clone() } else { toml_string(k) };
                e.line(&format!("{key} = {}", toml_string(v)));
            }
        }
    }

    // --- [hooks] (WinSW v3 prestart/poststart/prestop/poststop) -------------
    const HOOK_NAMES: [(&str, &str); 4] = [
        ("prestart", "pre_start"),
        ("poststart", "post_start"),
        ("prestop", "pre_stop"),
        ("poststop", "post_stop"),
    ];
    let hook_elems: Vec<(&str, &BTreeMap<String, String>)> = w
        .attr_elements
        .iter()
        .filter_map(|(n, a)| {
            HOOK_NAMES
                .iter()
                .find(|(xml, _)| xml == n)
                .map(|(_, rust)| (*rust, a))
        })
        .collect();
    if !hook_elems.is_empty() {
        e.line("");
        e.line("[hooks]");
        for (rust_name, a) in &hook_elems {
            let exe = a.get("executable").cloned().unwrap_or_default();
            if exe.is_empty() {
                continue;
            }
            e.line("");
            e.line(&format!(
                "{rust_name} = {{ executable = {}, arguments = {}, stdout = {}, stderr = {} }}",
                toml_string(&exe),
                a.get("arguments")
                    .map(|raw| toml_array(&crate::binpath::parse(raw)))
                    .unwrap_or_else(|| "[]".into()),
                toml_string(a.get("stdoutpath").map(|s| s.as_str()).unwrap_or("NUL")),
                toml_string(a.get("stderrpath").map(|s| s.as_str()).unwrap_or("NUL")),
            ));
        }
    }

    // --- [[on_failure]] ----------------------------------------------------
    let failures: Vec<&BTreeMap<String, String>> = w
        .attr_elements
        .iter()
        .filter(|(n, _)| n == "onfailure")
        .map(|(_, a)| a)
        .collect();
    for a in failures {
        e.line("");
        e.line("[[on_failure]]");
        e.line(&format!(
            "action = \"{}\"",
            a.get("action").cloned().unwrap_or_else(|| "none".into())
        ));
        e.line(&format!(
            "delay = {}",
            toml_string(&a.get("delay").cloned().unwrap_or_else(|| "0".into()))
        ));
    }

    // --- [logging] ---------------------------------------------------------
    let logpath = w.text("logpath");
    let legacy_mode = w.text_lower("logmode");
    let mode = w
        .log_attrs
        .get("mode")
        .cloned()
        .or_else(|| legacy_mode.clone());
    if logpath.is_some() || mode.is_some() {
        e.line("");
        e.line("[logging]");
        if let Some(lp) = logpath {
            e.line(&format!("dir = {}", toml_string(&lp.replace('\\', "/"))));
        }
        let mode = mode.as_deref().unwrap_or("append").to_ascii_lowercase();
        let mode = match mode.as_str() {
            "rotate" => "roll-by-size".to_string(), // WinSW legacy alias
            other => other.to_string(),
        };
        match mode.as_str() {
            "append" | "reset" | "none" | "roll" | "roll-by-size" | "roll-by-time"
            | "roll-by-size-time" => {
                if mode != "append" {
                    e.line(&format!("mode = \"{mode}\""));
                } else {
                    e.line("mode = \"append\"");
                }
            }
            other => {
                e.warn(&format!("unknown log mode `{other}`; using append"));
                e.line("mode = \"append\"");
            }
        }
        if let Some(kb) = w.log_child("sizethreshold")
            && let Ok(kb) = kb.parse::<u64>()
        {
            e.line(&format!("size_threshold_mb = {}", (kb / 1024).max(1)));
        }
        if let Some(p) = w.log_child("pattern") {
            e.line(&format!("time_pattern = {}", toml_string(p)));
        }
        if let Some(at) = w.log_child("autorollattime") {
            e.line(&format!("auto_roll_at = {}", toml_string(at)));
        }
        if let Some(kf) = w.log_child("keepfiles") {
            e.line(&format!("keep_files = {kf}"));
        } else if let Some(days) = w
            .log_child("zipolderthannumdays")
            .and_then(|v| v.parse::<i64>().ok())
        {
            // Approximate zip-based retention with file-count retention.
            e.line(&format!(
                "keep_files = {days}   # approximates zipOlderThanNumDays={days}"
            ));
        }
        if w.log_child("zipolderthannumdays").is_some() || w.log_child("zipdateformat").is_some() {
            e.warn("zipOlderThanNumDays/zipDateFormat have no equivalent: rsw does not zip logs (the WinSW feature is officially broken). Retention is approximated with keep_files.");
        }
        if let Some(period) = w.log_child("period") {
            e.warn(&format!(
                "roll-by-time `period` multiplier ({period}) has no equivalent in rsw; the pattern defines the rollover period."
            ));
        }
    }

    // --- [[download]] ------------------------------------------------------
    let downloads: Vec<&BTreeMap<String, String>> = w
        .attr_elements
        .iter()
        .filter(|(n, _)| n == "download")
        .map(|(_, a)| a)
        .collect();
    for d in downloads {
        e.line("");
        e.line("[[download]]");
        e.line(&format!(
            "from = {}",
            toml_string(&d.get("from").cloned().unwrap_or_default())
        ));
        e.line(&format!(
            "to = {}",
            toml_string(&d.get("to").cloned().unwrap_or_default())
        ));
        if let Some(v) = d.get("failonerror")
            && v == "true"
        {
            e.line("fail_on_error = true");
        }
        if let Some(px) = d.get("proxy") {
            e.line(&format!("proxy = {}", toml_string(px)));
        }
        if let Some(auth) = d.get("auth") {
            if auth.eq_ignore_ascii_case("sspi") {
                e.warn("download auth=\"sspi\" is not supported by rsw (basic only); the download may fail without credentials.");
            } else if auth.eq_ignore_ascii_case("basic") {
                e.line("auth = { kind = \"basic\", user = \"\", password = \"\" }   # fill in credentials");
            }
        }
    }

    // --- [[map_drive]] (v3 sharedDirectoryMapping > map) --------------------
    let maps: Vec<&BTreeMap<String, String>> = w
        .attr_elements
        .iter()
        .filter(|(n, _)| n == "map")
        .map(|(_, a)| a)
        .collect();
    for m in maps {
        if let (Some(label), Some(unc)) = (m.get("label"), m.get("uncpath")) {
            e.line("");
            e.line("[[map_drive]]");
            e.line(&format!("label = {}", toml_string(label)));
            e.line(&format!("unc_path = {}", toml_string(unc)));
        }
    }

    // deliberately unsupported elements the user mentioned in the XML
    if w.text("beeponshutdown").is_some() {
        e.warn("beeponshutdown has no equivalent in rsw.");
    }
    if w.text("interactive").is_some() {
        e.warn("interactive services are not supported by rsw (deprecated by Microsoft).");
    }
    if w.text("stopparentprocessfirst").is_some() {
        e.warn("stopparentprocessfirst was removed in WinSW v3 and has no equivalent in rsw (tree kill order is fixed).");
    }

    // footer: next steps for the takeover
    e.line("");
    e.line("# ---- takeover steps -------------------------------------------------");
    e.line("# 1) stop & remove the WinSW-managed service:");
    e.line("#      <winsw-exe> stop   (or: sc stop <id>)");
    e.line("#      <winsw-exe> uninstall   (or: sc delete <id>)");
    e.line("# 2) replace <winsw-exe> with rsw.exe renamed to match this file's stem, then:");
    e.line("#      rsw install <this-file>");
    e.line("#      rsw start <this-file>");

    for w in &e.warnings {
        eprintln!("rsw convert: note — {w}");
    }
    e.text
}

fn parse_duration_secs(s: &str) -> u64 {
    let s = s.trim();
    // WinSW TimeSpan form: HH:mm:ss (e.g. preshutdownTimeout "00:03:00").
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() == 3
        && let (Ok(h), Ok(m), Ok(sec)) = (
            parts[0].parse::<u64>(),
            parts[1].parse::<u64>(),
            parts[2].parse::<u64>(),
        )
    {
        return h * 3600 + m * 60 + sec;
    }
    crate::config::parse_duration_ms(s)
        .map(|ms| ms / 1000)
        .unwrap_or(15)
}

/// CLI entry: converts `winsw_xml` into an rsw TOML next to it (or `output`).
pub fn run(winsw_xml: &Path, output: Option<&Path>) -> anyhow::Result<PathBuf> {
    let cfg = parse_winsw_xml(winsw_xml)?;
    let out_path = output.map(Path::to_path_buf).unwrap_or_else(|| {
        let stem = winsw_xml
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("rsw-service");
        winsw_xml.with_file_name(format!("{stem}.toml"))
    });
    let text = generate(&cfg, winsw_xml);
    std::fs::write(&out_path, text).with_context(|| format!("writing {}", out_path.display()))?;
    println!(
        "converted {} -> {}",
        winsw_xml.display(),
        out_path.display()
    );
    println!(
        "takeover: uninstall the WinSW service, then `rsw install {}`",
        out_path.display()
    );
    Ok(out_path)
}
