//! `exec::run` against a child that outlives its timeout.

use std::process::Command;
use std::time::Duration;

use zj_herdr::exec;

#[test]
fn a_child_that_outlives_its_timeout_is_killed_and_reported() {
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "sleep 30"]);
    let err = exec::run(cmd, Duration::from_millis(100)).unwrap_err();
    assert!(err.starts_with("/bin/sh timed out after "), "{err}");
}
