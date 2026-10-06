//! Nonblocking advisory locks. Sidecar lock files are permanent: unlinking a locked inode lets a second
//! process create a different inode and acquire a second, unrelated lock for the same resource.
use std::{
    fs::{File, OpenOptions},
    os::fd::AsRawFd,
    path::Path,
};

#[must_use = "the guard must stay alive for the entire protected operation"]
pub struct Lock {
    file: File,
}
impl Drop for Lock {
    fn drop(&mut self) {
        // Explicit unlock matters: a concurrently forked child can inherit a descriptor until exec.
        // Merely closing ours would leave that child's reference keeping the lock alive.
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub fn try_lock(file: &File) -> Result<Option<Lock>, String> {
    let file = file.try_clone().map_err(|e| e.to_string())?;
    // SAFETY: File owns the descriptor and flock does not access Rust memory.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Ok(Some(Lock { file }))
    } else {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(format!("locking unavailable: {err}"))
        }
    }
}

pub fn lock_file(file: &File) -> Result<Lock, String> {
    try_lock(file)?.ok_or_else(|| "resource busy".into())
}

pub fn sidecar(path: &Path) -> Result<Lock, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|e| format!("lock {}: {e}", path.display()))?;
    lock_file(&file).map_err(|e| {
        format!(
            "{}: {e}; retry after the other operation finishes",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dropping_guard_unlocks_even_with_an_inherited_descriptor() {
        let path =
            std::env::temp_dir().join(format!("memetag-inherited-lock-{}", std::process::id()));
        let guard = sidecar(&path).unwrap();
        let inherited = guard.file.try_clone().unwrap();
        assert!(sidecar(&path).is_err());
        drop(guard);
        let next =
            sidecar(&path).expect("an inherited descriptor must not extend the operation's lock");
        drop(next);
        drop(inherited);
        std::fs::remove_file(path).unwrap();
    }
}
