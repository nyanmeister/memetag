//! XDG locations shared by clients and server helpers.
use std::path::{Path, PathBuf};

fn xdg(key: &str, fallback: &str) -> PathBuf {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(fallback)
        })
        .join("memetag")
}
pub fn config_dir() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config")
}
pub fn data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share")
}
pub fn cache_dir() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache")
}

/// `config.toml` as one table, read fresh each call (tests point XDG_CONFIG_HOME at their own directory, so nothing
/// is cached per process). A missing file is an empty table; a file that does not parse is an error naming it, so a
/// typo never silently turns into the defaults. Every reader of the file goes through here.
pub fn config_table() -> Result<toml::Table, String> {
    let path = config_dir().join("config.toml");
    match std::fs::read_to_string(&path) {
        Ok(text) => text.parse().map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// The file's bytes, or `None` when it does not exist; any other error is reported with the path.
pub fn read_opt(path: &Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read {}: {e}", path.display())),
    }
}

/// Publish `bytes` at `path` all at once: a sibling temporary file (unique to this process) is written, synced
/// and renamed over the target, and the directory is synced, so a reader sees the old file or the new one and a
/// crash leaves no half-written config. The temporary is removed on any failure.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| format!("{}: no parent directory", path.display()))?;
    std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let tmp = path.with_extension(format!("{}.tmp", std::process::id()));
    let result = (|| -> Result<(), String> {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        f.write_all(bytes)
            .and_then(|_| f.sync_all())
            .map_err(|e| format!("write {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| format!("replace {}: {e}", path.display()))?;
        std::fs::File::open(parent)
            .and_then(|d| d.sync_all())
            .map_err(|e| format!("sync {}: {e}", parent.display()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// An optimistic save of a file several windows may edit: under the sidecar lock, refuse with `stale` when the
/// file no longer holds the bytes this editor started from (`original`, `None` for "did not exist"), else
/// publish `bytes` atomically. The caller records `bytes` as its new original on success.
pub fn save_guarded(
    path: &Path,
    lock: &Path,
    original: &Option<Vec<u8>>,
    bytes: &[u8],
    stale: &str,
) -> Result<(), String> {
    let _lock = crate::locking::sidecar(lock)?;
    if read_opt(path)? != *original {
        return Err(stale.into());
    }
    atomic_write(path, bytes)
}

/// The path without its `.` components (`./a/./b` → `a/b`); `.` alone becomes empty.
pub fn clean(path: &Path) -> PathBuf {
    path.components()
        .filter(|c| *c != std::path::Component::CurDir)
        .collect()
}

/// `root/rel` resolved and proven to lie under `root`, the check every worker makes before touching a file
/// named by a request. `rel` must already be a plain relative path (see `sources::relative`).
pub fn within(root: &Path, rel: &Path) -> Result<PathBuf, String> {
    let path = root.join(rel).canonicalize().map_err(|e| e.to_string())?;
    if !path.starts_with(root.canonicalize().map_err(|e| e.to_string())?) {
        return Err("File resolves outside the collection".into());
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("memetag-paths-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_publishes_whole_files_and_leaves_no_temporary() {
        let dir = scratch("atomic");
        let path = dir.join("deep/er/file.toml");
        atomic_write(&path, b"one\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"one\n");
        atomic_write(&path, b"two\n").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"two\n");
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(left, vec!["file.toml"]);
        assert_eq!(read_opt(&path).unwrap().as_deref(), Some(&b"two\n"[..]));
        assert_eq!(read_opt(&dir.join("absent")).unwrap(), None);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn save_guarded_refuses_a_file_that_moved_under_the_editor() {
        let dir = scratch("guard");
        let (path, lock) = (dir.join("c.toml"), dir.join("c.lock"));
        // created fresh: the editor started from "no file"
        save_guarded(&path, &lock, &None, b"a = 1\n", "stale").unwrap();
        // the editor that started from "a = 1" saves over it
        let original = Some(b"a = 1\n".to_vec());
        save_guarded(&path, &lock, &original, b"a = 2\n", "stale").unwrap();
        // an editor still holding "a = 1" is refused with the caller's message, and the file is untouched
        assert_eq!(
            save_guarded(&path, &lock, &original, b"a = 3\n", "stale: reopen").unwrap_err(),
            "stale: reopen"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"a = 2\n");
        // an editor that started from "no file" is refused once the file exists
        assert!(save_guarded(&path, &lock, &None, b"x", "stale").is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn clean_drops_dot_components_and_within_keeps_requests_inside_the_root() {
        assert_eq!(clean(Path::new("./a/./b")), PathBuf::from("a/b"));
        assert_eq!(clean(Path::new(".")), PathBuf::new());
        assert_eq!(clean(Path::new("a/../b")), PathBuf::from("a/../b"));
        let root = scratch("within");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("sub/f.png"), b"x").unwrap();
        std::fs::write(root.with_extension("outside"), b"y").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(root.with_extension("outside"), root.join("sub/link")).unwrap();
        assert!(within(&root, Path::new("sub/f.png"))
            .unwrap()
            .ends_with("sub/f.png"));
        assert_eq!(
            within(&root, Path::new("sub/link")).unwrap_err(),
            "File resolves outside the collection"
        );
        assert!(within(&root, Path::new("sub/missing")).is_err());
        let _ = std::fs::remove_file(root.with_extension("outside"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
