//! In-place write with a journal and full verification: same inode (birth time survives), atime/mtime restored,
//! decoded pixels identical (or the non-XMP bytes identical where pixels cannot be decoded).
//!
//! Preserving inode and birth time rules out write-temp-then-rename.
//! An in-place rewrite is not atomic, so the file is protected by a **journal** on the local disk instead
//! to recover from power loss or an interrupted write:
//!   1. the original bytes and timestamps are written to `~/.local/share/memetag/journal/<entry>/`, fsynced, and
//!      the entry is committed by renaming `meta.json.tmp` → `meta.json` (fsynced directory);
//!   2. the file is rewritten, fsynced, its times restored, fsynced again, re-read and verified;
//!   3. only then is the entry removed. A write that fails is rolled back from memory on the spot.
//!
//! `recover()` runs at every start: any entry still present means a write was interrupted, and the file is put back
//! to its original bytes and times (or, if the new bytes landed completely, just its times). Verification is by SHA-256.
//! An entry whose file now holds the same media under a readable packet is a later, complete write; it is retired and
//! the file left alone. The journal is per machine: the file-server batch helper keeps its own, and messages say so.
//! Each entry holds a stable sidecar `flock` in the sibling `journal.locks` directory from `begin` to `finish`, because "still present" is not "interrupted"
//! while another memetag process is mid-write (every command replays the journal at startup): `recover` leaves a locked entry alone, and a second writer
//! of the same file is refused rather than journaled twice.
//! Caveats that no code here can fix: a drive that acknowledges fsync before the data is stable, and an sshfs mount
//! whose server does not honour fsync — the journal narrows the damage to "restorable", it cannot make the network honest.
use crate::containers::{self, Kind};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn ts(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{}.{:09}", d.as_secs(), d.subsec_nanos())
}
pub fn now_s() -> String {
    ts(SystemTime::now())
}

/// Stable id for a file: hash of decoded RGBA pixels when decodable, else of the non-XMP bytes.
pub fn pixel_hash(b: &[u8]) -> Option<String> {
    let img = containers::decode(b).ok()?;
    let mut h = Sha256::new();
    h.update(img.into_rgba8().as_raw());
    Some(hex::encode(&h.finalize()[..8]))
}
/// Provisional id for an untagged file: hash of the container bytes minus any XMP carrier. Cheap (no decode); replaced by the pixel hash when the file is first tagged.
pub fn byte_id(b: &[u8]) -> String {
    let base = containers::strip_xmp(b).unwrap_or_else(|_| b.to_vec());
    let mut h = Sha256::new();
    h.update(&base);
    format!("b{}", hex::encode(&h.finalize()[..8]))
}
pub fn content_id(b: &[u8]) -> String {
    if let Some(h) = pixel_hash(b) {
        return h;
    }
    let base = containers::strip_xmp(b).unwrap_or_else(|_| b.to_vec());
    let mut h = Sha256::new();
    h.update(&base);
    format!("x{}", hex::encode(&h.finalize()[..8]))
}
fn sha(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}

/// Full file revision, including metadata. Content IDs deliberately ignore XMP and cannot detect edits.
pub(crate) fn revision(bytes: &[u8]) -> String {
    sha(bytes)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub kind: Kind,
    pub before: usize,
    pub after: usize,
    pub pixels: String,
    pub mtime_kept: bool,
    pub btime_kept: bool,
    pub inode_same: bool,
}
/// What a successful write leaves behind: the report and the bytes re-read from disk (so callers need not read again).
pub struct Written {
    pub report: Report,
    pub bytes: Vec<u8>,
}

// ---------- journal ----------
#[derive(Serialize, Deserialize)]
struct Meta {
    path: PathBuf,
    old_sha: String,
    new_sha: String,
    old_len: usize,
    new_len: usize,
    atime_ns: u128,
    mtime_ns: u128,
    started: String,
}

