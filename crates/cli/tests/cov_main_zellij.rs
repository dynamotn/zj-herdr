//! `ZellijCli` against a fake `zellij` script on PATH. One test, because
//! PATH is process-wide.

use std::os::unix::fs::PermissionsExt;

use zj_herdr::zellij::{Zellij, ZellijCli};

#[test]
fn the_cli_runs_zellij_and_reports_what_it_says() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("zellij");
    // The first argument after any `--session S` picks the answer; the
    // `MODE` variable makes `list-sessions` fail in the ways Zellij does.
    std::fs::write(
        &bin,
        r#"#!/bin/sh
echo "$@" >> "$FAKE_LOG"
case "$*" in
  list-sessions*)
    case "$MODE" in
      none) echo "No active zellij sessions found." >&2; exit 1 ;;
      broken) echo "boom" >&2; exit 2 ;;
    esac
    printf 'main\nold [Created 1d ago] (EXITED - attach to resurrect)\n' ;;
  *list-clients*) printf 'CLIENT_ID ZELLIJ_PANE_ID RUNNING_COMMAND\n1 terminal_0 claude\n' ;;
  *list-panes*) echo '[{"id":3,"is_plugin":false,"tab_position":1,"tab_name":"api"}]' ;;
  *--plugin*) echo "acked" ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log = dir.path().join("log");
    std::env::set_var("FAKE_LOG", &log);
    let path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:{path}", dir.path().display()));

    let z = ZellijCli;
    assert_eq!(z.sessions().unwrap(), ["main"]);
    std::env::set_var("MODE", "none");
    assert_eq!(z.sessions().unwrap(), Vec::<String>::new());
    std::env::set_var("MODE", "broken");
    assert!(z.sessions().unwrap_err().contains("boom"));

    assert!(z.attached("dev").unwrap());
    z.pipe("dev", "name", "payload").unwrap();
    assert_eq!(
        z.deliver("dev", "zj_herdr", "name", "payload")
            .unwrap()
            .trim(),
        "acked"
    );
    z.run("dev", Some("/w"), &["claude".to_string()]).unwrap();
    assert!(z
        .run("dev", None, &[])
        .unwrap_err()
        .contains("needs a command"));
    let panes = z.panes("dev").unwrap();
    assert_eq!(panes[&3].name, "api");

    let calls = std::fs::read_to_string(&log).unwrap();
    for want in [
        "--session dev pipe --name name -- payload",
        "--session dev action pipe --plugin zj_herdr",
        "--session dev run --cwd /w -- claude",
        "--session dev action list-panes --json --tab",
    ] {
        assert!(calls.contains(want), "{want}: {calls}");
    }
}
