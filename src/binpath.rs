//! binPath argument encoding: MSVCRT escaping and CommandLineToArgvW parsing.
//!
//! The SCM launches the service with `CreateProcess(cmdline, ...)` and every
//! consumer (including rsw's own clap) splits the command line with
//! CommandLineToArgvW semantics. `windows-service` escapes `launch_arguments`
//! with those rules; this module pins the contract with round-trip tests so
//! quoting bugs (the "quote-if-space" class of defect shawl shipped) cannot
//! creep in unnoticed.

/// Escapes one argument per MSVCRT rules (see "Everyone quotes command line
/// arguments the wrong way"): quote only when needed, double backslash runs
/// that precede a quote or close a quoted section.
pub fn escape(arg: &str) -> String {
    let needs_quotes = arg.is_empty()
        || arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '\u{000B}' | '"'));
    if !needs_quotes {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // Every backslash before a literal quote is doubled, then
                // escape the quote itself.
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                out.push(c);
            }
        }
    }
    // Backslashes directly before the closing quote are doubled.
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// Joins arguments into one command line.
pub fn join(args: &[String]) -> String {
    args.iter().map(|a| escape(a)).collect::<Vec<_>>().join(" ")
}

/// Parses a command line with CommandLineToArgvW semantics (also used by
/// `rsw convert` to split WinSW argument strings into rsw argument arrays).: `2n` backslashes
/// followed by a quote toggle in-quote mode and keep `n` backslashes, `2n+1`
/// backslashes + quote yield a literal quote; an unterminated quote is closed
/// at end of input.
pub fn parse(cmdline: &str) -> Vec<String> {
    let chars: Vec<char> = cmdline.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut has_arg = false;
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            let run = chars[i..].iter().take_while(|&&b| b == '\\').count();
            let followed_by_quote = i + run < chars.len() && chars[i + run] == '"';
            // A quote only half-consumes a backslash run (pairing rule), but
            // the whole run is consumed either way.
            let literal_pairs = if followed_by_quote { run / 2 } else { run };
            for _ in 0..literal_pairs {
                cur.push('\\');
            }
            i += run;
            if followed_by_quote {
                if run % 2 == 0 {
                    in_quotes = !in_quotes;
                } else {
                    cur.push('"');
                }
                i += 1; // consume the quote
            }
            has_arg = true;
        } else if c == '"' {
            in_quotes = !in_quotes;
            has_arg = true;
            i += 1;
        } else if (c == ' ' || c == '\t') && !in_quotes {
            if has_arg {
                out.push(std::mem::take(&mut cur));
                has_arg = false;
            }
            i += 1;
        } else {
            cur.push(c);
            has_arg = true;
            i += 1;
        }
    }
    if has_arg {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cross-checks `parse` against the real Windows API.
    fn argv_from_windows(cmdline: &str) -> Vec<String> {
        use std::os::windows::ffi::OsStringExt as _;
        use windows::Win32::Foundation::LocalFree;
        use windows::Win32::UI::Shell::CommandLineToArgvW;
        use windows::core::PCWSTR;
        let wide: Vec<u16> = cmdline.encode_utf16().chain([0]).collect();
        unsafe {
            let mut count = 0i32;
            let argv = CommandLineToArgvW(PCWSTR(wide.as_ptr()), &mut count);
            assert!(!argv.is_null(), "CommandLineToArgvW failed for {cmdline}");
            let args: Vec<String> = std::slice::from_raw_parts(argv, count as usize)
                .iter()
                .map(|p| {
                    let mut len = 0;
                    while *p.0.add(len) != 0 {
                        len += 1;
                    }
                    std::ffi::OsString::from_wide(std::slice::from_raw_parts(p.0, len))
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
            let _ = LocalFree(Some(windows::Win32::Foundation::HLOCAL(
                argv as *mut core::ffi::c_void,
            )));
            args
        }
    }

    #[test]
    fn round_trip_through_real_api() {
        // NOTE: argv[0] is parsed by CreateProcess's own binPath rules, not
        // CommandLineToArgvW's argument rules (backslash runs in the program
        // path are not pair-folded), so exotic cases live in arg positions.
        let cases: Vec<Vec<&str>> = vec![
            vec![
                r"C:\apps\rsw.exe",
                "run-service",
                "--config",
                r"C:\my app\app.toml",
            ],
            vec![r"C:\apps\rsw.exe", "--name", "hello world"],
            vec![
                "app.exe",
                "plain",
                "",
                "with space",
                r"trailing\",
                r#"back\"quote"#,
                "a\"b",
            ],
            vec![
                "app.exe",
                r"ends with backslashes \\",
                "quote\"inside",
                "a\tb\t",
                r"C:\dir with space\",
                r"\\server\share\",
            ],
            vec!["app.exe", "one"],
        ];
        for args in cases {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            let cmdline = join(&owned);
            assert_eq!(
                parse(&cmdline),
                owned,
                "own parser round-trip failed for {cmdline}"
            );
            assert_eq!(
                argv_from_windows(&cmdline),
                owned,
                "CommandLineToArgvW disagrees for {cmdline}"
            );
        }
    }

    #[test]
    fn escape_skips_quotes_when_unneeded() {
        assert_eq!(escape(r"C:\apps\rsw.exe"), r"C:\apps\rsw.exe");
        assert_eq!(escape("plain"), "plain");
    }

    #[test]
    fn parse_handles_unterminated_quote() {
        // CommandLineToArgvW closes an unterminated quote at end of input.
        assert_eq!(parse(r#""open arg tail"#), vec!["open arg tail"]);
    }

    #[test]
    fn join_empty_list() {
        assert_eq!(join(&[]), "");
        assert_eq!(parse(""), Vec::<String>::new());
    }
}