#[cfg(test)]
thread_local! { static TEST_JOURNAL: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) }; }
/// `$MEMETAG_JOURNAL`, else `~/.local/share/memetag/journal`; tests use a per-process directory under the temp dir.
pub fn journal_dir() -> PathBuf {
    #[cfg(test)]
    if let Some(p) = TEST_JOURNAL.with(|t| t.borrow().clone()) {
        return p;
    }
    if cfg!(test) {
        return std::env::temp_dir().join(format!("memetag-journal-test-{}", std::process::id()));
    }
    if let Some(p) = std::env::var_os("MEMETAG_JOURNAL") {
        return PathBuf::from(p);
    }
    crate::paths::data_dir().join("journal")
}
/// The machine this process runs on, for messages about a journal that lives here and nowhere else
/// (the file-server batch helper keeps its own; `memetag recover` on the desktop cannot see it).
fn host() -> String {
    fs::read_to_string("/etc/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "this machine".into())
}
fn fsync_dir(dir: &Path) -> Result<(), String> {
    File::open(dir)
        .and_then(|d| d.sync_all())
        .map_err(|e| format!("fsync {}: {e}", dir.display()))
}
fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut f = File::create(path).map_err(|e| format!("journal {}: {e}", path.display()))?;
    f.write_all(bytes)
        .and_then(|_| f.sync_all())
        .map_err(|e| format!("journal {}: {e}", path.display()))
}
fn ns(t: SystemTime) -> u128 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
}
fn from_ns(n: u128) -> SystemTime {
    UNIX_EPOCH + Duration::new((n / 1_000_000_000) as u64, (n % 1_000_000_000) as u32)
}

struct Entry {
    dir: PathBuf,
    /// The permanent sidecar flock, held until `finish` (or the drop of a failed attempt) so that `recover` in another
    /// process sees a live write, not an interrupted one.
    _lock: JournalLock,
}
struct JournalLock {
    _stable: crate::locking::Lock,
    // Also honour the old entry lock while already-running older helpers drain.
    _legacy: crate::locking::Lock,
}
/// Permanent sibling lock taken exclusively without waiting. It outlives entry removal, so a cleanup
/// and a new begin cannot lock different inodes for the same entry.
fn try_lock(dir: &Path) -> Result<Option<JournalLock>, String> {
    let locks = dir
        .parent()
        .ok_or("Missing journal parent")?
        .with_extension("locks");
    fs::create_dir_all(&locks).map_err(|e| e.to_string())?;
    let path = locks.join(dir.file_name().ok_or("Missing journal entry name")?);
    let Some(stable) = lock_at(&path)? else {
        return Ok(None);
    };
    let Some(legacy) = lock_at(&dir.join("lock"))? else {
        return Ok(None);
    };
    Ok(Some(JournalLock {
        _stable: stable,
        _legacy: legacy,
    }))
}

fn lock_at(path: &Path) -> Result<Option<crate::locking::Lock>, String> {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("journal lock {}: {e}", path.display()))?;
    crate::locking::try_lock(&f)
}
fn begin(
    path: &Path,
    old: &[u8],
    new: &[u8],
    atime: SystemTime,
    mtime: SystemTime,
) -> Result<Entry, String> {
    let abs = path
        .canonicalize()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let root = journal_dir();
    fs::create_dir_all(&root).map_err(|e| format!("journal dir: {e}"))?;
    let dir = root.join(&sha(abs.as_os_str().as_encoded_bytes())[..24]);
    fs::create_dir_all(&dir).map_err(|e| format!("journal entry: {e}"))?;
    let Some(lock) = try_lock(&dir)? else {
        return Err(format!(
            "a write of {} is in progress in another memetag process on {}; try again when it has finished",
            abs.display(),
            host()
        ));
    };
    if dir.join("meta.json").exists() {
        return Err(format!(
            "an interrupted write of {} is still journaled on {} in {}; run `memetag recover` there first",
            abs.display(),
            host(),
            root.display()
        ));
    }
    for stale in ["old.bin", "meta.json.tmp"] {
        let _ = fs::remove_file(dir.join(stale));
    }
    write_synced(&dir.join("old.bin"), old)?;
    let meta = Meta {
        path: abs,
        old_sha: sha(old),
        new_sha: sha(new),
        old_len: old.len(),
        new_len: new.len(),
        atime_ns: ns(atime),
        mtime_ns: ns(mtime),
        started: now_s(),
    };
    write_synced(
        &dir.join("meta.json.tmp"),
        &serde_json::to_vec(&meta).map_err(|e| e.to_string())?,
    )?;
    fs::rename(dir.join("meta.json.tmp"), dir.join("meta.json"))
        .map_err(|e| format!("journal commit: {e}"))?;
    fsync_dir(&dir)?;
    fsync_dir(&root)?;
    Ok(Entry { dir, _lock: lock })
}
fn finish(entry: Entry) -> Result<(), String> {
    // meta.json goes first: once it is gone the entry is inert even if the rest lingers; the lock is released when
    // `entry` drops at the end, after the directory is gone
    fs::remove_file(entry.dir.join("meta.json")).map_err(|e| format!("journal release: {e}"))?;
    fsync_dir(&entry.dir)?;
    for f in ["old.bin", "lock"] {
        let _ = fs::remove_file(entry.dir.join(f));
    }
    let _ = fs::remove_dir(&entry.dir);
    fsync_dir(&journal_dir())
}

