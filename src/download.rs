//! Startup downloads (WinSW `download` element): fetch files over HTTP(S)
//! before the child process is spawned.
//!
//! Mirrors WinSW's caching behavior: an `If-Modified-Since` header derived
//! from the destination's mtime, HTTP 304 treated as success, and the file's
//! mtime restored from the server's `Last-Modified` header so later starts
//! skip unchanged files. Bodies go to a `.tmp` sibling and are renamed into
//! place, so an interrupted download never truncates the destination.

use std::fs;
use std::io::Read as _;
use std::os::windows::io::AsRawHandle as _;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context as _, bail};
use chrono::{DateTime, Utc};
use ureq::Agent;

use crate::config::{AuthConfig, AuthKind, DownloadConfig, DownloadTlsConfig};
use crate::logging::LogSink;

pub fn run_downloads(
    downloads: &[DownloadConfig],
    working_dir: &Path,
    sink: &LogSink,
) -> anyhow::Result<()> {
    for d in downloads {
        if let Err(e) = download_one(d, working_dir, sink) {
            let msg = format!("download {} -> {} failed: {e}", d.from, d.to);
            if d.fail_on_error {
                sink.error(&msg);
                bail!("download failed (fail_on_error = true): {}", d.from);
            }
            sink.warn(&msg);
        }
    }
    Ok(())
}

fn download_one(d: &DownloadConfig, working_dir: &Path, sink: &LogSink) -> anyhow::Result<()> {
    // Resolve the destination relative to the working dir unless absolute.
    let dest = Path::new(&d.to);
    let dest = if dest.is_absolute() {
        dest.to_path_buf()
    } else {
        working_dir.join(dest)
    };
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }

    let agent = build_agent(d.proxy.as_deref(), d.tls.as_ref())?;
    let mut request = agent.get(&d.from);
    if let Some(auth) = &d.auth {
        request = request.header("Authorization", &basic_auth_header(auth));
    }
    if let Some(mtime) = file_mtime(&dest) {
        request = request.header("If-Modified-Since", &http_date(mtime));
        sink.info(&format!(
            "checking {} (updated since {}?)",
            d.from,
            http_date(mtime)
        ));
    } else {
        sink.info(&format!("downloading {} -> {}", d.from, dest.display()));
    }

    let mut response = match request.call() {
        Ok(r) if r.status() == 304 => {
            sink.info(&format!(
                "{} unchanged since the last download; skipping",
                d.from
            ));
            return Ok(());
        }
        Ok(r) => r,
        // depending on version, 304 may surface as a StatusCode error
        Err(ureq::Error::StatusCode(304)) => {
            sink.info(&format!(
                "{} unchanged since the last download; skipping",
                d.from
            ));
            return Ok(());
        }
        Err(ureq::Error::StatusCode(code)) => bail!("HTTP {code}"),
        Err(e) => return Err(e).context("HTTP request failed"),
    };
    if !response.status().is_success() {
        bail!("HTTP {}", response.status());
    }

    let mut body = Vec::new();
    response
        .body_mut()
        .as_reader()
        .read_to_end(&mut body)
        .context("reading response body")?;

    let tmp: PathBuf = dest.with_extension("tmp.rsw");
    fs::write(&tmp, &body).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &dest).with_context(|| format!("renaming into {}", dest.display()))?;

    if let Some(t) = response
        .headers()
        .get("Last-Modified")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_http_date)
    {
        let _ = set_file_mtime(&dest, t);
    }
    sink.info(&format!(
        "downloaded {} bytes to {}",
        body.len(),
        dest.display()
    ));
    Ok(())
}

fn build_agent(proxy: Option<&str>, tls: Option<&DownloadTlsConfig>) -> anyhow::Result<Agent> {
    let mut builder = ureq::config::Config::builder();
    if let Some(proxy_url) = proxy {
        let proxy = ureq::Proxy::new(proxy_url).context("invalid proxy URL")?;
        builder = builder.proxy(Some(proxy));
    }
    if let Some(t) = tls {
        builder = builder.tls_config(load_tls_config(t)?);
    }
    Ok(builder.build().new_agent())
}

/// TLS setup for one download entry. `ca` replaces the root set used to
/// verify the *server* certificate (verification is never disabled);
/// `client_cert` + `client_key` present this client for mTLS. File problems
/// surface as errors here so they flow through the entry's `fail_on_error`
/// semantics like any other download failure.
fn load_tls_config(t: &DownloadTlsConfig) -> anyhow::Result<ureq::tls::TlsConfig> {
    let mut builder = ureq::tls::TlsConfig::builder();
    if let Some(ca) = &t.ca {
        let roots = load_cert_chain(ca).with_context(|| format!("download tls.ca: {ca}"))?;
        builder = builder.root_certs(ureq::tls::RootCerts::new_with_certs(&roots));
    }
    if let (Some(cert), Some(key)) = (&t.client_cert, &t.client_key) {
        let chain =
            load_cert_chain(cert).with_context(|| format!("download tls.client_cert: {cert}"))?;
        let key_bytes = fs::read(key).with_context(|| format!("download tls.client_key: {key}"))?;
        let key = ureq::tls::PrivateKey::from_pem(&key_bytes).map_err(|e| {
            anyhow::anyhow!("download tls.client_key: {key}: {e} (expected an unencrypted PEM key: PKCS8 `PRIVATE KEY`, `RSA PRIVATE KEY`, or `EC PRIVATE KEY`)")
        })?;
        builder = builder.client_cert(Some(ureq::tls::ClientCert::new_with_certs(&chain, key)));
    }
    Ok(builder.build())
}

