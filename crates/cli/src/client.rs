//! Talk to a running daemon over its socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use crate::api::{self, Event, Request, Response};
use crate::paths;

pub fn connect(path: &Path) -> Result<UnixStream, String> {
    // Never hand prompts to a socket another user planted.
    if std::fs::symlink_metadata(path).is_ok() {
        paths::check_socket_owner(path, paths::uid()).map_err(|e| e.to_string())?;
    }
    let stream = UnixStream::connect(path).map_err(|e| {
        format!(
            "cannot reach the daemon at {} ({e}); start it with `zj-herdr daemon`",
            path.display()
        )
    })?;
    // The path check above races with whoever binds the path next; the
    // peer's credentials do not.
    check_peer(&stream, paths::uid()).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(stream)
}

/// Refuse a listener that does not run as `uid`.
pub fn check_peer(stream: &UnixStream, uid: u32) -> Result<(), String> {
    match crate::server::peer_uid(stream) {
        Ok(peer) if peer == uid => Ok(()),
        Ok(peer) => Err(format!(
            "the listener runs as uid {peer}, not us; refusing it"
        )),
        Err(e) => Err(format!("cannot check who is listening ({e})")),
    }
}

/// Send one request and read its response.
pub fn request(path: &Path, req: &Request) -> Result<Response, String> {
    request_on(connect(path)?, req)
}

/// [`request`] over an already connected stream.
pub fn request_on(mut stream: UnixStream, req: &Request) -> Result<Response, String> {
    stream
        .write_all(api::to_line(req).as_bytes())
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if line.is_empty() {
        return Err("the daemon closed the connection".into());
    }
    serde_json::from_str::<Response>(&line)
        .map_err(|e| format!("bad response: {e}"))?
        .into_result()
}

/// Subscribe and call `on_event` for each event until it returns false.
pub fn subscribe(path: &Path, on_event: impl FnMut(Event) -> bool) -> Result<(), String> {
    subscribe_on(connect(path)?, on_event)
}

/// [`subscribe`] over an already connected stream.
pub fn subscribe_on(
    mut stream: UnixStream,
    mut on_event: impl FnMut(Event) -> bool,
) -> Result<(), String> {
    stream
        .write_all(api::to_line(&Request::Subscribe).as_bytes())
        .map_err(|e| e.to_string())?;
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|e| e.to_string())?;
        match serde_json::from_str::<Event>(&line) {
            Ok(event) => {
                if !on_event(event) {
                    return Ok(());
                }
            }
            Err(_) => {
                let resp: Response =
                    serde_json::from_str(&line).map_err(|e| format!("bad event: {e}"))?;
                resp.into_result()?;
            }
        }
    }
    Err("the daemon closed the connection".into())
}
