//! The Unix socket server.

use std::collections::HashSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use zj_herdr_core::{prompt_reply, Agent, AgentState, PluginCommand, CMD_SCHEMA};

use crate::adopt::{self, Processes};
use crate::api::{self, Request, Response};
use crate::paths;
use crate::registry::Registry;
use crate::zellij::Zellij;

pub struct Context {
    pub registry: Arc<Registry>,
    pub zellij: Arc<dyn Zellij>,
    /// Name of this machine; only its agents can be prompted.
    pub machine: String,
    /// Zellij plugin URL or alias that types prompts.
    pub plugin: String,
    /// What `adopt` uses to check and stop agent processes.
    pub procs: Arc<dyn Processes>,
    /// How long `adopt` waits for an agent to exit.
    pub adopt_timeout: Duration,
    /// Pids an `adopt` is moving now; a second request for one is refused,
    /// so a double click never opens the agent twice.
    pub adopting: Mutex<HashSet<u32>>,
}

/// Bind the socket with mode 0600 in a directory others cannot tamper
/// with, replacing a stale socket of ours but refusing to steal one from a
/// running daemon or to touch anything another user put there.
pub fn bind(path: &Path) -> io::Result<UnixListener> {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    paths::ensure_socket_dir(dir, paths::uid())?;
    if fs::symlink_metadata(path).is_ok() {
        paths::check_socket_owner(path, paths::uid()).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("{e}; set ZJ_HERDR_SOCKET or XDG_RUNTIME_DIR to use another path"),
            )
        })?;
        if UnixStream::connect(path).is_ok() {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("a daemon is already listening on {}", path.display()),
            ));
        }
        fs::remove_file(path)?;
    }
    // Bind inside a fresh 0700 directory, make the socket 0600, then link it
    // into place: it is never reachable with looser permissions, and the
    // process umask (shared by every thread) is left alone. The link fails
    // if another daemon bound `path` meanwhile, as a bind would.
    let staging = make_staging_dir(dir)?;
    let staged = staging.join("s");
    let linked = UnixListener::bind(&staged).and_then(|listener| {
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600))?;
        link_socket(&staged, path)?;
        Ok(listener)
    });
    let _ = fs::remove_dir_all(&staging);
    linked
}

/// Hard-link the staged socket to `path`, failing like a bind would if
/// another daemon got there first.
fn link_socket(staged: &Path, path: &Path) -> io::Result<()> {
    fs::hard_link(staged, path).map_err(|e| {
        if e.kind() == io::ErrorKind::AlreadyExists {
            io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("a daemon is already listening on {}", path.display()),
            )
        } else {
            e
        }
    })
}

/// A new 0700 directory in `dir` with an unpredictable, short name (the
/// socket path inside it must stay under the ~104-byte limit). In a shared
/// `/tmp` another user may squat names, so a taken one is skipped.
fn make_staging_dir(dir: &Path) -> io::Result<PathBuf> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
        ^ (u64::from(std::process::id()) << 32);
    make_staging_dir_seeded(dir, seed)
}

fn staging_candidate(dir: &Path, seed: u64, n: u64) -> PathBuf {
    dir.join(format!(
        ".zjh{:x}",
        seed.wrapping_mul(n + 1) & 0xff_ffff_ffff
    ))
}

fn make_staging_dir_seeded(dir: &Path, seed: u64) -> io::Result<PathBuf> {
    for n in 0..16u64 {
        let candidate = staging_candidate(dir, seed, n);
        match fs::DirBuilder::new().mode(0o700).create(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        format!("no free staging directory in {}", dir.display()),
    ))
}

