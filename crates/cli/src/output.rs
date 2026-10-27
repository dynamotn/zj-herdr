//! Human-readable output for the CLI.

use zj_herdr_core::{fmt_elapsed, Agent, AgentState, Machine, MachineStatus};

/// Replace control characters so names from processes or remote hosts
/// cannot drive the terminal with escape sequences.
pub fn clean(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { '?' } else { c }).collect()
}

fn pane_label(agent: &Agent) -> String {
    agent
        .pane
        .as_ref()
        .map_or_else(|| "-".to_string(), |p| format!("{}:{}", p.session, p.pane_id))
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
        let line: Vec<String> = row.iter().zip(&widths).map(|(cell, w)| format!("{cell:<w$}")).collect();
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

    #[test]
    fn strips_terminal_escapes() {
        assert_eq!(clean("/w/\x1b]0;pwned\x07x"), "/w/?]0;pwned?x");
        assert_eq!(clean("plain dir"), "plain dir");
    }
}
