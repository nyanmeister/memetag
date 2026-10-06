//! SQLite index — a cache rebuilt from the files. Files are the source of truth.
use crate::containers;
use crate::vocab::Vocab;
use crate::xmp;
use crate::Cfg;
use rusqlite::{params, Connection};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

pub struct Db {
    pub scope: crate::library::Scope,
    pub conn: Connection,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct FileRow {
    pub id: String,
    pub path: String,
    pub format: String,
    pub kind: String,
    pub width: i64,
    pub height: i64,
    pub size: i64,
    pub mtime: f64,
    pub created_at: f64,
    pub tagged_at: f64,
    pub xmp_tag_count: i64,
    pub tags: BTreeSet<String>,
    pub text: String,
}

fn migrate_sources(conn: &Connection, path: &Path) -> Result<(), String> {
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if version == 3 {
        return Ok(());
    }
    let _lock = crate::locking::sidecar(&path.with_extension("migration.lock"))?;
    let version: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    if version == 3 {
        return Ok(());
    }
    let count: i64 = conn
        .query_row("SELECT count(*) FROM files", [], |r| r.get(0))
        .map_err(|e| e.to_string())?;
    let nonce = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_nanos();
    if count > 0 {
        let backup = path.with_extension(format!("before-sources-{nonce}.sqlite"));
        conn.execute(
            "VACUUM INTO ?1",
            [backup.to_str().ok_or("Index backup path is not UTF-8")?],
        )
        .map_err(|e| format!("Index backup failed; migration aborted: {e}"))?;
        eprintln!("Index backup: {}", backup.display());
    }
    let tx = conn.unchecked_transaction().map_err(|e| e.to_string())?;
    let staging = format!("__migration_{nonce}_{}/", std::process::id());
    for table in [
        "files",
        "tags",
        "text",
        "phash2",
        "text_meta",
        "manual_text",
        "proposal_rejects",
    ] {
        let collision: i64 = tx
            .query_row(
                &format!("SELECT count(*) FROM {table} WHERE substr(path,1,?1)=?2"),
                params![staging.len() as i64, staging],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if collision > 0 {
            return Err("Migration staging key collision; index retained".into());
        }
        tx.execute(&format!("UPDATE {table} SET path=?1||path"), [&staging])
            .map_err(|e| e.to_string())?;
        tx.execute(
            &format!("UPDATE {table} SET path='main/'||substr(path,?1)"),
            [(staging.len() + 1) as i64],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.execute_batch("PRAGMA user_version=3")
        .map_err(|e| e.to_string())?;
    tx.commit().map_err(|e| e.to_string())
}

/// A unix-seconds timestamp (a `FileRow`'s `mtime`/`created_at`, an OCR time) as `YYYY-MM-DD HH:MMZ`, UTC, no crate —
/// the same form the search grammar's dates use. Shown in the tile tooltip and the editor as the file's download time.
pub fn fmt_time(t: f64) -> String {
    chrono_lite(t as i64)
}
fn chrono_lite(s: i64) -> String {
    // UTC, no crate
    let days = s.div_euclid(86400);
    let rem = s.rem_euclid(86400);
    let (mut y, mut m, mut d) = (1970i64, 1i64, 1i64);
    let mut left = days;
    loop {
        let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
        let ylen = if leap { 366 } else { 365 };
        if left < ylen {
            break;
        }
        left -= ylen;
        y += 1;
    }
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let ml = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    for (i, &n) in ml.iter().enumerate() {
        if left < n {
            m = i as i64 + 1;
            d = left + 1;
            break;
        }
        left -= n;
    }
    format!(
        "{y:04}-{m:02}-{d:02} {:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60
    )
}

impl Db {
    pub fn open(path: &Path) -> Result<Db, String> {
        Self::open_scope(path, crate::library::load(&crate::cfg().root)?, false)
    }
    pub fn open_cfg(c: &Cfg) -> Result<Db, String> {
        c.ensure_current()?;
        match c.library()? {
            Some(library) => {
                if !crate::sources::path().exists() {
                    let mut draft = crate::sources::Draft::load(&c.root)?;
                    draft.library = library.clone();
                    draft.save()?;
                }
                Self::open_scope(&c.db, library.selection(c.active_source.as_deref()), true)
            }
            None => Self::open(&c.db),
        }
    }
    fn open_scope(path: &Path, scope: crate::library::Scope, multi: bool) -> Result<Db, String> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        // schema v2: rows keyed by PATH; id is a property (several files may share one — copies, re-encodes with XMP carried over)
        let v: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap_or(0);
        if !matches!(v, 0 | 2 | 3) {
            return Err(format!("Unsupported index schema {v}; index was not reset"));
        }
        if v == 0 {
            conn.execute_batch("PRAGMA user_version=2;")
                .map_err(|e| e.to_string())?;
        }
        conn.execute_batch("
            PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY, id TEXT NOT NULL, format TEXT, kind TEXT, width INTEGER, height INTEGER, size INTEGER,
                mtime REAL, created_at REAL, tagged_at REAL, xmp_tag_count INTEGER DEFAULT 0, indexed_at REAL, text_in_file INTEGER NOT NULL DEFAULT 0);
            CREATE INDEX IF NOT EXISTS files_id ON files(id);
            CREATE TABLE IF NOT EXISTS tags(path TEXT NOT NULL, tag TEXT NOT NULL, source TEXT NOT NULL, PRIMARY KEY(path, tag, source));
            CREATE INDEX IF NOT EXISTS tags_tag ON tags(tag);
            CREATE VIRTUAL TABLE IF NOT EXISTS text USING fts5(path UNINDEXED, body);
            CREATE TABLE IF NOT EXISTS phash2(path TEXT NOT NULL, alg TEXT NOT NULL, hash BLOB NOT NULL, PRIMARY KEY(path, alg));
            CREATE TABLE IF NOT EXISTS text_meta(path TEXT PRIMARY KEY, engine TEXT NOT NULL, at REAL NOT NULL, secs REAL);
            CREATE TABLE IF NOT EXISTS manual_text(path TEXT PRIMARY KEY, body TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS tag_history(tag TEXT PRIMARY KEY, uses INTEGER NOT NULL DEFAULT 1);
            CREATE TABLE IF NOT EXISTS proposal_rejects(tag TEXT NOT NULL, path TEXT NOT NULL, PRIMARY KEY(tag, path));
            CREATE TABLE IF NOT EXISTS proposal_modes(tag TEXT PRIMARY KEY, mode TEXT NOT NULL);
        ").map_err(|e| format!("schema: {e}"))?;
        // text_in_file: did the file's own XMP carry meme:text at its last scan? Set on every scan (see write_row), it is
        // the difference between OCR text living only in this index and OCR text embedded in the files themselves — the
        // gap a server-side `embed-text` pass closes (see `ocr --status`). Added by ALTER so an existing v2 index keeps
        // its 26k rows and its OCR text instead of being dropped and rebuilt; existing rows default to 0 and become
        // accurate as `pull`/`reindex` re-scan the files, which the embed runbook's closing pull does in bulk.
        let has_col = conn
            .prepare("SELECT 1 FROM pragma_table_info('files') WHERE name='text_in_file'")
            .and_then(|mut s| s.exists([]))
            .unwrap_or(false);
        if !has_col {
            conn.execute(
                "ALTER TABLE files ADD COLUMN text_in_file INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(|e| format!("schema (text_in_file): {e}"))?;
        }
        if multi {
            migrate_sources(&conn, path)?;
        }
        conn.execute_batch("CREATE TEMP TABLE excluded_files(path TEXT PRIMARY KEY)")
            .map_err(|e| e.to_string())?;
        {
            let mut query = conn
                .prepare("SELECT path FROM files")
                .map_err(|e| e.to_string())?;
            for row in query
                .query_map([], |r| r.get::<_, String>(0))
                .map_err(|e| e.to_string())?
            {
                let path = row.map_err(|e| e.to_string())?;
                if !scope.contains(Path::new(&path)) {
                    conn.execute("INSERT INTO excluded_files VALUES(?1)", [path])
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        Ok(Db { conn, scope })
    }

    /// Test fixture helper; live writes use source-aware `upsert_cfg`.
    #[cfg(test)]
    pub fn upsert_file(&self, root: &Path, path: &Path, vocab: &Vocab) -> Result<FileRow, String> {
        self.write_scan(scan_file(root, path, vocab)?)
    }
    pub fn upsert_cfg(&self, c: &Cfg, path: &Path, bytes: &[u8]) -> Result<FileRow, String> {
        c.ensure_current()?;
        let md = std::fs::metadata(path).map_err(|e| e.to_string())?;
        self.write_scan(c.scan_bytes(path, bytes, &md)?)
    }
    fn write_scan(&self, scan: Scan) -> Result<FileRow, String> {
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        write_row(&tx, &scan)?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(scan.row)
    }
    /// One row with its tags and text, as `all()` would build it.
    pub fn row(&self, rel: &str) -> Result<Option<FileRow>, String> {
        use rusqlite::OptionalExtension;
        let mut row = match self.conn.query_row("SELECT id,path,format,kind,width,height,size,mtime,created_at,tagged_at,xmp_tag_count FROM files WHERE path=?1", [rel],
            |r| Ok(FileRow { id: r.get(0)?, path: r.get(1)?, format: r.get(2)?, kind: r.get(3)?, width: r.get(4)?, height: r.get(5)?, size: r.get(6)?, mtime: r.get(7)?, created_at: r.get(8)?, tagged_at: r.get(9)?, xmp_tag_count: r.get(10)?, tags: BTreeSet::new(), text: String::new() }))
            .optional().map_err(|e| e.to_string())? { Some(r) => r, None => return Ok(None) };
        let mut st = self
            .conn
            .prepare("SELECT tag FROM tags WHERE path=?1")
            .map_err(|e| e.to_string())?;
        row.tags = st
            .query_map([rel], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .collect();
        let manual: Option<String> = self
            .conn
            .query_row("SELECT body FROM manual_text WHERE path=?1", [rel], |r| {
                r.get(0)
            })
            .optional()
            .map_err(|e| e.to_string())?;
        row.text = match manual {
            Some(m) => m,
            None => self
                .conn
                .query_row("SELECT body FROM text WHERE path=?1 LIMIT 1", [rel], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(|e| e.to_string())?
                .unwrap_or_default(),
        };
        Ok(Some(row))
    }

    /// Import a full scan (a new or changed file from `pull`): rows, tags, embedded text; cached OCR stays unless the file carries its own text.
    pub fn import_scan(&self, scan: Scan) -> Result<(), String> {
        self.write_scan(scan).map(|_| ())
    }

    /// Import a server-side scan after a tag-only edit without replacing cached OCR.
    pub fn import_tag_scan(&self, mut scan: Scan) -> Result<(), String> {
        if !scan.manual {
            scan.text = None;
        }
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        write_row(&tx, &scan)?;
        tx.commit().map_err(|e| e.to_string())
    }

    /// Rebuild from the files: `threads` workers scan (read + parse XMP + header dims + cheap hash, no pixel decoding),
    /// the main thread alone writes, in batches. Memory in flight ≈ threads × one file.
    pub fn reindex(
        &self,
        root: &Path,
        vocab: &Vocab,
        threads: usize,
    ) -> Result<(usize, usize), String> {
        self.reindex_inner(root, vocab, threads, None, self.scope.clone(), None)
    }
    pub fn reindex_cfg(&self, c: &Cfg) -> Result<(usize, usize), String> {
        c.ensure_current()?;
        let scope = match (c.library()?, c.active_source.as_deref()) {
            (Some(library), Some(id)) => {
                let source = library.source(id)?;
                if !source.available() {
                    return Err("Source is offline or the folder was replaced; cached rows retained. Use Locate to confirm its location".into());
                }
                source.scope.clone()
            }
            _ => self.scope.clone(),
        };
        self.reindex_inner(
            &c.root,
            &c.vocab,
            c.index_threads,
            c.active_source.clone(),
            scope,
            Some(c),
        )
    }
    fn reindex_inner(
        &self,
        root: &Path,
        vocab: &Vocab,
        threads: usize,
        source: Option<String>,
        scope: crate::library::Scope,
        snapshot: Option<&Cfg>,
    ) -> Result<(usize, usize), String> {
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc, Arc,
        };
        if !root.is_dir() {
            return Err(format!(
                "{}: not a directory; index retained",
                root.display()
            ));
        }
        let mut paths = Vec::<PathBuf>::new();
        for entry in walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
            e.depth() == 0
                || !e.file_type().is_dir()
                || scope.traverse(e.path().strip_prefix(root).unwrap_or(e.path()))
        }) {
            let entry = entry.map_err(|e| format!("listing failed; index retained: {e}"))?;
            if entry.file_type().is_file()
                && scope.contains(entry.path().strip_prefix(root).unwrap_or(entry.path()))
            {
                paths.push(entry.into_path());
            }
        }
        // Files which failed to parse still exist; do not discard their cached OCR/vector rows.
        let seen_paths: std::collections::HashSet<String> = paths
            .iter()
            .filter_map(|p| p.strip_prefix(root).ok())
            .map(|p| {
                source
                    .as_ref()
                    .map(|id| format!("{id}/{}", p.display()))
                    .unwrap_or_else(|| p.to_string_lossy().into_owned())
            })
            .collect();
        if source.is_some() && paths.is_empty() && !self.all()?.is_empty() {
            return Err(
                "Empty source listing; cached files retained. Check the mount or locate the folder"
                    .into(),
            );
        }
        let total = paths.len();
        let paths = Arc::new(paths);
        let next = Arc::new(AtomicUsize::new(0));
        let (tx, rx) = mpsc::sync_channel::<Result<Scan, String>>(threads * 4); // bounded: writers never fall far behind
        let root_a = Arc::new(root.to_path_buf());
        let vocab_a = Arc::new(vocab.clone());
        let mut handles = vec![];
        for _ in 0..threads.max(1) {
            let source = source.clone();
            let (paths, next, tx, root, vocab) = (
                paths.clone(),
                next.clone(),
                tx.clone(),
                root_a.clone(),
                vocab_a.clone(),
            );
            handles.push(std::thread::spawn(move || loop {
                let k = next.fetch_add(1, Ordering::Relaxed);
                if k >= paths.len() {
                    break;
                }
                let scan = scan_file(&root, &paths[k], &vocab).map(|mut scan| {
                    if let Some(id) = &source {
                        scan.row.path = format!("{id}/{}", scan.row.path);
                    }
                    scan
                });
                if tx.send(scan).is_err() {
                    break;
                }
            }));
        }
        drop(tx);
        let (mut ok, mut bad, mut seen) = (0usize, 0usize, Vec::with_capacity(total));
        let mut txn = self
            .conn
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        let mut in_batch = 0;
        for r in rx {
            match r {
                Ok(scan) => {
                    write_row(&txn, &scan)?;
                    seen.push(scan.row.path);
                    ok += 1;
                    in_batch += 1;
                }
                Err(e) => {
                    eprintln!("skip {e}");
                    bad += 1;
                }
            }
            if in_batch >= 500 {
                if let Some(c) = snapshot {
                    c.ensure_current()?;
                }
                txn.commit().map_err(|e| e.to_string())?;
                txn = self
                    .conn
                    .unchecked_transaction()
                    .map_err(|e| e.to_string())?;
                in_batch = 0;
            }
            if (ok + bad) % 1000 == 0 {
                eprintln!("  {}/{total}", ok + bad);
            }
        }
        if let Some(c) = snapshot {
            c.ensure_current()?;
        }
        txn.commit().map_err(|e| e.to_string())?;
        for h in handles {
            let _ = h.join();
        }
        // drop rows whose files are gone
        let seen = seen_paths;
        let mut stmt = self
            .conn
            .prepare("SELECT path FROM files")
            .map_err(|e| e.to_string())?;
        let gone: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .filter(|p| self.scope.contains(Path::new(p)) && !seen.contains(p))
            .collect();
        for p in gone {
            for t in [
                "tags",
                "files",
                "text",
                "phash2",
                "manual_text",
                "text_meta",
            ] {
                self.conn
                    .execute(&format!("DELETE FROM {t} WHERE path=?1"), params![p])
                    .ok();
            }
        }
        Ok((ok, bad))
    }

    pub fn all(&self) -> Result<Vec<FileRow>, String> {
        Ok(self
            .all_cached()?
            .into_iter()
            .filter(|r| self.scope.contains(Path::new(&r.path)))
            .collect())
    }

    pub fn all_cached(&self) -> Result<Vec<FileRow>, String> {
        let mut stmt = self.conn.prepare("SELECT id,path,format,kind,width,height,size,mtime,created_at,tagged_at,xmp_tag_count FROM files ORDER BY created_at DESC, path").map_err(|e| e.to_string())?;
        let mut rows: Vec<FileRow> = stmt
            .query_map([], |r| {
                Ok(FileRow {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    format: r.get(2)?,
                    kind: r.get(3)?,
                    width: r.get(4)?,
                    height: r.get(5)?,
                    size: r.get(6)?,
                    mtime: r.get(7)?,
                    created_at: r.get(8)?,
                    tagged_at: r.get(9)?,
                    xmp_tag_count: r.get(10)?,
                    tags: BTreeSet::new(),
                    text: String::new(),
                })
            })
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .collect();
        let mut ts = self
            .conn
            .prepare("SELECT path, tag FROM tags")
            .map_err(|e| e.to_string())?;
        let mut map: std::collections::HashMap<String, BTreeSet<String>> = Default::default();
        for r in ts
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
        {
            map.entry(r.0).or_default().insert(r.1);
        }
        for f in &mut rows {
            if let Some(t) = map.remove(&f.path) {
                f.tags = t;
            }
        }
        let mut tx = self
            .conn
            .prepare("SELECT path, body FROM text")
            .map_err(|e| e.to_string())?;
        let mut texts: std::collections::HashMap<String, String> = Default::default();
        for r in tx
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
        {
            texts.insert(r.0, r.1);
        }
        let mut manual = self
            .conn
            .prepare("SELECT path, body FROM manual_text")
            .map_err(|e| e.to_string())?;
        for row in manual
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?
        {
            let (path, body) = row.map_err(|e| e.to_string())?;
            texts.insert(path, body);
        }
        for f in &mut rows {
            if let Some(t) = texts.remove(&f.path) {
                f.text = t;
            }
        }
        Ok(rows)
    }

    /// Rebuild every derived `implied` row from the stored xmp and folder rows, as a reindex would, without reading a
    /// file. Returns each file whose tag set changed, with its new full tag set (xmp, folder and implied together).
    pub fn reimply(&self, vocab: &Vocab) -> Result<Vec<(String, BTreeSet<String>)>, String> {
        let mut base: HashMap<String, (BTreeSet<String>, BTreeSet<String>)> = HashMap::new(); // path -> (own, old implied)
        for (p, t, s) in self.tag_rows()? {
            let e = base.entry(p).or_default();
            if s == "implied" {
                e.1.insert(t);
            } else {
                e.0.insert(t);
            }
        }
        let mut changed = vec![];
        let tx = self
            .conn
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        for (p, (own, old)) in base {
            let all = vocab.expand(own.iter().cloned());
            let new: BTreeSet<String> = all.difference(&own).cloned().collect();
            if new == old {
                continue;
            }
            tx.execute(
                "DELETE FROM tags WHERE path=?1 AND source='implied'",
                params![p],
            )
            .map_err(|e| e.to_string())?;
            for t in &new {
                tx.execute(
                    "INSERT OR IGNORE INTO tags VALUES(?1,?2,'implied')",
                    params![p, t],
                )
                .map_err(|e| e.to_string())?;
            }
            changed.push((p, all));
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(changed)
    }
    pub fn path_by_id(&self, id: &str) -> Option<String> {
        self.conn
            .query_row(
                "SELECT path FROM files WHERE id LIKE ?1 || '%' ORDER BY path LIMIT 1",
                params![id],
                |r| r.get(0),
            )
            .ok()
    }
    /// Every `phash2` row of one algorithm whose path is in this scope, as (path, bytes): the perceptual hashes
    /// (`similar::ALG`) and the CLIP vectors (`propose::ALG`) share the table.
    pub fn cached_blobs(&self, alg: &str) -> Result<Vec<(String, Vec<u8>)>, String> {
        let mut st = self
            .conn
            .prepare("SELECT path, hash FROM phash2 WHERE alg=?1")
            .map_err(|e| e.to_string())?;
        let v = st
            .query_map([alg], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .filter(|(p, _)| self.scope.contains(Path::new(p)))
            .collect();
        Ok(v)
    }
    /// Every tag row as (path, tag, source), source being `xmp`, `folder` or `implied`.
    pub fn tag_rows(&self) -> Result<Vec<(String, String, String)>, String> {
        let mut st = self
            .conn
            .prepare("SELECT path, tag, source FROM tags")
            .map_err(|e| e.to_string())?;
        let rows = st
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        Ok(rows)
    }
    pub fn tag_counts(&self) -> Result<Vec<(String, i64)>, String> {
        let mut counts = HashMap::<String, i64>::new();
        for row in self.all()? {
            for tag in row.tags {
                *counts.entry(tag).or_default() += 1;
            }
        }
        let mut values: Vec<_> = counts.into_iter().collect();
        values.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(values)
    }
}

/// Everything the writer needs for one file, computed off the main thread.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Scan {
    pub row: FileRow,
    pub xmp_tags: Vec<String>,
    pub folder_tags: Vec<String>,
    pub text: Option<String>,
    pub manual: bool,
}

/// Cheap: read the file once, parse the XMP packet, header-only dimensions, byte hash for a provisional id. No pixel decoding.
pub fn scan_file(root: &Path, path: &Path, vocab: &Vocab) -> Result<Scan, String> {
    use std::os::unix::fs::MetadataExt;
    let before = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let md = std::fs::metadata(path).map_err(|e| e.to_string())?;
    // A file rewritten while it was being read (the batch helper or a Save through the mount, with the pull worker
    // scanning beside them) would go into the index torn, under the size and time the finished write leaves, so no
    // later pull would look at it again. The write moves mtime while it runs and ctime when it restores mtime, so a
    // size or time that differs across the read is the overlap showing; the row is left for the next pull.
    let stamp = |m: &std::fs::Metadata| (m.len(), m.modified().ok(), m.ctime(), m.ctime_nsec());
    if stamp(&before) != stamp(&md) {
        return Err(format!(
            "{}: changed while it was being read; scanned again next time",
            path.display()
        ));
    }
    Ok(scan_bytes(root, path, &bytes, &md, vocab))
}
/// The scan itself, from bytes and metadata already in hand.
pub fn scan_bytes(
    root: &Path,
    path: &Path,
    bytes: &[u8],
    md: &std::fs::Metadata,
    vocab: &Vocab,
) -> Scan {
    let bytes = &bytes;
    let kind = containers::sniff(bytes);
    let packet = containers::get_xmp(bytes).unwrap_or(None);
    let parsed = packet
        .as_deref()
        .map(xmp::read)
        .transpose()
        .unwrap_or_else(|e| {
            eprintln!("warn {}: {e}", path.display());
            None
        })
        .unwrap_or_default();
    let rel = path
        .strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let created_at = parsed
        .fields
        .get("origMtime")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(mtime);
    // id: the one written into the file if tagged; otherwise a provisional hash of the container bytes ("b…"), replaced by the pixel hash on first tag
    let id = parsed
        .fields
        .get("id")
        .cloned()
        .unwrap_or_else(|| crate::writer::byte_id(bytes));
    let (w, h) = match kind {
        containers::Kind::Avif => probe_dimensions(path),
        k if k.is_image() => header_dimensions(bytes),
        _ => (0, 0),
    };
    let format = match kind {
        containers::Kind::Other => Path::new(&rel)
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default(),
        containers::Kind::Png if containers::is_apng(bytes) => "apng".to_string(),
        k => k.label().to_string(),
    };
    let kind_s = if kind.is_video() {
        "video"
    } else if kind.is_image() {
        "image"
    } else {
        "other"
    };
    let mut folder_tags: Vec<String> = vec![];
    for comp in Path::new(&rel)
        .parent()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
    {
        if comp != "." && !comp.is_empty() {
            folder_tags.push(format!("folder:{}", comp.to_ascii_lowercase()));
        }
    }
    let xmp_tags: Vec<String> = parsed.tags.iter().map(|t| vocab.canon(t)).collect();
    let all: BTreeSet<String> =
        vocab.expand(xmp_tags.iter().cloned().chain(folder_tags.iter().cloned()));
    let tagged_at = parsed
        .fields
        .get("taggedAt")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(0.0);
    Scan {
        row: FileRow {
            id,
            path: rel,
            format,
            kind: kind_s.into(),
            width: w,
            height: h,
            size: md.len() as i64,
            mtime,
            created_at,
            tagged_at,
            xmp_tag_count: xmp_tags.len() as i64,
            tags: all,
            text: parsed.fields.get("text").cloned().unwrap_or_default(),
        },
        xmp_tags,
        folder_tags,
        text: parsed.fields.get("text").cloned(),
        manual: parsed.fields.get("textSource").map(String::as_str) == Some("manual"),
    }
}

/// Dimensions from ffprobe, for the containers the image crate cannot read the header of (AVIF).
fn probe_dimensions(path: &Path) -> (i64, i64) {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output();
    let Ok(out) = out else { return (0, 0) };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut it = text.trim().split(',').map(|v| v.trim().parse::<i64>().ok());
    match (it.next().flatten(), it.next().flatten()) {
        (Some(w), Some(h)) => (w, h),
        _ => (0, 0),
    }
}

fn header_dimensions(bytes: &[u8]) -> (i64, i64) {
    image::ImageReader::new(std::io::Cursor::new(containers::trailer_strip(bytes)))
        .with_guessed_format()
        .ok()
        .and_then(|r| r.into_dimensions().ok())
        .map(|(w, h)| (w as i64, h as i64))
        .unwrap_or((0, 0))
}

fn write_row(tx: &rusqlite::Transaction, s: &Scan) -> Result<(), String> {
    let r = &s.row;
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    tx.execute("DELETE FROM tags WHERE path=?1", params![r.path])
        .map_err(|e| e.to_string())?;
    // A different id of the same kind means different pixels: the perceptual hash and the machine OCR text belong to
    // the old picture. Ids that differ only in kind (the provisional byte id `b…` becomes the pixel hash on the first
    // tag) say nothing about the pixels, so those caches stay (review, 2026-09-22: a first tag used to drop the hash,
    // and a replaced picture kept the old picture's text forever). The kind is the length: a pixel hash is 16 hex
    // characters, the provisional ids a letter plus 16. Not the first character: one pixel hash in sixteen starts
    // with "b" as well, and telling the kinds apart that way cost two files their text on 2026-09-22.
    for t in ["phash2", "text", "text_meta"] {
        tx.execute(
            &format!(
                "DELETE FROM {t} WHERE path=?1 AND EXISTS (SELECT 1 FROM files WHERE path=?1 AND id<>?2 \
                 AND (length(id)=16) = (length(?2)=16))"
            ),
            params![r.path, r.id],
        )
        .map_err(|e| e.to_string())?;
    }
    tx.execute("DELETE FROM files WHERE path=?1", params![r.path])
        .map_err(|e| e.to_string())?;
    // text_in_file: whether this scan found meme:text in the file's own XMP (manual or machine). Distinguishes text that
    // is embedded in the file from text that lives only in this index; `ocr --status` counts the latter as the embed gap.
    let text_in_file = s.text.is_some() as i64;
    tx.execute("INSERT INTO files(id,path,format,kind,width,height,size,mtime,created_at,tagged_at,xmp_tag_count,indexed_at,text_in_file) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![r.id, r.path, r.format, r.kind, r.width, r.height, r.size, r.mtime, r.created_at, r.tagged_at, r.xmp_tag_count, now, text_in_file]).map_err(|e| e.to_string())?;
    for t in &s.xmp_tags {
        tx.execute(
            "INSERT OR IGNORE INTO tags VALUES(?1,?2,'xmp')",
            params![r.path, t],
        )
        .map_err(|e| e.to_string())?;
    }
    for t in &s.folder_tags {
        tx.execute(
            "INSERT OR IGNORE INTO tags VALUES(?1,?2,'folder')",
            params![r.path, t],
        )
        .map_err(|e| e.to_string())?;
    }
    for t in r
        .tags
        .iter()
        .filter(|t| !s.xmp_tags.contains(t) && !s.folder_tags.contains(t))
    {
        tx.execute(
            "INSERT OR IGNORE INTO tags VALUES(?1,?2,'implied')",
            params![r.path, t],
        )
        .map_err(|e| e.to_string())?;
    }
    if s.manual {
        tx.execute(
            "INSERT OR REPLACE INTO manual_text VALUES(?1,?2)",
            params![r.path, s.text.as_deref().unwrap_or("")],
        )
        .map_err(|e| e.to_string())?;
    } else {
        tx.execute("DELETE FROM manual_text WHERE path=?1", params![r.path])
            .map_err(|e| e.to_string())?;
        if let Some(txt) = &s.text {
            tx.execute("DELETE FROM text WHERE path=?1", params![r.path])
                .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT INTO text(path, body) VALUES(?1,?2)",
                params![r.path, txt],
            )
            .map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fmt_time_is_utc_ymd_hm() {
        assert_eq!(fmt_time(0.0), "1970-01-01 00:00Z");
        assert_eq!(fmt_time(1_700_000_000.0), "2023-11-14 22:13Z"); // a known instant
        assert_eq!(fmt_time(1_582_934_400.0), "2020-02-29 00:00Z"); // leap day exists in 2020
    }
    #[test]
    fn caches_follow_the_picture_not_the_first_tag() {
        let dir = std::env::temp_dir().join(format!("memetag-idkind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("index.sqlite")).unwrap();
        let scan = |id: &str| Scan {
            row: FileRow {
                id: id.into(),
                path: "a.png".into(),
                format: "png".into(),
                kind: "image".into(),
                width: 8,
                height: 8,
                size: 100,
                mtime: 1.0,
                created_at: 1.0,
                tagged_at: 0.0,
                xmp_tag_count: 0,
                tags: BTreeSet::new(),
                text: String::new(),
            },
            xmp_tags: vec![],
            folder_tags: vec![],
            text: None,
            manual: false,
        };
        let cached = |db: &Db| -> (i64, i64, i64) {
            let n = |q: &str| db.conn.query_row(q, [], |r| r.get::<_, i64>(0)).unwrap();
            (
                n("SELECT count(*) FROM text WHERE path='a.png'"),
                n("SELECT count(*) FROM text_meta WHERE path='a.png'"),
                n("SELECT count(*) FROM phash2 WHERE path='a.png'"),
            )
        };
        let fill = |db: &Db| {
            db.conn
                .execute(
                    "INSERT OR REPLACE INTO text(path, body) VALUES('a.png','machine text')",
                    [],
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT OR REPLACE INTO text_meta VALUES('a.png','deepseek-ocr',1.0,1.0)",
                    [],
                )
                .unwrap();
            db.conn
                .execute(
                    "INSERT OR REPLACE INTO phash2 VALUES('a.png','dct16m',x'00')",
                    [],
                )
                .unwrap();
        };
        db.import_scan(scan("b0123456789abcdef")).unwrap(); // untagged: the provisional byte id
        fill(&db);
        db.import_scan(scan("0123456789abcdef")).unwrap(); // first tag: the pixel id, same pixels
        assert_eq!(
            cached(&db),
            (1, 1, 1),
            "a first tag keeps the OCR text and the hash"
        );
        db.import_scan(scan("fedcba9876543210")).unwrap(); // a different picture under the same name
        assert_eq!(
            cached(&db),
            (0, 0, 0),
            "a replaced picture takes the old picture's caches with it"
        );
        fill(&db);
        db.import_scan(scan("b1111111111111111")).unwrap(); // tags stripped again: back to a byte id
        assert_eq!(
            cached(&db),
            (1, 1, 1),
            "a change of id kind says nothing about the pixels"
        );
        // the 2026-09-22 regression: a pixel hash that happens to start with "b" is still a pixel hash
        db.import_scan(scan("b00a1a35f811742e")).unwrap(); // first tag again, and the hash begins with b
        assert_eq!(
            cached(&db),
            (1, 1, 1),
            "a pixel hash starting with b is not a byte id"
        );
        db.import_scan(scan("b00a1a35f811742e")).unwrap(); // saved again, unchanged
        assert_eq!(cached(&db), (1, 1, 1));
        db.import_scan(scan("c00a1a35f811742e")).unwrap(); // a real pixel change still clears the caches
        assert_eq!(cached(&db), (0, 0, 0));
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn text_in_file_tracks_whether_the_xmp_carried_text() {
        // A scan sets files.text_in_file from whether the file's own XMP held meme:text, which is the difference
        // between OCR text living only in the index (the embed gap) and text embedded in the file itself.
        let dir = std::env::temp_dir().join(format!("memetag-textinfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("index.sqlite")).unwrap();
        let scan = |text: Option<&str>, manual: bool| Scan {
            row: FileRow {
                id: "0123456789abcdef".into(),
                path: "a.png".into(),
                format: "png".into(),
                kind: "image".into(),
                width: 8,
                height: 8,
                size: 100,
                mtime: 1.0,
                created_at: 1.0,
                tagged_at: 0.0,
                xmp_tag_count: 0,
                tags: BTreeSet::new(),
                text: String::new(),
            },
            xmp_tags: vec![],
            folder_tags: vec![],
            text: text.map(str::to_string),
            manual,
        };
        let flag = |db: &Db| -> i64 {
            db.conn
                .query_row(
                    "SELECT text_in_file FROM files WHERE path='a.png'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        // No text in the file: 0. Then OCR fills the index only, still 0 — this is the gap.
        db.import_scan(scan(None, false)).unwrap();
        assert_eq!(flag(&db), 0, "a file with no XMP text is not embedded");
        db.conn
            .execute(
                "INSERT INTO text(path, body) VALUES('a.png','machine read')",
                [],
            )
            .unwrap();
        assert_eq!(
            flag(&db),
            0,
            "OCR into the index alone does not embed the file"
        );
        assert_eq!(
            crate::ocr::embed_pending(&db).unwrap(),
            1,
            "one image awaits embedding"
        );
        // A later scan finds the text now in the file (as the embed pass + a pull would): flips to 1, gap closes.
        db.import_scan(scan(Some("machine read"), false)).unwrap();
        assert_eq!(
            flag(&db),
            1,
            "a scan that sees meme:text marks the file embedded"
        );
        assert_eq!(
            crate::ocr::embed_pending(&db).unwrap(),
            0,
            "nothing awaits embedding now"
        );
        // Reviewed text is in the file by definition and never counts toward the gap, embedded flag or not.
        db.import_scan(scan(Some("human fixed"), true)).unwrap();
        assert_eq!(
            crate::ocr::embed_pending(&db).unwrap(),
            0,
            "reviewed text is not an embed gap"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn reimply_rebuilds_derived_rows_from_stored_ones() {
        let dir = std::env::temp_dir().join(format!("memetag-reimply-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&dir.join("index.sqlite")).unwrap();
        for (p, t, s) in [
            ("a.png", "twitter post", "xmp"),
            ("a.png", "folder:old", "folder"),
            ("b.png", "cat", "xmp"),
            ("c.png", "twitter collage", "xmp"),
            ("c.png", "stale", "implied"),
        ] {
            db.conn
                .execute("INSERT INTO tags VALUES(?1,?2,?3)", params![p, t, s])
                .unwrap();
        }
        let mut v = Vocab::default();
        v.add_rule("twitter post", "twitter");
        v.add_rule("twitter", "social media");
        let mut changed = db.reimply(&v).unwrap();
        changed.sort();
        assert_eq!(
            changed,
            vec![
                (
                    "a.png".to_string(),
                    ["twitter post", "folder:old", "twitter", "social media"]
                        .into_iter()
                        .map(String::from)
                        .collect()
                ),
                (
                    "c.png".to_string(),
                    ["twitter collage"].into_iter().map(String::from).collect()
                ),
            ]
        );
        let implied: Vec<(String, String)> = db
            .conn
            .prepare("SELECT path, tag FROM tags WHERE source='implied' ORDER BY path, tag")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            implied,
            vec![
                ("a.png".into(), "social media".into()),
                ("a.png".into(), "twitter".into())
            ]
        );
        assert!(
            db.reimply(&v).unwrap().is_empty(),
            "a second pass changes nothing"
        );
        v.remove_rule("twitter post", "twitter");
        assert_eq!(db.reimply(&v).unwrap().len(), 1);
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT COUNT(*) FROM tags WHERE source='implied'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