/// The uid of the process at the other end of `stream`.
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let fd = stream.as_raw_fd();
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` and `len` are valid for writes of the sizes given.
        let rc = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut cred as *mut libc::ucred).cast(),
                &mut len,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(cred.uid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    {
        let (mut uid, mut gid) = (0, 0);
        // SAFETY: `uid` and `gid` are valid for writes.
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(uid)
    }
}

/// Only the user running the daemon may drive it, whatever the file mode.
pub fn authorize(stream: &UnixStream, uid: u32) -> io::Result<()> {
    match peer_uid(stream)? {
        peer if peer == uid => Ok(()),
        peer => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refused connection from uid {peer}"),
        )),
    }
}

pub fn serve(listener: UnixListener, ctx: Arc<Context>) {
    for stream in listener.incoming() {
        accept(stream, &ctx);
    }
}

/// Serve one accepted connection on its own thread, or report why none came.
pub fn accept(
    stream: io::Result<UnixStream>,
    ctx: &Arc<Context>,
) -> Option<thread::JoinHandle<()>> {
    match stream {
        Ok(stream) => {
            let ctx = Arc::clone(ctx);
            Some(thread::spawn(move || {
                if let Err(e) = handle(stream, &ctx) {
                    if e.kind() != io::ErrorKind::BrokenPipe {
                        eprintln!("zj-herdr: client: {e}");
                    }
                }
            }))
        }
        Err(e) => {
            eprintln!("zj-herdr: accept: {e}");
            None
        }
    }
}

/// Serve one client connection until it closes.
pub fn handle(stream: UnixStream, ctx: &Context) -> io::Result<()> {
    authorize(&stream, paths::uid())?;
    let mut writer = stream.try_clone()?;
    for line in BufReader::new(stream).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match api::parse_request(&line) {
            Ok(Request::Subscribe) => return stream_events(writer, ctx),
            Ok(req) => writer.write_all(api::to_line(&respond(ctx, req)).as_bytes())?,
            Err(e) => writer.write_all(api::to_line(&Response::error(e)).as_bytes())?,
        }
    }
    Ok(())
}

fn stream_events(mut writer: UnixStream, ctx: &Context) -> io::Result<()> {
    let (tx, rx) = mpsc::channel();
    ctx.registry.subscribe(tx);
    for line in rx {
        writer.write_all(line.as_bytes())?;
    }
    Ok(())
}

/// Answer one request (everything but `subscribe`).
pub fn respond(ctx: &Context, req: Request) -> Response {
    match req {
        Request::List => Response {
            state: Some(ctx.registry.snapshot()),
            ..Response::ok()
        },
        Request::Wait {
            agent,
            state,
            timeout,
        } => {
            let timeout = match timeout.map(Duration::try_from_secs_f64).transpose() {
                Ok(t) => t,
                Err(_) => {
                    return Response::error("timeout must be a non-negative number of seconds")
                }
            };
            match ctx.registry.wait_for(&agent, state, timeout, &ctx.machine) {
                Ok(agent) => Response {
                    agent: Some(agent),
                    ..Response::ok()
                },
                Err(e) => Response::error(e),
            }
        }
        Request::Prompt {
            agent,
            text,
            submit,
        } => match prompt(ctx, &agent, text, submit) {
            Ok(()) => Response::ok(),
            Err(e) => Response::error(e),
        },
        Request::Spawn {
            session,
            cwd,
            command,
        } => match ctx.zellij.run(&session, cwd.as_deref(), &command.argv()) {
            Ok(()) => Response::ok(),
            Err(e) => Response::error(e),
        },
        Request::Subscribe => {
            Response::error("subscribe streams events; send it on its own connection")
        }
        Request::Ack { agent } => match ctx.registry.ack(&agent, &ctx.machine) {
            Ok(agent) => Response {
                agent: Some(agent),
                ..Response::ok()
            },
            Err(e) => Response::error(e),
        },
        Request::Adopt { agent, session } => match adopt_agent(ctx, &agent, &session) {
            Ok(agent) => Response {
                agent: Some(agent),
                ..Response::ok()
            },
            Err(e) => Response::error(e),
        },
    }
}

/// [`adopt::adopt`] the running local agent `selector` names.
fn adopt_agent(ctx: &Context, selector: &str, session: &str) -> Result<Agent, String> {
    let matches: Vec<Agent> = ctx
        .registry
        .find_all(selector)
        .into_iter()
        .filter(|a| a.state != AgentState::Done)
        .collect();
    let agent = matches
        .iter()
        .find(|a| a.machine == ctx.machine)
        .or(matches.first())
        .cloned()
        .ok_or_else(|| format!("no running agent matches {selector:?}"))?;
    if agent.machine != ctx.machine {
        return Err(format!(
            "{} is on {}; only local agents can be adopted",
            agent.id, agent.machine
        ));
    }
    if agent.background {
        return Err(format!(
            "{} is a background session of the Claude Code daemon; a client shows it, so it is not stopped",
            agent.id
        ));
    }
    let lock = || crate::sync::lock(&ctx.adopting);
    if !lock().insert(agent.pid) {
        return Err(format!("{} is already being moved", agent.id));
    }
    let result = adopt::adopt(
        &agent,
        session,
        ctx.zellij.as_ref(),
        ctx.procs.as_ref(),
        ctx.adopt_timeout,
    );
    lock().remove(&agent.pid);
    result.map(|_| agent)
}

fn prompt(ctx: &Context, selector: &str, mut text: String, submit: bool) -> Result<(), String> {
    // Pane selectors like `dev:3` can exist on several machines; mean ours.
    // An exited agent is kept as `done` for a while, but its pane may now
    // hold a shell or another program, so it is never a prompt target.
    let matches: Vec<_> = ctx
        .registry
        .find_all(selector)
        .into_iter()
        .filter(|a| a.state != AgentState::Done)
        .collect();
    let agent = matches
        .iter()
        .find(|a| a.machine == ctx.machine)
        .or(matches.first())
        .cloned()
        .ok_or_else(|| format!("no running agent matches {selector:?}"))?;
    if agent.machine != ctx.machine {
        return Err(format!(
            "{} is on {}; prompting remote agents is not supported",
            agent.id, agent.machine
        ));
    }
    let pane = agent
        .pane
        .ok_or_else(|| format!("{} is not running inside Zellij", agent.id))?;
    if submit {
        text.push('\r');
    }
    let cmd = PluginCommand::Prompt {
        pane_id: pane.pane_id,
        text,
    };
    // Addressed to the plugin itself so Zellij loads it in a session that
    // has not opened the picker yet; only its acknowledgement counts.
    let reply = ctx
        .zellij
        .deliver(&pane.session, &ctx.plugin, CMD_SCHEMA, &cmd.to_json())?;
    prompt_reply(&reply).map_err(|e| {
        format!(
            "prompt to {} in session {} not delivered: {e}",
            agent.id, pane.session
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_too_long_to_link_is_a_plain_error_and_a_taken_one_is_in_use() {
        let tmp = tempfile::tempdir().unwrap();
        let staged = tmp.path().join("staged");
        fs::write(&staged, "").unwrap();
        let taken = tmp.path().join("taken");
        fs::write(&taken, "").unwrap();
        let err = link_socket(&staged, &taken).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        let long = tmp.path().join("x".repeat(300));
        let err = link_socket(&staged, &long).unwrap_err();
        assert_ne!(err.kind(), io::ErrorKind::AddrInUse);
        link_socket(&staged, &tmp.path().join("free")).unwrap();
    }

    #[test]
    fn staging_names_that_are_taken_are_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let (dir, seed) = (tmp.path(), 7);
        fs::create_dir(staging_candidate(dir, seed, 0)).unwrap();
        let got = make_staging_dir_seeded(dir, seed).unwrap();
        assert_eq!(got, staging_candidate(dir, seed, 1));
        for n in 2..16 {
            fs::create_dir(staging_candidate(dir, seed, n)).unwrap();
        }
        let err = make_staging_dir_seeded(dir, seed).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(
            err.to_string().contains("no free staging directory"),
            "{err}"
        );
        let missing = make_staging_dir_seeded(&dir.join("nope"), seed).unwrap_err();
        assert_eq!(missing.kind(), io::ErrorKind::NotFound);
    }
}
