//! Run external commands (zellij, ssh) with a hard timeout.

use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// How often a running child is polled for exit.
const POLL: Duration = Duration::from_millis(20);
/// A scan of hundreds of agents is tens of KB; a remote sending more than
/// this is broken or hostile, and must not grow the daemon without bound.
pub const MAX_OUTPUT: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
}

/// Run `cmd` with stdin closed; fail on timeout or a non-zero exit.
pub fn run(mut cmd: Command, timeout: Duration) -> Result<Output, String> {
    let program = cmd.get_program().to_string_lossy().into_owned();
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("cannot run {program}: {e}"))?;

    // Drain pipes on threads so a chatty child cannot fill them and stall;
    // keep at most MAX_OUTPUT bytes of each, discarding the rest.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.by_ref().take(MAX_OUTPUT).read_to_end(&mut buf);
                let _ = std::io::copy(&mut p, &mut std::io::sink());
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    };
    let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
    let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{program} timed out after {}s", timeout.as_secs_f32()));
            }
            Ok(None) => thread::sleep(POLL),
            Err(e) => return Err(format!("{program}: {e}")),
        }
    };
    let output = Output {
        stdout: out.join().unwrap_or_default(),
        stderr: err.join().unwrap_or_default(),
    };
    if status.success() {
        Ok(output)
    } else {
        let detail = output.stderr.trim();
        Err(format!(
            "{program} exited with {status}{}{detail}",
            if detail.is_empty() { "" } else { ": " }
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn captures_output() {
        let out = run(sh("echo hi; echo oops >&2"), Duration::from_secs(5)).unwrap();
        assert_eq!(out.stdout, "hi\n");
        assert_eq!(out.stderr, "oops\n");
    }

    #[test]
    fn caps_output() {
        let script = format!("head -c {} /dev/zero", MAX_OUTPUT + 4096);
        let out = run(sh(&script), Duration::from_secs(30)).unwrap();
        assert_eq!(out.stdout.len() as u64, MAX_OUTPUT);
    }

    #[test]
    fn reports_failure_and_timeout() {
        let err = run(sh("echo bad >&2; exit 3"), Duration::from_secs(5)).unwrap_err();
        assert!(err.contains("bad"), "{err}");
        let err = run(sh("sleep 5"), Duration::from_millis(100)).unwrap_err();
        assert!(err.contains("timed out"), "{err}");
    }
}
