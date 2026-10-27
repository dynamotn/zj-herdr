//! Where zj-herdr keeps its socket, state and config.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn home() -> PathBuf {
    env("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

pub fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// `$ZJ_HERDR_SOCKET`, else `$XDG_RUNTIME_DIR/zj-herdr.sock`, else
/// `$TMPDIR/zj-herdr-$UID/zj-herdr.sock`. The last one lives in its own
/// directory because `$TMPDIR` may be the shared `/tmp`, where a bare
/// socket name could be claimed first by another user.
pub fn socket_path_from(get: impl Fn(&str) -> Option<String>, uid: u32) -> PathBuf {
    if let Some(path) = get("ZJ_HERDR_SOCKET") {
        return PathBuf::from(path);
    }
    if let Some(dir) = get("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("zj-herdr.sock");
    }
    let tmp = get("TMPDIR").unwrap_or_else(|| "/tmp".to_string());
    PathBuf::from(tmp).join(format!("zj-herdr-{uid}")).join("zj-herdr.sock")
}

/// Refuse a socket directory another user could tamper with: it must be a
/// real directory (not a symlink), owned by us, and not group- or
/// world-writable.
pub fn check_private_dir(dir: &Path, uid: u32) -> io::Result<()> {
    let meta = fs::symlink_metadata(dir)?;
    let reason = if !meta.file_type().is_dir() {
        Some("is not a directory")
    } else if meta.uid() != uid {
        Some("is owned by another user")
    } else if meta.mode() & 0o022 != 0 {
        Some("is writable by other users")
    } else {
        None
    };
    match reason {
        Some(r) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} {r}; refusing to use it for the socket", dir.display()),
        )),
        None => Ok(()),
    }
}

/// Create `dir` with mode 0700 if missing, then [`check_private_dir`] it.
pub fn ensure_private_dir(dir: &Path, uid: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    check_private_dir(dir, uid)
}

pub fn socket_path() -> PathBuf {
    socket_path_from(env, uid())
}

/// `$XDG_STATE_HOME/zj-herdr`, else `~/.local/state/zj-herdr`.
pub fn state_dir() -> PathBuf {
    env("XDG_STATE_HOME")
        .map_or_else(|| home().join(".local/state"), PathBuf::from)
        .join("zj-herdr")
}

/// `$XDG_CONFIG_HOME/zj-herdr/config.toml`, else `~/.config/zj-herdr/config.toml`.
pub fn config_path() -> PathBuf {
    env("XDG_CONFIG_HOME")
        .map_or_else(|| home().join(".config"), PathBuf::from)
        .join("zj-herdr/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_precedence() {
        let vars = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| pairs.iter().find(|(key, _)| *key == k).map(|(_, v)| v.to_string())
        };
        assert_eq!(
            socket_path_from(vars(&[("ZJ_HERDR_SOCKET", "/s"), ("XDG_RUNTIME_DIR", "/run/u")]), 1),
            PathBuf::from("/s")
        );
        assert_eq!(
            socket_path_from(vars(&[("XDG_RUNTIME_DIR", "/run/u"), ("TMPDIR", "/t")]), 1),
            PathBuf::from("/run/u/zj-herdr.sock")
        );
        assert_eq!(
            socket_path_from(vars(&[("TMPDIR", "/t/")]), 501),
            PathBuf::from("/t/zj-herdr-501/zj-herdr.sock")
        );
        assert_eq!(
            socket_path_from(vars(&[]), 7),
            PathBuf::from("/tmp/zj-herdr-7/zj-herdr.sock")
        );
    }

    #[test]
    fn private_dir_is_created_0700_and_checked() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("zj-herdr-1");
        ensure_private_dir(&dir, uid()).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        ensure_private_dir(&dir, uid()).unwrap();

        assert!(
            check_private_dir(&dir, uid().wrapping_add(1)).is_err(),
            "foreign owner accepted"
        );

        let open = tmp.path().join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(check_private_dir(&open, uid()).is_err(), "world-writable dir accepted");

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(check_private_dir(&link, uid()).is_err(), "symlink accepted");
    }
}