fn open_rw(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| format!("open rw: {e}"))
}
/// Overwrite the open file with `bytes` in place (same inode), fsync, restore times, fsync again.
/// The caller opens the file first: a file that cannot be opened for writing (root-owned on the share,
/// 2026-09-22) must fail before any journal entry exists, or the entry outlives an attempt that wrote nothing.
fn overwrite(
    f: &mut File,
    bytes: &[u8],
    atime: SystemTime,
    mtime: SystemTime,
) -> Result<(), String> {
    f.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    f.write_all(bytes).map_err(|e| format!("write: {e}"))?;
    f.set_len(bytes.len() as u64)
        .map_err(|e| format!("truncate: {e}"))?;
    f.sync_all().map_err(|e| format!("fsync: {e}"))?;
    f.set_times(FileTimes::new().set_accessed(atime).set_modified(mtime))
        .map_err(|e| format!("set_times: {e}"))?;
    f.sync_all().map_err(|e| format!("fsync (times): {e}"))
}

/// Put the original bytes back through the same handle after a failed write; releases the entry when that verifies.
fn rollback(
    f: &mut File,
    path: &Path,
    old_bytes: &[u8],
    atime: SystemTime,
    mtime: SystemTime,
    entry: Entry,
    why: String,
) -> String {
    let restored = overwrite(f, old_bytes, atime, mtime)
        .and_then(|_| fs::read(path).map_err(|e| e.to_string()));
    let entry_dir = entry.dir.display().to_string();
    match restored {
        Ok(re) if re == old_bytes => {
            let _ = finish(entry);
            format!("{why} — original restored ({})", path.display())
        }
        // the entry stays committed (and its lock is released with `entry`), so the next start recovers it
        Ok(_) => format!(
            "{why} — restore did not verify; the original is journaled on {} in {entry_dir} (run `memetag recover` there)",
            host()
        ),
        Err(e) => format!(
            "{why} — restore failed ({e}); the original is journaled on {} in {entry_dir} (run `memetag recover` there)",
            host()
        ),
    }
}

