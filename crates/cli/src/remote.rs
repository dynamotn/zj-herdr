//! Remote machines: `ssh -o BatchMode=yes <host> zj-herdr scan --json`,
//! merged into the daemon's registry under the configured name.

use std::process::Command;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use zj_herdr_core::{Agent, Machine, MachineStatus, Pane, StateV1};

use crate::config::Remote;
use crate::exec;
use crate::output::clean;
use crate::registry::Registry;
use crate::scan::now_secs;

/// The ssh invocation for one scan. BatchMode makes ssh fail instead of
/// prompting for a password or host key the daemon cannot answer.
pub fn ssh_command(remote: &Remote) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.args(["-o", "BatchMode=yes", "-o"])
        .arg(format!("ConnectTimeout={}", remote.timeout_secs))
        .arg("--")
        .arg(&remote.ssh)
        .arg(format!("{} scan --json", remote.command));
    cmd
}

/// An empty entry for a configured machine, before its first answer.
pub fn template(remote: &Remote) -> Machine {
    Machine {
        name: remote.name.clone(),
        ssh: Some(remote.ssh.clone()),
        status: MachineStatus::Ok,
        error: None,
        agents: vec![],
    }
}

/// Turn a remote `scan --json` into one machine named after the config
/// entry, re-keying agent ids so they cannot collide with local ones. The
/// remote is untrusted: its strings lose any control characters.
pub fn merge_remote(remote: &Remote, stdout: &str) -> Result<Machine, String> {
    let state = StateV1::from_json(stdout.trim()).map_err(|e| format!("{}: {e}", remote.name))?;
    let agents = state
        .machines
        .into_iter()
        .flat_map(|m| m.agents)
        .map(|a| Agent {
            id: Agent::make_id(&remote.name, a.pid),
            machine: remote.name.clone(),
            kind: clean(&a.kind),
            cwd: a.cwd.as_deref().map(clean),
            pane: a.pane.map(|p| Pane {
                session: clean(&p.session),
                ..p
            }),
            session_id: a.session_id.as_deref().map(clean),
            ..a
        })
        .collect();
    Ok(Machine {
        agents,
        ..template(remote)
    })
}

/// Poll one remote forever.
pub fn poll_loop(remote: Remote, registry: Arc<Registry>) {
    let interval = Duration::from_secs(remote.interval_secs);
    let timeout = Duration::from_secs(remote.timeout_secs);
    loop {
        let started = Instant::now();
        let result = exec::run(ssh_command(&remote), timeout).and_then(|out| merge_remote(&remote, &out.stdout));
        match result {
            Ok(machine) => {
                registry.publish(machine, now_secs());
            }
            Err(e) => registry.mark_unreachable(template(&remote), e),
        }
        thread::sleep(interval.saturating_sub(started.elapsed()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zj_herdr_core::{AgentState, Pane};

    fn remote() -> Remote {
        crate::config::Config::parse("[[remote]]\nname = \"far\"\nssh = \"far-alias\"")
            .unwrap()
            .remote
            .remove(0)
    }

    #[test]
    fn ssh_command_is_batch_mode_and_option_safe() {
        let cmd = ssh_command(&remote());
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(cmd.get_program(), "ssh");
        assert_eq!(
            args,
            [
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "--",
                "far-alias",
                "zj-herdr scan --json"
            ]
        );
    }

    #[test]
    fn merges_remote_scan_under_configured_name() {
        let remote_state = StateV1::new(
            7,
            vec![Machine {
                name: "far.local".into(),
                ssh: None,
                status: MachineStatus::Ok,
                error: None,
                agents: vec![Agent {
                    id: "far.local/55".into(),
                    machine: "far.local".into(),
                    kind: "codex".into(),
                    pid: 55,
                    cwd: Some("/srv/app\x1b[2J".into()),
                    state: AgentState::Blocked,
                    since: 3,
                    pane: Some(Pane {
                        session: "ops".into(),
                        pane_id: 2,
                    }),
                    session_id: Some("r1".into()),
                }],
            }],
        );
        let merged = merge_remote(&remote(), &format!("{}\n", remote_state.to_json())).unwrap();
        assert_eq!(merged.name, "far");
        assert_eq!(merged.ssh.as_deref(), Some("far-alias"));
        assert_eq!(merged.status, MachineStatus::Ok);
        let a = &merged.agents[0];
        assert_eq!((a.id.as_str(), a.machine.as_str()), ("far/55", "far"));
        assert_eq!(
            (a.state, a.since, a.session_id.as_deref()),
            (AgentState::Blocked, 3, Some("r1"))
        );
        assert_eq!(a.pane.as_ref().unwrap().session, "ops");
        assert_eq!(
            a.cwd.as_deref(),
            Some("/srv/app?[2J"),
            "escape sequences are neutralised"
        );
    }

    #[test]
    fn rejects_garbage_output() {
        assert!(merge_remote(&remote(), "bash: zj-herdr: command not found").is_err());
        assert!(merge_remote(
            &remote(),
            r#"{"schema":"zj_herdr.state.v2","generated_at":0,"machines":[]}"#
        )
        .is_err());
    }

    #[test]
    fn merged_remote_lands_in_the_registry_next_to_local() {
        let reg = Registry::default();
        reg.publish(
            Machine {
                name: "box".into(),
                ..template(&remote())
            },
            1,
        );
        let out = StateV1::new(1, vec![]).to_json();
        reg.publish(merge_remote(&remote(), &out).unwrap(), 1);
        reg.mark_unreachable(template(&remote()), "ssh: timed out".into());
        let snap = reg.snapshot();
        let names: Vec<&str> = snap.machines.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["box", "far"]);
        assert_eq!(snap.machines[1].status, MachineStatus::Unreachable);
        assert_eq!(snap.machines[0].status, MachineStatus::Ok);
    }
}
