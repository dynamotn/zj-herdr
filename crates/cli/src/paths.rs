//! Where zj-herdr keeps its socket, state and config.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Path, PathBuf};

fn env(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.is_empty())
}

fn home(get: &impl Fn(&str) -> Option<String>) -> PathBuf {
    get("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

pub fn uid() -> u32 {
    // SAFETY: getuid has no preconditions and cannot fail.
    unsafe { libc::getuid() }
}

/// `$ZJ_HERDR_SOCKET`, else `$XDG_RUNTIME_DIR/zj-herdr.sock`, else
/// `$TMPDIR/zj-herdr-$UID.sock`. `$TMPDIR` may be the shared `/tmp`; see
/// [`check_socket_dir`] for why that is still safe.
pub fn socket_path_from(get: impl Fn(&str) -> Option<String>, uid: u32) -> PathBuf {
    if let Some(path) = get("ZJ_HERDR_SOCKET") {
        return PathBuf::from(path);
    }
    if let Some(dir) = get("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("zj-herdr.sock");
    }
    let tmp = get("TMPDIR").unwrap_or_else(|| "/tmp".to_string());
    PathBuf::from(tmp).join(format!("zj-herdr-{uid}.sock"))
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

/// A directory the socket may live in: a private one, or a shared one with
/// the sticky bit (`/tmp`), where other users cannot remove or replace a
/// file we own. A socket another user planted there first is refused by
/// [`check_socket_owner`] on both ends.
pub fn check_socket_dir(dir: &Path, uid: u32) -> io::Result<()> {
    let private = check_private_dir(dir, uid);
    if private.is_ok() {
        return private;
    }
    let trusted = |m: &fs::Metadata| m.uid() == 0 || m.uid() == uid;
    let mut meta = fs::symlink_metadata(dir)?;
    // macOS's `/tmp` is a root-owned link to `/private/tmp`; follow a link
    // only when root or we own it.
    if meta.file_type().is_symlink() && trusted(&meta) {
        meta = fs::metadata(dir)?;
    }
    if meta.file_type().is_dir() && trusted(&meta) && meta.mode() & 0o1000 != 0 {
        Ok(())
    } else {
        private
    }
}

/// An existing socket path must be a socket we own.
pub fn check_socket_owner(path: &Path, uid: u32) -> io::Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let meta = fs::symlink_metadata(path)?;
    let reason = if !meta.file_type().is_socket() {
        Some("is not a socket".to_string())
    } else if meta.uid() != uid {
        Some(format!("is owned by uid {}", meta.uid()))
    } else {
        None
    };
    match reason {
        Some(r) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} {r}; refusing it", path.display()),
        )),
        None => Ok(()),
    }
}

/// Create `dir` with mode 0700 if missing, then [`check_socket_dir`] it.
pub fn ensure_socket_dir(dir: &Path, uid: u32) -> io::Result<()> {
    match fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    check_socket_dir(dir, uid)
}

pub fn socket_path() -> PathBuf {
    socket_path_from(env, uid())
}

/// `$XDG_STATE_HOME/zj-herdr`, else `~/.local/state/zj-herdr`.
pub fn state_dir() -> PathBuf {
    state_dir_from(env)
}

fn state_dir_from(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    get("XDG_STATE_HOME")
        .map_or_else(|| home(&get).join(".local/state"), PathBuf::from)
        .join("zj-herdr")
}

/// `$XDG_CONFIG_HOME/zj-herdr/config.toml`, else `~/.config/zj-herdr/config.toml`.
pub fn config_path() -> PathBuf {
    config_path_from(env)
}

fn config_path_from(get: impl Fn(&str) -> Option<String>) -> PathBuf {
    get("XDG_CONFIG_HOME")
        .map_or_else(|| home(&get).join(".config"), PathBuf::from)
        .join("zj-herdr/config.toml")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_precedence() {
        let vars = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            socket_path_from(
                vars(&[("ZJ_HERDR_SOCKET", "/s"), ("XDG_RUNTIME_DIR", "/run/u")]),
                1
            ),
            PathBuf::from("/s")
        );
        assert_eq!(
            socket_path_from(vars(&[("XDG_RUNTIME_DIR", "/run/u"), ("TMPDIR", "/t")]), 1),
            PathBuf::from("/run/u/zj-herdr.sock")
        );
        assert_eq!(
            socket_path_from(vars(&[("TMPDIR", "/t/")]), 501),
            PathBuf::from("/t/zj-herdr-501.sock")
        );
        assert_eq!(
            socket_path_from(vars(&[]), 7),
            PathBuf::from("/tmp/zj-herdr-7.sock")
        );
    }

    #[test]
    fn state_and_config_follow_xdg_then_home() {
        let vars = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            state_dir_from(vars(&[("XDG_STATE_HOME", "/x"), ("HOME", "/h")])),
            PathBuf::from("/x/zj-herdr")
        );
        assert_eq!(
            state_dir_from(vars(&[("HOME", "/h")])),
            PathBuf::from("/h/.local/state/zj-herdr")
        );
        assert_eq!(
            config_path_from(vars(&[("XDG_CONFIG_HOME", "/c")])),
            PathBuf::from("/c/zj-herdr/config.toml")
        );
        assert_eq!(
            config_path_from(vars(&[])),
            PathBuf::from("/.config/zj-herdr/config.toml")
        );
        // The real-environment wrappers agree on the fixed suffixes.
        assert!(state_dir().ends_with("zj-herdr"));
        assert!(config_path().ends_with("zj-herdr/config.toml"));
        assert!(socket_path().to_string_lossy().contains("zj-herdr"));
    }

    #[test]
    fn ensure_socket_dir_reports_a_missing_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("no/such/parent");
        assert_eq!(
            ensure_socket_dir(&dir, uid()).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn private_dir_is_created_0700_and_checked() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("zj-herdr-1");
        ensure_socket_dir(&dir, uid()).unwrap();
        assert_eq!(fs::metadata(&dir).unwrap().mode() & 0o777, 0o700);
        ensure_socket_dir(&dir, uid()).unwrap();

        assert!(
            check_private_dir(&dir, uid().wrapping_add(1)).is_err(),
            "foreign owner accepted"
        );

        let open = tmp.path().join("open");
        fs::create_dir(&open).unwrap();
        fs::set_permissions(&open, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(
            check_private_dir(&open, uid()).is_err(),
            "world-writable dir accepted"
        );

        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        assert!(check_private_dir(&link, uid()).is_err(), "symlink accepted");
        assert!(
            check_socket_dir(&open, uid()).is_err(),
            "non-sticky shared dir accepted"
        );

        let sticky = tmp.path().join("sticky");
        fs::create_dir(&sticky).unwrap();
        fs::set_permissions(&sticky, fs::Permissions::from_mode(0o1777)).unwrap();
        check_socket_dir(&sticky, uid()).unwrap();
    }
}