/// Rewrites `path` with `new_bytes` under the journal, restoring timestamps, then verifies against `old_bytes`.
pub fn write_in_place(path: &Path, old_bytes: &[u8], new_bytes: &[u8]) -> Result<Written, String> {
    if crate::batch::requires_remote(path)? {
        return Err("Network files must be written by the server helper so all writers share its lock and journal.".into());
    }
    let mut f = open_rw(path)?;
    // The media inode is stable across our in-place writes. This lock also coordinates writers with
    // different journal directories on this filesystem, and is never unlinked at journal cleanup.
    let _file_lock = crate::locking::lock_file(&f).map_err(|e| {
        format!(
            "a write of {} is in progress or cannot be locked: {e}",
            path.display()
        )
    })?;
    let md = f.metadata().map_err(|e| e.to_string())?;
    let path_md = fs::metadata(path).map_err(|e| e.to_string())?;
    if (md.dev(), md.ino()) != (path_md.dev(), path_md.ino()) {
        return Err("File was replaced while opening it; reload and retry.".into());
    }
    let mut current = Vec::new();
    f.read_to_end(&mut current).map_err(|e| e.to_string())?;
    if current != old_bytes {
        return Err("File changed before the write lock was acquired; reload and retry. Nothing was written.".into());
    }
    let (atime, mtime, btime, ino) = (
        md.accessed().map_err(|e| e.to_string())?,
        md.modified().map_err(|e| e.to_string())?,
        md.created().ok(),
        md.ino(),
    );
    let kind = containers::sniff(old_bytes);
    let hash0 = pixel_hash(old_bytes);
    let entry = begin(path, old_bytes, new_bytes, atime, mtime)?;
    let rollback = |f: &mut File, entry: Entry, why: String| -> String {
        rollback(f, path, old_bytes, atime, mtime, entry, why)
    };
    if let Err(e) = overwrite(&mut f, new_bytes, atime, mtime) {
        return Err(rollback(&mut f, entry, e));
    }
    let md2 = fs::metadata(path).map_err(|e| e.to_string())?;
    let re = fs::read(path).map_err(|e| e.to_string())?;
    if re != new_bytes {
        return Err(rollback(
            &mut f,
            entry,
            format!(
                "read-back differs from what was written ({} vs {} bytes)",
                re.len(),
                new_bytes.len()
            ),
        ));
    }
    let pixels = match (&hash0, pixel_hash(&re)) {
        (Some(a), Some(b)) if a == &b => "same",
        (Some(_), _) => "DIFF",
        (None, _) => {
            if containers::strip_xmp(&re).ok() == containers::strip_xmp(old_bytes).ok() {
                "undecodable,bytes-same"
            } else {
                "DIFF"
            }
        }
    };
    let report = Report {
        kind,
        before: old_bytes.len(),
        after: new_bytes.len(),
        pixels: pixels.into(),
        mtime_kept: md2.modified().ok() == Some(mtime),
        btime_kept: md2.created().ok() == btime,
        inode_same: md2.ino() == ino,
    };
    // Every promise in the report is enforced, not just noted: a write that moved the file's times or inode
    // (a mount that stores coarser timestamps than it reports) is put back rather than kept with a FAIL line.
    let broken = [
        (report.pixels == "DIFF", "pixels changed"),
        (
            !report.mtime_kept,
            "modification time did not survive the write",
        ),
        (!report.btime_kept, "birth time did not survive the write"),
        (!report.inode_same, "inode changed"),
    ];
    if let Some((_, why)) = broken.iter().find(|(bad, _)| *bad) {
        return Err(rollback(&mut f, entry, (*why).into()));
    }
    finish(entry)?;
    Ok(Written { report, bytes: re })
}

/// A file whose media bytes equal the journaled original's and whose XMP packet (if any) reads: a complete later
/// write, not the debris of an interrupted one. An interrupted write leaves a prefix of the new bytes over a suffix of
/// the old, which shifts the media unless the packet kept its length, in which case the packet itself is torn.
fn later_version(now: &[u8], old: &[u8]) -> bool {
    let media_same = matches!(
        (containers::strip_xmp(now), containers::strip_xmp(old)),
        (Ok(a), Ok(b)) if a == b
    );
    let packet_reads = match containers::get_xmp(now) {
        Ok(None) => true,
        Ok(Some(p)) => crate::xmp::read(&p).is_ok(),
        Err(_) => false,
    };
    media_same && packet_reads
}

