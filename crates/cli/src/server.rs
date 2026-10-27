//! The Unix socket server.

use std::fs;
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use zj_herdr_core::{prompt_reply, AgentState, PluginCommand, CMD_SCHEMA};

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
        fs::hard_link(&staged, path).map_err(|e| {
            if e.kind() == io::ErrorKind::AlreadyExists {
                io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("a daemon is already listening on {}", path.display()),
                )
            } else {
                e
            }
        })?;
        Ok(listener)
    });
    let _ = fs::remove_dir_all(&staging);
    linked
}

/// A new 0700 directory in `dir` with an unpredictable, short name (the
/// socket path inside it must stay under the ~104-byte limit). In a shared
/// `/tmp` another user may squat names, so a taken one is skipped.
fn make_staging_dir(dir: &Path) -> io::Result<PathBuf> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos() as u64)
        ^ (u64::from(std::process::id()) << 32);
    for n in 0..16u64 {
        let candidate = dir.join(format!(
            ".zjh{:x}",
            seed.wrapping_mul(n + 1) & 0xff_ffff_ffff
        ));
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
        match stream {
            Ok(stream) => {
                let ctx = Arc::clone(&ctx);
                thread::spawn(move || {
                    if let Err(e) = handle(stream, &ctx) {
                        if e.kind() != io::ErrorKind::BrokenPipe {
                            eprintln!("zj-herdr: client: {e}");
                        }
                    }
                });
            }
            Err(e) => eprintln!("zj-herdr: accept: {e}"),
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
    }
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
