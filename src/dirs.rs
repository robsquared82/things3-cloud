use std::{
    fs::{self, File},
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const APP_NAME: &str = "things3";
const LEGACY_APP_NAME: &str = "things-cli";

fn state_home() -> PathBuf {
    dirs::state_dir()
        .or_else(dirs::data_local_dir)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// The pre-`dirs`-crate location of the app's state directory. Always the
/// Linux XDG path, even on macOS/Windows, so that users migrating from an
/// earlier Unix build still get their legacy directory picked up.
fn legacy_state_home() -> PathBuf {
    if let Ok(custom) = std::env::var("XDG_STATE_HOME") {
        return PathBuf::from(custom);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("state")
}

pub fn app_state_dir() -> PathBuf {
    let target = state_home().join(APP_NAME);
    let legacy = legacy_state_home().join(LEGACY_APP_NAME);

    if target.exists() || !legacy.exists() {
        return target;
    }

    if fs::rename(&legacy, &target).is_ok() {
        return target;
    }

    target
}

pub fn append_log_dir() -> PathBuf {
    app_state_dir().join("append-log")
}

pub fn auth_file_path() -> PathBuf {
    app_state_dir().join("auth.json")
}

/// Create `dir` and narrow it to owner-only. The state directory holds the
/// Things Cloud password and an append log carrying every task title and note,
/// so the default 0755 leaves that readable by any other local user.
pub fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Write `bytes` to `path` as mode 0600 without following a symlink at the
/// staging path or replacing the final path through a symlink.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        create_private_dir(parent)?;
    }
    refuse_symlink(path)?;

    let tmp = path.with_extension("tmp");
    let _ = fs::remove_file(&tmp);
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
        opts.custom_flags(o_nofollow());
    }
    let mut file = opts.open(&tmp)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
    }
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Append to `path`, creating it mode 0600. A symlink at `path` is refused.
pub fn open_private_append(path: &Path) -> io::Result<File> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        create_private_dir(parent)?;
    }
    refuse_symlink(path)?;

    let mut opts = fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
        opts.custom_flags(o_nofollow());
    }
    let file = opts.open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Open `path` for reading. A symlink is refused.
pub fn open_private_read(path: &Path) -> io::Result<File> {
    refuse_symlink(path)?;
    let mut opts = fs::OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(o_nofollow());
    }
    opts.open(path)
}

/// Read `path` to a string. `Ok(None)` means the file is absent.
/// A symlink is an error, so a planted link cannot supply a history key.
pub fn read_private_to_string(path: &Path) -> io::Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("refusing to follow symlink at {}", path.display()),
            ));
        }
        Ok(_) => {}
    }
    let mut file = open_private_read(path)?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(Some(buf))
}

fn refuse_symlink(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("refusing to follow symlink at {}", path.display()),
        )),
        _ => Ok(()),
    }
}

#[cfg(unix)]
fn o_nofollow() -> i32 {
    #[cfg(target_os = "linux")]
    {
        0o400000
    }
    #[cfg(target_os = "macos")]
    {
        0x0100
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode_of(path: &Path) -> u32 {
        fs::metadata(path).expect("metadata").permissions().mode() & 0o777
    }

    #[test]
    fn creates_the_directory_private() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("things3").join("append-log");
        create_private_dir(&dir).expect("create");
        assert_eq!(mode_of(&dir), 0o700);
    }

    #[test]
    fn private_write_does_not_follow_a_symlink() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("cursor.json");
        let victim = tmp.path().join("victim");
        fs::write(&victim, "original").expect("seed victim");
        std::os::unix::fs::symlink(&victim, path.with_extension("tmp")).expect("plant symlink");

        write_private_atomic(&path, b"{\"history_key\":\"secret\"}").expect("write");

        assert_eq!(fs::read_to_string(&victim).expect("victim"), "original");
        assert_eq!(mode_of(&path), 0o600);
    }

    #[test]
    fn narrows_an_existing_world_readable_directory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("things3");
        fs::create_dir_all(&dir).expect("seed");
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).expect("widen");
        create_private_dir(&dir).expect("create");
        assert_eq!(mode_of(&dir), 0o700);
    }
}
