//! `rsw convert` tests: WinSW XML → rsw TOML, verified by parsing the output
//! back through the real config loader.

use std::process::Command;

const RSW: &str = env!("CARGO_BIN_EXE_rsw");

fn convert(xml_path: &std::path::Path, out_path: &std::path::Path) {
    let out = Command::new(RSW)
        .args([
            "convert",
            xml_path.to_str().unwrap(),
            "-o",
            out_path.to_str().unwrap(),
        ])
        .output()
        .expect("run rsw convert");
    assert!(
        out.status.success(),
        "convert failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn parse_output(out_path: &std::path::Path) -> toml::Table {
    let text = std::fs::read_to_string(out_path).unwrap();
    toml::from_str(&text).expect("generated TOML must be parseable")
}

/// The real-world DeepSeek Harness WinSW definition.
#[test]
fn converts_real_dsh_service_xml() {
    let xml =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/dsh-service.xml");
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("dsh-service.toml");
    convert(&xml, &out);

    let table = parse_output(&out);
    let service = table["service"].as_table().unwrap();
    assert_eq!(service["id"].as_str(), Some("DeepSeekHarness"));
    assert_eq!(service["start_type"].as_str(), Some("auto"));
    assert_eq!(service["failure_reset_after"].as_str(), Some("1 hour"));

    let process = table["process"].as_table().unwrap();
    let args = process["arguments"].as_array().unwrap();
    let args: Vec<&str> = args.iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(
        args,
        vec![
            "apps/cli/lib/bin.js",
            "web",
            "--patch",
            "service/browse-picker.patch.yml",
            "--no-open",
            "--port",
            "3080"
        ],
        "arguments must be split command-line style with the entry script first"
    );

    let logging = table["logging"].as_table().unwrap();
    assert_eq!(logging["mode"].as_str(), Some("roll-by-size-time"));
    assert_eq!(logging["size_threshold_mb"].as_integer(), Some(10));
    assert_eq!(logging["time_pattern"].as_str(), Some("yyyyMMdd"));
    assert_eq!(logging["auto_roll_at"].as_str(), Some("00:00:00"));
    assert_eq!(logging["keep_files"].as_integer(), Some(7)); // zipOlderThanNumDays

    let failures = table["on_failure"].as_array().unwrap();
    assert_eq!(failures.len(), 2);
    assert_eq!(failures[0]["delay"].as_str(), Some("10 sec"));
    assert_eq!(failures[1]["delay"].as_str(), Some("30 sec"));

    let env = table["env"].as_table().unwrap();
    assert_eq!(env["NODE_ENV"].as_str(), Some("production"));
}

/// A hand-written edge-case XML: env/depend/onfailure/download/account/
/// stoparguments/legacy logmode, plus case-insensitive element names.
#[test]
fn converts_edge_case_xml() {
    let dir = tempfile::tempdir().unwrap();
    let xml = dir.path().join("edge.xml");
    std::fs::write(
        &xml,
        r#"<SERVICE>
  <ID>EdgeSvc</ID>
  <EXECUTABLE>C:\Program Files\app\run.cmd</EXECUTABLE>
  <ARGUMENTS>--name "hello world" --flag</ARGUMENTS>
  <StopArguments>shutdown-now</StopArguments>
  <WorkingDirectory>%BASE%\work</WorkingDirectory>
  <LogPath>%BASE%\logs</LogPath>
  <LogMode>rotate</LogMode>
  <StartMode>Manual</StartMode>
  <Depend>Tcpip</Depend>
  <Depend>Dhcp</Depend>
  <Env name="A" value="1"/>
  <OnFailure action="restart" delay="5 sec"/>
  <ResetFailure>2 hours</ResetFailure>
  <serviceaccount>
    <username>.\svc_edge</username>
    <password>secret</password>
  </serviceaccount>
  <download from="https://example.com/a.jar" to="a.jar" failOnError="true"/>
  <HideWindow>true</HideWindow>
  <Priority>AboveNormal</Priority>
</SERVICE>
"#,
    )
    .unwrap();
    let out = dir.path().join("edge.toml");
    convert(&xml, &out);

    // The output must load and validate through the real config layer.
    let check = Command::new(RSW)
        .args(["validate", out.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "generated TOML fails rsw validate: {}",
        String::from_utf8_lossy(&check.stderr)
    );

    let table = parse_output(&out);
    assert_eq!(table["service"]["id"].as_str(), Some("EdgeSvc"));
    assert_eq!(table["service"]["start_type"].as_str(), Some("manual"));
    assert_eq!(
        table["service"]["dependencies"].as_array().unwrap().len(),
        2
    );
    assert_eq!(
        table["service"]["failure_reset_after"].as_str(),
        Some("2 hours")
    );
    let account = table["service"]["account"].as_table().unwrap();
    assert_eq!(account["username"].as_str(), Some(".\\svc_edge"));

    let process = table["process"].as_table().unwrap();
    assert_eq!(
        process["arguments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["--name", "hello world", "--flag"],
        "quoted space argument must survive the split"
    );
    assert_eq!(
        process["stop_arguments"].as_array().unwrap()[0].as_str(),
        Some("shutdown-now")
    );
    assert_eq!(process["priority"].as_str(), Some("above-normal"));
    assert_eq!(process["hide_window"].as_bool(), Some(true));

    let logging = table["logging"].as_table().unwrap();
    assert_eq!(logging["mode"].as_str(), Some("roll-by-size")); // legacy "rotate"

    let downloads = table["download"].as_array().unwrap();
    assert_eq!(downloads[0]["fail_on_error"].as_bool(), Some(true));
}