/// Every PEM certificate in the file (a CA bundle may hold several); the
/// first key, CSR, etc. sections are ignored.
fn load_cert_chain(path: &str) -> anyhow::Result<Vec<ureq::tls::Certificate<'static>>> {
    let bytes = fs::read(path).with_context(|| format!("reading {path}"))?;
    let mut certs = Vec::new();
    for item in ureq::tls::parse_pem(&bytes) {
        if let ureq::tls::PemItem::Certificate(c) = item.context("parsing PEM certificates")? {
            certs.push(c.to_owned());
        }
    }
    if certs.is_empty() {
        bail!("no PEM certificates found in {path}");
    }
    Ok(certs)
}

fn basic_auth_header(auth: &AuthConfig) -> String {
    debug_assert_eq!(auth.kind, AuthKind::Basic, "only basic auth is supported");
    format!(
        "Basic {}",
        base64(format!("{}:{}", auth.user, auth.password).as_bytes())
    )
}

/// Minimal standard-alphabet base64 encoder (avoids pulling in a crate).
fn base64(input: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(ALPHABET[(n >> 18) as usize & 0x3F] as char);
        out.push(ALPHABET[(n >> 12) as usize & 0x3F] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 0x3F] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 0x3F] as char
        } else {
            '='
        });
    }
    out
}

fn http_date(t: SystemTime) -> String {
    let dt: DateTime<Utc> = t.into();
    dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn parse_http_date(s: &str) -> Option<SystemTime> {
    let dt = chrono::NaiveDateTime::parse_from_str(s.trim(), "%a, %d %b %Y %H:%M:%S GMT").ok()?;
    Some(DateTime::<Utc>::from_naive_utc_and_offset(dt, Utc).into())
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

/// Restores a file's mtime (SetFileTime) so conditional GETs keep working.
fn set_file_mtime(path: &Path, t: SystemTime) -> std::io::Result<()> {
    use std::fs::OpenOptions;

    use windows::Win32::Foundation::{FILETIME, HANDLE};
    use windows::Win32::Storage::FileSystem::SetFileTime;

    let dt: DateTime<Utc> = t.into();
    // FILETIME = 100ns intervals since 1601-01-01; offset from the Unix epoch.
    let ticks = dt.timestamp() * 10_000_000 + 116_444_736_000_000_000;
    let filetime = FILETIME {
        dwLowDateTime: ticks as u32,
        dwHighDateTime: (ticks >> 32) as u32,
    };

    let file = OpenOptions::new().write(true).open(path)?;
    unsafe {
        SetFileTime(HANDLE(file.as_raw_handle()), None, None, Some(&filetime))
            .map_err(|e| std::io::Error::other(e.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    fn fixture(name: &str) -> String {
        format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn load_tls_config_accepts_ca_bundle_and_mtls_pair() {
        let t = DownloadTlsConfig {
            ca: Some(fixture("tls-bundle.pem")),
            client_cert: Some(fixture("tls-client.pem")),
            client_key: Some(fixture("tls-client-key.pem")),
        };
        let cfg = load_tls_config(&t).unwrap();
        assert!(cfg.client_cert().is_some(), "mTLS pair must be configured");
    }

    #[test]
    fn load_tls_config_rejects_missing_and_garbage_files() {
        let garbage = tempfile::tempdir().unwrap();
        let garbage_ca = garbage.path().join("garbage.pem");
        fs::write(&garbage_ca, b"not a pem").unwrap();

        let none = || DownloadTlsConfig {
            ca: None,
            client_cert: None,
            client_key: None,
        };
        // NB: a PEM-looking but crypto-invalid key is NOT caught here — ureq
        // only looks for the PEM section — it fails later at the TLS
        // handshake. Cases below all fail at load time.
        let cases = [
            (
                DownloadTlsConfig {
                    ca: Some(fixture("no-such-file.pem")),
                    ..none()
                },
                "tls.ca",
            ),
            (
                DownloadTlsConfig {
                    ca: Some(garbage_ca.to_string_lossy().into_owned()),
                    ..none()
                },
                "tls.ca",
            ),
            (
                // realistic mistake: client_key pointing at a certificate file
                DownloadTlsConfig {
                    client_cert: Some(fixture("tls-client.pem")),
                    client_key: Some(fixture("tls-bundle.pem")),
                    ..none()
                },
                "tls.client_key",
            ),
        ];
        for (t, needle) in &cases {
            let err = load_tls_config(t).unwrap_err().to_string();
            assert!(
                err.contains(needle),
                "error must name the offending field ({needle}): {err}"
            );
        }
    }

    #[test]
    fn http_date_roundtrip() {
        let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_444_928_880);
        let s = http_date(t);
        assert_eq!(s, "Thu, 15 Oct 2015 17:08:00 GMT");
        assert_eq!(parse_http_date(&s).unwrap(), t);
    }

    #[test]
    fn set_mtime_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, b"x").unwrap();
        let t = SystemTime::now() - std::time::Duration::from_secs(86_400);
        set_file_mtime(&path, t).unwrap();
        let got = fs::metadata(&path).unwrap().modified().unwrap();
        let delta = got
            .duration_since(t)
            .unwrap_or_else(|_| t.duration_since(got).unwrap());
        assert!(
            delta < std::time::Duration::from_secs(2),
            "mtime {got:?} vs {t:?}"
        );
    }
}
