//! Human-readable output for the CLI.

use zj_herdr_core::{fmt_elapsed, Agent, AgentState, Machine, MachineStatus};

/// Replace control characters so names from processes or remote hosts
/// cannot drive the terminal with escape sequences.
pub fn clean(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

fn pane_label(agent: &Agent) -> String {
    agent.pane.as_ref().map_or_else(
        || "-".to_string(),
        |p| format!("{}:{}", p.session, p.pane_id),
    )
}

/// `box/123 claude blocked dev:3 /w/proj`
pub fn agent_line(agent: &Agent) -> String {
    clean(&format!(
        "{} {} {} {} {}",
        agent.id,
        agent.kind,
        agent.state,
        pane_label(agent),
        agent.cwd.as_deref().unwrap_or("-")
    ))
}

/// `box unreachable: timeout` or `box ok`
pub fn machine_line(name: &str, status: MachineStatus, error: Option<&str>) -> String {
    clean(&match (status, error) {
        (MachineStatus::Unreachable, Some(e)) => format!("{name} unreachable: {e}"),
        (MachineStatus::Unreachable, None) => format!("{name} unreachable"),
        (MachineStatus::Ok, _) => format!("{name} ok"),
    })
}

/// `box/123 claude idle -> blocked dev:3 /w/proj`
pub fn change_line(agent: &Agent, from: Option<AgentState>) -> String {
    let from = from.map_or_else(|| "new".to_string(), |s| s.to_string());
    clean(&format!(
        "{} {} {from} -> {} {} {}",
        agent.id,
        agent.kind,
        agent.state,
        pane_label(agent),
        agent.cwd.as_deref().unwrap_or("-")
    ))
}

/// One line per agent, unreachable machines noted, blocked agents first.
pub fn table(machines: &[Machine], now: u64) -> String {
    let mut rows = vec![["ID", "STATE", "FOR", "KIND", "PANE", "CWD"].map(str::to_string)];
    let mut notes = Vec::new();
    for m in machines {
        if m.status == MachineStatus::Unreachable {
            notes.push(format!(
                "{}: unreachable ({})",
                m.name,
                m.error.as_deref().unwrap_or("no reason")
            ));
        }
        let mut agents: Vec<&Agent> = m.agents.iter().collect();
        agents.sort_by_key(|a| (a.state.attention_rank(), a.pid));
        for a in agents {
            rows.push([
                a.id.clone(),
                a.state.to_string(),
                fmt_elapsed(now.saturating_sub(a.since)),
                a.kind.clone(),
                pane_label(a),
                a.cwd.clone().unwrap_or_default(),
            ]);
        }
    }
    let widths: Vec<usize> = (0..6)
        .map(|c| rows.iter().map(|r| r[c].chars().count()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, w)| format!("{cell:<w$}"))
            .collect();
        out.push_str(&clean(line.join("  ").trim_end()));
        out.push('\n');
    }
    for note in notes {
        out.push_str(&clean(&note));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(pid: u32, state: AgentState, pane: Option<zj_herdr_core::Pane>) -> Agent {
        Agent {
            id: format!("box/{pid}"),
            machine: "box".into(),
            kind: "claude".into(),
            pid,
            cwd: Some("/w/p\x1b[31m".into()),
            state,
            since: 40,
            pane,
            session_id: None,
            waiting: None,
            started: 0,
            tokens: None,
            error: false,
            acked: false,
            tasks: vec![],
            model: None,
            context: None,
            background: false,
            title: None,
        }
    }

    #[test]
    fn lines_name_the_agent_its_pane_and_its_change() {
        let pane = Some(zj_herdr_core::Pane::new("dev", 3));
        let a = agent(7, AgentState::Blocked, pane);
        assert_eq!(agent_line(&a), "box/7 claude blocked dev:3 /w/p?[31m");
        let b = agent(8, AgentState::Idle, None);
        assert_eq!(agent_line(&b), "box/8 claude idle - /w/p?[31m");
        assert_eq!(
            change_line(&a, Some(AgentState::Working)),
            "box/7 claude working -> blocked dev:3 /w/p?[31m"
        );
        assert!(change_line(&b, None).starts_with("box/8 claude new -> idle"));
        assert_eq!(
            machine_line("far", MachineStatus::Unreachable, Some("timeout")),
            "far unreachable: timeout"
        );
        assert_eq!(
            machine_line("far", MachineStatus::Unreachable, None),
            "far unreachable"
        );
        assert_eq!(
            machine_line("box", MachineStatus::Ok, Some("old")),
            "box ok"
        );
    }

    #[test]
    fn the_table_puts_blocked_agents_first_and_notes_unreachable_machines() {
        let machines = [
            Machine {
                name: "box".into(),
                ssh: None,
                status: MachineStatus::Ok,
                error: None,
                agents: vec![
                    agent(2, AgentState::Working, None),
                    agent(1, AgentState::Blocked, None),
                ],
            },
            Machine {
                name: "far".into(),
                ssh: Some("far".into()),
                status: MachineStatus::Unreachable,
                error: None,
                agents: vec![],
            },
        ];
        let table = table(&machines, 100);
        let lines: Vec<&str> = table.lines().collect();
        assert!(lines[0].starts_with("ID     STATE"), "{table}");
        assert!(lines[1].starts_with("box/1  blocked  1m"), "{table}");
        assert!(lines[2].starts_with("box/2  working"), "{table}");
        assert_eq!(lines[3], "far: unreachable (no reason)");
    }

    #[test]
    fn strips_terminal_escapes() {
        assert_eq!(clean("/w/\x1b]0;pwned\x07x"), "/w/?]0;pwned?x");
        assert_eq!(clean("plain dir"), "plain dir");
    }
}