/// Replay the journal: every entry is a write that did not reach `finish`. Returns one line per entry handled;
/// entries whose file cannot be read right now (share not mounted) are kept for next time.
pub fn recover() -> Vec<String> {
    let root = journal_dir();
    let mut out = vec![];
    let Ok(rd) = fs::read_dir(&root) else {
        return out;
    };
    for e in rd.flatten() {
        let dir = e.path();
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let meta_path = dir.join("meta.json");
        // A live entry belongs to a write another memetag process is making right now; touching its file would
        // undo that write, or race it. The lock is held for as long as this entry is being handled.
        let _lock = match try_lock(&dir) {
            Ok(Some(l)) => l,
            Ok(None) => {
                let file = fs::read(&meta_path)
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Meta>(&b).ok())
                    .map(|m| m.path.display().to_string())
                    .unwrap_or_else(|| dir.display().to_string());
                out.push(format!(
                    "recover: {file}: a write is in progress in another memetag process; left alone"
                ));
                continue;
            }
            Err(e) => {
                out.push(format!("recover: {e}"));
                continue;
            }
        };
        if !meta_path.exists() {
            let _ = fs::remove_dir_all(&dir);
            continue;
        } // never committed: nothing was written
        let line = (|| -> Result<String, String> {
            let meta: Meta =
                serde_json::from_slice(&fs::read(&meta_path).map_err(|e| e.to_string())?)
                    .map_err(|e| format!("meta: {e}"))?;
            if crate::batch::requires_remote(&meta.path)? {
                return Err(format!("{}: legacy network journal kept in {}; recover on the file server with other writers stopped", meta.path.display(), dir.display()));
            }
            let old = fs::read(dir.join("old.bin")).map_err(|e| format!("old.bin: {e}"))?;
            if sha(&old) != meta.old_sha || old.len() != meta.old_len {
                return Err(format!(
                    "journal copy of {} is itself damaged; left in {}",
                    meta.path.display(),
                    dir.display()
                ));
            }
            let (atime, mtime) = (from_ns(meta.atime_ns), from_ns(meta.mtime_ns));
            let mut file = open_rw(&meta.path)?;
            let _file_lock = crate::locking::lock_file(&file).map_err(|e| {
                format!("{}: {e}; recovery kept for next time", meta.path.display())
            })?;
            let mut now = Vec::new();
            file.read_to_end(&mut now).map_err(|e| e.to_string())?;
            let state = sha(&now);
            let what = if state == meta.new_sha {
                "new bytes had landed; times restored"
            } else if state == meta.old_sha {
                "untouched; times stand"
            } else if later_version(&now, &old) {
                // The journaled attempt never touched the file (it could not be opened for writing: a root-owned
                // file on the share, 2026-09-22) and a later write reached it. Restoring old.bin here would erase
                // every tag written since. The file is left as it is and its own times stand.
                "changed by a later write, media intact; entry retired, file left alone"
            } else {
                overwrite(&mut file, &old, atime, mtime)?;
                let back = fs::read(&meta.path).map_err(|e| e.to_string())?;
                if sha(&back) != meta.old_sha {
                    return Err(format!(
                        "{}: restore did not verify; journal kept in {}",
                        meta.path.display(),
                        dir.display()
                    ));
                }
                "partial write found; original restored"
            };
            let times_stand = fs::metadata(&meta.path)
                .ok()
                .and_then(|m| m.modified().ok())
                .is_some_and(|m| m == mtime);
            if (state == meta.new_sha || state == meta.old_sha) && !times_stand {
                file.set_times(FileTimes::new().set_accessed(atime).set_modified(mtime))
                    .map_err(|e| e.to_string())?;
                file.sync_all().map_err(|e| e.to_string())?;
            }
            // meta.json is gone before the lock is: an entry never reads as interrupted while it is being retired
            fs::remove_file(&meta_path).map_err(|e| format!("journal release: {e}"))?;
            fsync_dir(&dir)?;
            for f in ["old.bin", "lock"] {
                let _ = fs::remove_file(dir.join(f));
            }
            let _ = fs::remove_dir(&dir);
            fsync_dir(&root)?;
            Ok(format!(
                "recovered {} ({what}; write started {})",
                meta.path.display(),
                meta.started
            ))
        })();
        out.push(line.unwrap_or_else(|e| format!("recover: {e}")));
    }
    out
}

