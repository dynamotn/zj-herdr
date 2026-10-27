use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use zj_herdr::api::{CommandLine, Event, Request};
use zj_herdr::config::Config;
use zj_herdr::detect::Roots;
use zj_herdr::scan::{local_machine_name, now_secs, Scanner};
use zj_herdr::sessions::Store;
use zj_herdr::zellij::{Zellij, ZellijCli};
use zj_herdr::{client, daemon, output, paths};
use zj_herdr_core::{Agent, AgentState, StateV1};

/// Hook-free agent runtime for Zellij.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Daemon socket (default: $ZJ_HERDR_SOCKET, $XDG_RUNTIME_DIR/zj-herdr.sock
    /// or $TMPDIR/zj-herdr-$UID.sock).
    #[arg(long, global = true)]
    socket: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Scan this machine once and print every agent found.
    Scan {
        /// Print the `zj_herdr.state.v1` JSON snapshot.
        #[arg(long)]
        json: bool,
    },
    /// Run the daemon: rescan every second, poll remotes, serve the socket API.
    Daemon {
        /// Config file (default: $XDG_CONFIG_HOME/zj-herdr/config.toml).
        #[arg(long)]
        config: Option<PathBuf>,
    },
    /// List all agents known to the daemon, local and remote.
    List {
        /// Print the `zj_herdr.state.v1` JSON snapshot.
        #[arg(long)]
        json: bool,
    },
    /// Wait until an agent reaches a state.
    Wait {
        /// Agent id (`machine/pid`), pid, or `session:pane_id`.
        agent: String,
        /// working, blocked, idle or done.
        state: AgentState,
        /// Give up after this many seconds (exit status 1).
        #[arg(long)]
        timeout: Option<f64>,
    },
    /// Type a prompt into an agent's pane and press Enter.
    Prompt {
        /// Agent id (`machine/pid`), pid, or `session:pane_id`.
        agent: String,
        /// Text to send; several words are joined with spaces.
        #[arg(required = true)]
        text: Vec<String>,
        /// Type the text without pressing Enter.
        #[arg(long)]
        no_enter: bool,
    },
    /// Open a new pane running a command in a Zellij session.
    Spawn {
        #[arg(long, short)]
        session: String,
        #[arg(long)]
        cwd: Option<PathBuf>,
        /// Command and arguments, after `--`.
        #[arg(last = true, required = true)]
        command: Vec<String>,
    },
    /// Stream state changes as they happen.
    Watch {
        /// Print raw event JSON lines.
        #[arg(long)]
        json: bool,
    },
    /// Reopen recorded agent sessions in their Zellij sessions with their
    /// resume command (`claude --resume <id>`, `codex resume <id>`).
    Restore {
        /// Only this Zellij session.
        #[arg(long, short)]
        session: Option<String>,
        /// Print what would be reopened without doing it.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Local agents that are running now, from the daemon or a fresh scan.
fn live_local_agents(socket: &std::path::Path) -> Vec<Agent> {
    let machine = local_machine_name();
    match client::request(socket, &Request::List) {
        Ok(resp) => resp
            .state
            .map(|s| {
                s.agents()
                    .filter(|a| a.machine == machine)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default(),
        Err(_) => Scanner::scan_once(machine, &Roots::from_env()).agents,
    }
}

fn restore(socket: &std::path::Path, session: Option<&str>, dry_run: bool) -> Result<(), String> {
    let store = Store::load(&paths::state_dir().join("sessions.json"))?;
    let (plans, skipped) = store.plan(session, &live_local_agents(socket));
    for note in skipped {
        eprintln!("zj-herdr: skip {note}");
    }
    if plans.is_empty() {
        eprintln!("zj-herdr: nothing to restore");
    }
    let mut failed = 0;
    for p in plans {
        let r = &p.record;
        let line = format!(
            "{}:{} {} {}",
            r.session,
            r.pane_id,
            r.cwd.as_deref().unwrap_or("-"),
            p.argv.join(" ")
        );
        if dry_run {
            println!("would reopen {line}");
            continue;
        }
        match ZellijCli.run(&r.session, r.cwd.as_deref(), &p.argv) {
            Ok(()) => println!("reopened {line}"),
            Err(e) => {
                failed += 1;
                eprintln!("zj-herdr: {line}: {e}");
            }
        }
    }
    if failed > 0 {
        return Err(format!("{failed} session(s) could not be reopened"));
    }
    Ok(())
}

fn print_state(state: &StateV1, json: bool) {
    if json {
        println!("{}", state.to_json());
    } else {
        print!("{}", output::table(&state.machines, now_secs()));
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let socket = cli.socket.unwrap_or_else(paths::socket_path);
    match cli.command {
        Command::Scan { json } => {
            let machine = Scanner::scan_once(local_machine_name(), &Roots::from_env());
            print_state(&StateV1::new(now_secs(), vec![machine]), json);
        }
        Command::Daemon { config } => {
            let config = Config::load(&config.unwrap_or_else(paths::config_path))?;
            daemon::run(daemon::Options {
                socket,
                machine: local_machine_name(),
                roots: Roots::from_env(),
                remotes: config.remote,
                plugin: config.plugin,
                sessions: paths::state_dir().join("sessions.json"),
            })?;
        }
        Command::List { json } => match client::request(&socket, &Request::List) {
            Ok(resp) => print_state(
                &resp
                    .state
                    .unwrap_or_else(|| StateV1::new(now_secs(), vec![])),
                json,
            ),
            Err(e) => {
                eprintln!("zj-herdr: {e}\nzj-herdr: falling back to a local scan");
                let machine = Scanner::scan_once(local_machine_name(), &Roots::from_env());
                print_state(&StateV1::new(now_secs(), vec![machine]), json);
            }
        },
        Command::Wait {
            agent,
            state,
            timeout,
        } => {
            let resp = client::request(
                &socket,
                &Request::Wait {
                    agent,
                    state,
                    timeout,
                },
            )?;
            if let Some(agent) = resp.agent {
                println!("{}", output::agent_line(&agent));
            }
        }
        Command::Prompt {
            agent,
            text,
            no_enter,
        } => {
            client::request(
                &socket,
                &Request::Prompt {
                    agent,
                    text: text.join(" "),
                    submit: !no_enter,
                },
            )?;
        }
        Command::Spawn {
            session,
            cwd,
            command,
        } => {
            let cwd = cwd.map(|c| {
                std::path::absolute(&c)
                    .unwrap_or(c)
                    .to_string_lossy()
                    .into_owned()
            });
            client::request(
                &socket,
                &Request::Spawn {
                    session,
                    cwd,
                    command: CommandLine::Argv(command),
                },
            )?;
        }
        Command::Watch { json } => client::subscribe(&socket, |event| {
            if json {
                print!("{}", zj_herdr::api::to_line(&event));
            } else {
                match &event {
                    Event::Snapshot { state } => {
                        print!("{}", output::table(&state.machines, now_secs()))
                    }
                    Event::Change { agent, from } => {
                        println!("{}", output::change_line(agent, *from))
                    }
                }
            }
            true
        })?,
        Command::Restore { session, dry_run } => restore(&socket, session.as_deref(), dry_run)?,
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("zj-herdr: {e}");
            ExitCode::FAILURE
        }
    }
}
