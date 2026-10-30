//! `exec::run` when the OS cannot say how the child ended. Its own test
//! binary, because ignoring SIGCHLD is process-wide.

use std::process::Command;
use std::time::Duration;

use zj_herdr::exec;

#[test]
fn a_wait_error_is_reported_with_the_program_name() {
    // With SIGCHLD ignored the kernel reaps children itself, and
    // `waitpid` fails with ECHILD once the child is gone.
    // SAFETY: setting a disposition is async-signal-safe; nothing else here
    // depends on SIGCHLD.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
    let mut cmd = Command::new("/bin/sh");
    cmd.args(["-c", "sleep 0.2"]);
    let err = exec::run(cmd, Duration::from_secs(5)).unwrap_err();
    assert!(err.starts_with("/bin/sh: "), "{err}");
}