impl Report {
    pub fn ok(&self) -> bool {
        self.pixels != "DIFF" && self.mtime_kept && self.btime_kept && self.inode_same
    }
    pub fn line(&self, path: &Path, tags_ok: bool) -> String {
        format!(
            "{} {} kind={} bytes {}->{} pixels {} mtime {} btime {} inode {} tags {}",
            if self.ok() && tags_ok { "OK  " } else { "FAIL" },
            path.display(),
            self.kind.label(),
            self.before,
            self.after,
            self.pixels,
            if self.mtime_kept { "kept" } else { "MOVED" },
            if self.btime_kept { "kept" } else { "MOVED" },
            if self.inode_same { "same" } else { "NEW" },
            if tags_ok { "roundtrip" } else { "MISMATCH" }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn png(v: u8) -> Vec<u8> {
        let mut c = std::io::Cursor::new(Vec::new());
        image::RgbImage::from_pixel(8, 8, image::Rgb([v, 40, 10]))
            .write_to(&mut c, image::ImageFormat::Png)
            .unwrap();
        c.into_inner()
    }
    #[test]
    fn journal_restores_an_interrupted_write_and_its_times() {
        let root = std::env::temp_dir().join(format!("memetag-writer-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        TEST_JOURNAL.with(|t| *t.borrow_mut() = Some(root.join("journal")));
        let path = root.join("meme.png");
        let old = png(90);
        fs::write(&path, &old).unwrap();
        let time = UNIX_EPOCH + Duration::new(1_500_000_000, 123_456_789);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_accessed(time).set_modified(time))
            .unwrap();
        let ino = fs::metadata(&path).unwrap().ino();
        // a normal write: journal empty afterwards, inode and times kept, bytes returned = bytes on disk
        let new = containers::set_xmp(&old, "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\"/></rdf:RDF></x:xmpmeta>").unwrap();
        let w = write_in_place(&path, &old, &new).unwrap();
        assert!(w.report.ok());
        assert_eq!(w.bytes, new);
        assert_eq!(fs::read(&path).unwrap(), new);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), time);
        assert_eq!(fs::metadata(&path).unwrap().ino(), ino);
        assert!(
            fs::read_dir(journal_dir())
                .map(|d| d.count() == 0)
                .unwrap_or(true),
            "journal must be empty after a clean write"
        );
        // simulate a crash mid-write: journal committed, file left with a partial mix of new and old bytes, times moved
        let entry = begin(&path, &new, &png(200), time, time).unwrap();
        let mut partial = png(200);
        partial.truncate(partial.len() / 2);
        partial.extend_from_slice(&new[partial.len().min(new.len())..]);
        fs::write(&path, &partial).unwrap();
        assert!(fs::metadata(&path).unwrap().modified().unwrap() != time);
        // while the writing process lives (its entry holds the lock) a second writer is refused and `recover`,
        // as another process runs it at start, leaves the file exactly as the writer has it
        let refused = write_in_place(&path, &partial, &new)
            .err()
            .expect("a live entry refuses a second writer");
        assert!(refused.contains("in progress"), "{refused}");
        let lines = recover();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].contains("in progress") && lines[0].contains("left alone"),
            "{lines:?}"
        );
        assert_eq!(
            fs::read(&path).unwrap(),
            partial,
            "a live entry's file is not touched"
        );
        assert!(entry.dir.join("meta.json").exists());
        // the process died: its lock went with it, and the entry is an interrupted write
        let dir = entry.dir.clone();
        drop(entry);
        let refused = write_in_place(&path, &partial, &new)
            .err()
            .expect("a journaled file refuses new writes until recovered");
        assert!(refused.contains("still journaled"), "{refused}");
        let lines = recover();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("original restored"), "{lines:?}");
        assert_eq!(fs::read(&path).unwrap(), new);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), time);
        assert!(!dir.exists());
        // crash after the new bytes landed but before the times were restored
        let entry = begin(&path, &new, &png(200), time, time).unwrap();
        let dir = entry.dir.clone();
        drop(entry);
        fs::write(&path, png(200)).unwrap();
        let lines = recover();
        assert!(lines[0].contains("new bytes had landed"), "{lines:?}");
        assert_eq!(fs::read(&path).unwrap(), png(200));
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), time);
        assert!(!dir.exists());
        // a pixel change is rolled back on the spot and leaves no journal entry
        let err = match write_in_place(&path, &png(200), &png(201)) {
            Err(e) => e,
            Ok(_) => panic!("a pixel change must be refused"),
        };
        assert!(
            err.contains("pixels changed") && err.contains("original restored"),
            "{err}"
        );
        assert_eq!(fs::read(&path).unwrap(), png(200));
        assert!(fs::read_dir(journal_dir())
            .map(|d| d.count() == 0)
            .unwrap_or(true));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(journal_dir());
    }

    const PACKET_A: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:subject><rdf:Bag><rdf:li>catgirl</rdf:li></rdf:Bag></dc:subject></rdf:Description></rdf:RDF></x:xmpmeta>";
    const PACKET_B: &str = "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\"><rdf:Description rdf:about=\"\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\"><dc:subject><rdf:Bag><rdf:li>catgirl</rdf:li><rdf:li>high fantasy</rdf:li></rdf:Bag></dc:subject></rdf:Description></rdf:RDF></x:xmpmeta>";

    #[test]
    fn journal_lock_survives_entry_removal_and_recreation() {
        let root =
            std::env::temp_dir().join(format!("memetag-lock-lifetime-{}", std::process::id()));
        let entry = root.join("journal/entry");
        fs::create_dir_all(&entry).unwrap();
        let held = try_lock(&entry).unwrap().unwrap();
        fs::remove_dir_all(&entry).unwrap();
        fs::create_dir_all(&entry).unwrap();
        assert!(try_lock(&entry).unwrap().is_none());
        drop(held);
        assert!(try_lock(&entry).unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn stale_writers_and_separate_journals_cannot_erase_completed_tags() {
        let root = std::env::temp_dir().join(format!("memetag-stale-write-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        TEST_JOURNAL.with(|t| *t.borrow_mut() = Some(root.join("journal")));
        let path = root.join("image.png");
        let old = png(90);
        let first = containers::set_xmp(&old, PACKET_B).unwrap();
        let second = containers::set_xmp(&old, PACKET_A).unwrap();
        fs::write(&path, &old).unwrap();
        write_in_place(&path, &old, &first).unwrap();
        let err = write_in_place(&path, &old, &second).err().unwrap();
        assert!(err.contains("File changed"), "{err}");
        assert_eq!(fs::read(&path).unwrap(), first);
        let lock = open_rw(&path).unwrap();
        let held = crate::locking::lock_file(&lock).unwrap();
        TEST_JOURNAL.with(|t| *t.borrow_mut() = Some(root.join("other-journal")));
        let err = write_in_place(&path, &first, &second).err().unwrap();
        assert!(err.contains("in progress"), "{err}");
        assert_eq!(fs::read(&path).unwrap(), first);
        drop(held);
        drop(lock);
        // An explicit retry based on fresh bytes is allowed.
        write_in_place(&path, &first, &second).unwrap();
        fs::remove_dir_all(root).unwrap();
        TEST_JOURNAL.with(|t| *t.borrow_mut() = None);
    }

    // A failed write must not leave a journal that could later roll back a successful edit
    // made by another process. Recovery must preserve a later complete write.
    #[test]
    fn unwritable_file_leaves_no_entry_and_recover_keeps_a_later_write() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("memetag-writer-ro-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        TEST_JOURNAL.with(|t| *t.borrow_mut() = Some(root.join("journal")));
        let path = root.join("meme.png");
        let old = png(90);
        fs::write(&path, &old).unwrap();
        let new = containers::set_xmp(&old, PACKET_A).unwrap();
        // a file that cannot be opened for writing fails before any journal entry exists
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        let err = match write_in_place(&path, &old, &new) {
            Err(e) => e,
            Ok(_) => panic!("a read-only file must refuse the write"),
        };
        assert!(err.starts_with("open rw:"), "{err}");
        assert_eq!(fs::read(&path).unwrap(), old);
        let entries = fs::read_dir(journal_dir()).map(|d| d.count()).unwrap_or(0);
        assert_eq!(
            entries, 0,
            "no entry for an attempt that could not open the file"
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        // a stale entry (the old helper's, and that helper is gone) followed by a complete later write with other
        // tags and its own times
        let entry = begin(&path, &old, &new, UNIX_EPOCH, UNIX_EPOCH).unwrap();
        let dir = entry.dir.clone();
        drop(entry);
        let later = containers::set_xmp(&old, PACKET_B).unwrap();
        fs::write(&path, &later).unwrap();
        let t2 = UNIX_EPOCH + Duration::new(1_600_000_000, 5);
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(FileTimes::new().set_accessed(t2).set_modified(t2))
            .unwrap();
        let msg = write_in_place(&path, &later, &new).err().unwrap();
        assert!(
            msg.contains("still journaled on") && msg.contains("recover"),
            "{msg}"
        );
        let lines = recover();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains("later write"), "{lines:?}");
        assert_eq!(
            fs::read(&path).unwrap(),
            later,
            "the later write must survive recover"
        );
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), t2);
        assert!(!dir.exists());
        // but a torn packet of the same length is still debris and is restored
        let entry = begin(&path, &later, &new, t2, t2).unwrap();
        let dir = entry.dir.clone();
        drop(entry);
        let mut torn = later.clone();
        let at = torn.windows(7).position(|w| w == b"catgirl").unwrap();
        torn[at..at + 7].copy_from_slice(b"\0\0\0\0\0\0\0");
        fs::write(&path, &torn).unwrap();
        let lines = recover();
        assert!(lines[0].contains("original restored"), "{lines:?}");
        assert_eq!(fs::read(&path).unwrap(), later);
        assert!(!dir.exists());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(journal_dir());
    }
}
