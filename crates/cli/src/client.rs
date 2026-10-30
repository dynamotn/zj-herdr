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
    verify_peer(stream, path, paths::uid())
}

fn verify_peer(stream: UnixStream, path: &Path, uid: u32) -> Result<UnixStream, String> {
    check_peer(&stream, uid).map_err(|e| format!("{}: {e}", path.display()))?;
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
    stream: UnixStream,
    mut on_event: impl FnMut(Event) -> bool,
) -> Result<(), String> {
    subscribe_dyn(stream, &mut on_event)
}

// Not generic, so that coverage sees a single copy of the loop.
fn subscribe_dyn(
    mut stream: UnixStream,
    on_event: &mut dyn FnMut(Event) -> bool,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::OwnedFd;

    /// A client stream whose peer writes `reply` and stops writing; the peer is
    /// returned so the client's own writes still land.
    fn replying(reply: impl AsRef<[u8]>) -> (UnixStream, UnixStream) {
        let (client, mut server_end) = UnixStream::pair().unwrap();
        server_end.write_all(reply.as_ref()).unwrap();
        server_end.shutdown(std::net::Shutdown::Write).unwrap();
        (client, server_end)
    }

    #[test]
    fn a_peer_that_cannot_be_identified_is_refused() {
        let devnull = OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
        let err = check_peer(&UnixStream::from(devnull), paths::uid()).unwrap_err();
        assert!(err.contains("cannot check who is listening"), "{err}");
    }

    #[test]
    fn connect_refuses_a_path_that_is_not_our_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let plain = tmp.path().join("plain");
        std::fs::write(&plain, "").unwrap();
        assert!(connect(&plain).is_err());
    }

    #[test]
    fn a_listener_running_as_someone_else_is_refused() {
        let (stream, _peer) = UnixStream::pair().unwrap();
        let path = Path::new("/run/zj-herdr.sock");
        let err = verify_peer(stream, path, paths::uid().wrapping_add(1)).unwrap_err();
        assert!(err.starts_with("/run/zj-herdr.sock: "), "{err}");
        let (stream, _peer) = UnixStream::pair().unwrap();
        assert!(verify_peer(stream, path, paths::uid()).is_ok());
    }

    #[test]
    fn io_errors_on_the_stream_are_reported() {
        // The daemon is gone: writing fails.
        let (gone, peer) = UnixStream::pair().unwrap();
        drop(peer);
        let err = request_on(gone, &Request::List).unwrap_err();
        assert!(err.contains("Broken pipe"), "{err}");
        let (gone, peer) = UnixStream::pair().unwrap();
        drop(peer);
        let err = subscribe_on(gone, |_| true).unwrap_err();
        assert!(err.contains("Broken pipe"), "{err}");
        // The daemon answers with bytes that are not text: reading fails.
        let err = request_on(replying(b"\xff\n").0, &Request::List).unwrap_err();
        assert!(err.contains("UTF-8"), "{err}");
        let err = subscribe_on(replying(b"\xff\n").0, |_| true).unwrap_err();
        assert!(err.contains("UTF-8"), "{err}");
    }

    #[test]
    fn request_fails_when_the_daemon_hangs_up_or_garbles_its_answer() {
        let err = request_on(replying("").0, &Request::List).unwrap_err();
        assert!(err.contains("closed the connection"), "{err}");
        let err = request_on(replying("junk\n").0, &Request::List).unwrap_err();
        assert!(err.contains("bad response"), "{err}");
    }

    #[test]
    fn subscribe_hands_events_over_until_told_to_stop() {
        let line = api::to_line(&Event::Gone { id: "m/1".into() });
        let mut seen = 0;
        let got = subscribe_on(replying(format!("{line}{line}")).0, |_| {
            seen += 1;
            seen < 2
        });
        assert_eq!((got, seen), (Ok(()), 2));
    }

    #[test]
    fn subscribe_reports_an_error_reply_a_garbled_line_and_a_hangup() {
        let err = subscribe_on(replying("{\"ok\":false,\"error\":\"nope\"}\n").0, |_| true);
        assert_eq!(err, Err("nope".into()));
        let err = subscribe_on(replying("junk\n").0, |_| true).unwrap_err();
        assert!(err.contains("bad event"), "{err}");
        // An ok reply is not an event; keep reading until the hangup.
        let err = subscribe_on(replying("{\"ok\":true}\n").0, |_| true).unwrap_err();
        assert!(err.contains("closed the connection"), "{err}");
    }
}
