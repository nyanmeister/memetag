//! `memetag pull` — bring the desktop index up to date with the collection on the server without a full
//! reindex through the mount. The server lists every file (path, size, mtime) and scans only the ones the
//! index does not know or knows with a different size/mtime; the desktop merges those rows, carries OCR
//! text across renames by content id, drops rows whose files are gone, and builds thumbnails for what is new.
//!
//! Change detection is an exact key: size and mtime as the server's own Rust computes them (the same
//! `as_secs_f64` the index stored), so nothing is guessed. Text is never touched for a file that merely
//! changed bytes (a tag edit keeps the pixels); the OCR pass decides what to reread.
//!
//! For file safety this must not run while an OCR pass is writing the index —
//! `memetag-ocr.service` active refuses the pull unless `--force`.
use crate::index::{Db, FileRow, Scan};
use crate::remote::{configured as remote, Purpose};
use crate::vocab::Vocab;
use crate::vocabsync::Canonical; // `store.name()` in vocab_step is a trait method
use crate::Cfg;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::UNIX_EPOCH;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub path: String,
    pub size: i64,
    pub mtime: f64,
}

#[derive(Serialize, Deserialize)]
enum Request {
    List {
        root: PathBuf,
        #[serde(default)]
        scope: Option<crate::library::Scope>,
    },
    Scan {
        root: PathBuf,
        path: String,
        vocab: Vocab,
    },
}
#[derive(Serialize, Deserialize)]
enum Reply {
    List(Vec<Entry>),
    Scan(Scan),
}

/// Same walk as `Db::reindex`: every regular file under the root, hidden directories included.
#[cfg(test)]
fn list(root: &Path) -> Result<Vec<Entry>, String> {
    list_selected(root, &crate::library::Scope::default())
}

fn list_selected(root: &Path, scope: &crate::library::Scope) -> Result<Vec<Entry>, String> {
    scope.validate()?;
    if !root.is_dir() {
        return Err(format!("{}: not a directory", root.display()));
    }
    let mut out = vec![];
    // An entry that cannot be read is not absent. Swallowing it here would report the file as gone and the
    // desktop would drop its row, OCR text and hash (review, 2026-09-22); so any read failure fails the listing.
    let (mut unreadable, mut first) = (0usize, None::<String>);
    for r in walkdir::WalkDir::new(root).into_iter().filter_entry(|e| {
        e.depth() == 0
            || !e.file_type().is_dir()
            || scope.traverse(e.path().strip_prefix(root).unwrap_or(e.path()))
    }) {
        let e = match r {
            Ok(e) => e,
            Err(err) => {
                unreadable += 1;
                first.get_or_insert_with(|| err.to_string());
                continue;
            }
        };
        if !e.file_type().is_file()
            || !scope.contains(e.path().strip_prefix(root).unwrap_or(e.path()))
        {
            continue;
        }
        let md = match e.metadata() {
            Ok(m) => m,
            Err(err) => {
                unreadable += 1;
                first.get_or_insert_with(|| format!("{}: {err}", e.path().display()));
                continue;
            }
        };
        let mtime = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let rel = e
            .path()
            .strip_prefix(root)
            .unwrap_or(e.path())
            .to_string_lossy()
            .into_owned();
        out.push(Entry {
            path: rel,
            size: md.len() as i64,
            mtime,
        });
    }
    if unreadable > 0 {
        return Err(format!(
            "listing: {unreadable} entries could not be read ({}); nothing was dropped",
            first.unwrap_or_default()
        ));
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// More rows gone than a night's housekeeping accounts for: an empty mountpoint, a dataset that did not import, a
/// root pointed somewhere else. Dropping them would take the OCR text and hashes along; `--force` says it is meant.
fn too_many_gone(gone: usize, local: usize) -> bool {
    gone > 50 && gone * 20 > local
}

fn handle(req: Request) -> Result<Reply, String> {
    match req {
        Request::List { root, scope } => {
            list_selected(&root, &scope.unwrap_or_default()).map(Reply::List)
        }
        Request::Scan { root, path, vocab } => {
            if !root.is_absolute()
                || path.is_empty()
                || !Path::new(&path)
                    .components()
                    .all(|p| matches!(p, Component::Normal(_)))
            {
                return Err("Invalid pull file path".into());
            }
            let full = root.join(&path);
            if !full
                .canonicalize()
                .map_err(|e| e.to_string())?
                .starts_with(root.canonicalize().map_err(|e| e.to_string())?)
            {
                return Err("File resolves outside the collection".into());
            }
            crate::index::scan_file(&root, &full, &vocab).map(Reply::Scan)
        }
    }
}

/// Server side: one JSON request per stdin line, one JSON `Result<Reply, String>` per stdout line. EOF ends it.
pub fn worker() -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        let result = serde_json::from_str::<Request>(&line)
            .map_err(|e| e.to_string())
            .and_then(handle);
        serde_json::to_writer(&mut stdout, &result).map_err(|e| e.to_string())?;
        writeln!(stdout).map_err(|e| e.to_string())?;
        stdout.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The `[pull_remote]` host, for the vocab sync's own side-channel ssh (a plain `ssh host cat`, not the worker
/// protocol). `None` when the root is local and no server is configured — then there is nothing to sync against.
pub fn remote_host(c: &Cfg) -> Result<Option<String>, String> {
    Ok(remote(c, Purpose::Pull)?.map(|r| r.host))
}

/// Where listings and scans come from: the configured server worker over one ssh session, or this process.
/// A worker ssh that closed its stdout before answering, named by its cause. ssh exits 255 when it could not open the
/// session at all (host down or asleep, connection refused or timed out, auth rejected) and 127 when the session opened
/// but the helper command was not found — so an unreachable server and a missing helper stop saying the same thing.
pub fn unreachable_msg(host: &str, kind: &str) -> String {
    format!("Could not reach the {kind} server {host} — is it awake and on the network?")
}
pub fn worker_down(host: &str, kind: &str, status: Option<std::process::ExitStatus>) -> String {
    match status.and_then(|s| s.code()) {
        Some(255) => unreachable_msg(host, kind),
        Some(127) => format!("The {kind} helper is not installed on {host} (see PORTABILITY.md)"),
        _ => format!("The {kind} worker on {host} disconnected before answering"),
    }
}

struct Source {
    root: PathBuf,
    host: Option<String>,
    child: Option<Child>,
    input: Option<ChildStdin>,
    output: Option<BufReader<ChildStdout>>,
}
impl Source {
    fn open(c: &Cfg, connect_timeout_secs: u32) -> Result<Source, String> {
        let Some(r) = remote(c, Purpose::Pull)? else {
            return Ok(Source {
                root: c.root.clone(),
                host: None,
                child: None,
                input: None,
                output: None,
            });
        };
        let mut child = crate::remote::ssh("ssh", &r.host, &r.command, connect_timeout_secs)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("Start pull worker: {e}"))?;
        let input = child.stdin.take();
        let output = child.stdout.take().map(BufReader::new);
        Ok(Source {
            root: r.root,
            host: Some(r.host),
            child: Some(child),
            input,
            output,
        })
    }
    fn call(&mut self, req: Request) -> Result<Reply, String> {
        let (Some(input), Some(output)) = (&mut self.input, &mut self.output) else {
            return handle(req);
        };
        // any write/read failure here is the ssh worker gone — a connect that timed out or was refused (host asleep),
        // or a broken pipe from a helper that exited. Wait it out and name the real cause instead of blaming the helper.
        let line = (|| -> Result<Option<String>, String> {
            serde_json::to_writer(&mut *input, &req).map_err(|e| e.to_string())?;
            writeln!(input).map_err(|e| e.to_string())?;
            input.flush().map_err(|e| e.to_string())?;
            let mut line = String::new();
            if output.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                return Ok(None);
            }
            Ok(Some(line))
        })();
        match line {
            Ok(Some(line)) => serde_json::from_str::<Result<Reply, String>>(&line)
                .map_err(|e| format!("Pull worker response: {e}"))?,
            _ => {
                let status = self.child.as_mut().and_then(|c| c.wait().ok());
                Err(worker_down(
                    self.host.as_deref().unwrap_or("the server"),
                    "pull",
                    status,
                ))
            }
        }
    }
}
impl Drop for Source {
    fn drop(&mut self) {
        drop(self.input.take());
        if let Some(mut c) = self.child.take() {
            let _ = c.wait();
        }
    }
}

#[derive(Default, Debug, PartialEq)]
pub struct Plan {
    pub new: Vec<Entry>,
    pub changed: Vec<(Entry, Entry)>,
    pub gone: Vec<String>,
}
/// Pure diff: an index row matches a listing entry only when size and mtime are identical.
pub fn plan(local: &[Entry], server: &[Entry]) -> Plan {
    let known: HashMap<&str, &Entry> = local.iter().map(|e| (e.path.as_str(), e)).collect();
    let mut p = Plan::default();
    for e in server {
        match known.get(e.path.as_str()) {
            None => p.new.push(e.clone()),
            Some(k) if k.size != e.size || k.mtime.to_bits() != e.mtime.to_bits() => {
                p.changed.push(((*k).clone(), e.clone()))
            }
            Some(_) => {}
        }
    }
    let present: HashMap<&str, ()> = server.iter().map(|e| (e.path.as_str(), ())).collect();
    p.gone = local
        .iter()
        .filter(|e| !present.contains_key(e.path.as_str()))
        .map(|e| e.path.clone())
        .collect();
    p
}

fn local_entries(db: &Db) -> Result<Vec<Entry>, String> {
    let mut st = db
        .conn
        .prepare("SELECT path,size,mtime FROM files")
        .map_err(|e| e.to_string())?;
    let v: Vec<Entry> = st
        .query_map([], |r| {
            Ok(Entry {
                path: r.get(0)?,
                size: r.get(1)?,
                mtime: r.get(2)?,
            })
        })
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    Ok(v)
}

/// Images the engine has not read and nobody typed text for; `since_last_pass` keeps only those indexed after the engine's latest write.
fn unread(db: &Db, engine: &str, since_last_pass: bool) -> Result<i64, String> {
    let newer = if since_last_pass {
        " AND f.indexed_at > COALESCE((SELECT max(m.at) FROM text_meta m WHERE m.engine=?1), 0)"
    } else {
        ""
    };
    db.conn.query_row(&format!("SELECT count(*) FROM files f WHERE f.kind='image' AND f.path NOT IN (SELECT path FROM excluded_files) AND NOT EXISTS (SELECT 1 FROM text_meta m WHERE m.path=f.path AND m.engine=?1) AND NOT EXISTS (SELECT 1 FROM manual_text x WHERE x.path=f.path){newer}"), [engine], |r| r.get(0)).map_err(|e| e.to_string())
}

fn ocr_service_active() -> bool {
    Command::new("systemctl")
        .args(["--user", "is-active", "--quiet", "memetag-ocr.service"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn run(c: &Cfg, args: &[String]) -> Result<(), String> {
    let (mut dry, mut force, mut thumbs, mut start_ocr) = (false, false, true, false);
    for a in args {
        match a.as_str() {
            "--dry-run" => dry = true,
            "--force" => force = true,
            "--no-thumbs" => thumbs = false,
            "--start-ocr" => start_ocr = true,
            x => return Err(format!("pull: unknown flag {x}")),
        }
    }
    if !dry && !force && ocr_service_active() {
        return Err("memetag-ocr.service is running and writes this index; wait for the pass to finish (or --dry-run to look, --force to insist)".into());
    }
    let o = sync(c, dry, thumbs, start_ocr, force, 10)?;
    if o.failed > 0 {
        return Err(format!(
            "pull: {} files could not be scanned; their rows and any gone rows were left for next time",
            o.failed
        ));
    }
    Ok(())
}

/// What a merge touched: paths written (new or changed) and paths dropped — what the open grid needs to catch up.
#[derive(Default, Debug)]
pub struct Outcome {
    pub written: Vec<String>,
    pub gone: Vec<String>,
    /// scans that failed (transport or worker); gone rows are kept whenever this is nonzero
    pub failed: usize,
    /// set when the vocab step found the rules changed on both sides since the last sync: the grid opens the resolve
    /// card so the conflict is seen in the window, not only on stderr (Stage 2 of the sync, 2026-09-26)
    pub vocab_conflict: Option<crate::vocabsync::Conflict>,
    /// Rules may have changed even if no media changed (or the later file pull failed).
    pub vocab_changed: bool,
}

/// `memetag grab` opens on the index as it is and runs this on a thread: the same merge, without thumbnails (the grid
/// makes them on demand) and without starting OCR; the grid folds the outcome in when it arrives. Skipped, not failed,
/// while the OCR pass writes the index or when the server is unreachable. Background refresh avoids delaying the window.
pub fn for_grab(c: &Cfg) -> Outcome {
    if ocr_service_active() {
        eprintln!("pull skipped: memetag-ocr.service is writing the index; new files appear after the pass");
        return Outcome::default();
    }
    // a short connect timeout: with the server asleep the grid should open in seconds, not wait out the nightly pull's 10
    let mut outcome = match sync(c, false, false, false, false, 3) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("pull skipped: {e} — showing the index as it is");
            Outcome::default()
        }
    };
    // Check even after an error: vocabulary sync precedes the media worker connection.
    outcome.vocab_changed = c.vocab.path.as_ref().is_some_and(|p| {
        let current = Vocab::load(p);
        current.broken.is_none()
            && crate::vocabsync::fingerprint(&current) != crate::vocabsync::fingerprint(&c.vocab)
    });
    outcome
}

/// `memetag serve` runs this on a thread when a page is asked for and the last pull is old: the grab-time merge,
/// but with thumbnails, since the page has no window to make them in later. Same skips as `for_grab`.
pub fn for_serve(c: &Cfg) -> Outcome {
    if ocr_service_active() {
        eprintln!("pull skipped: memetag-ocr.service is writing the index");
        return Outcome::default();
    }
    match sync(c, false, true, false, false, 3) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("pull skipped: {e}");
            Outcome::default()
        }
    }
}

/// Bring the alias/implication rules level with the server before scanning (graph note "Sync Implications data",
/// 2026-09-26). Non-interactive here: a conflict or any vocab-side failure is reported and the file pull goes on with
/// the rules as they are — the conflict waits for `memetag vocab sync` or the Implications card. Returns true when the
/// local vocab.toml was replaced (a pull-down), so the caller reapplies the implications to the index; the conflict,
/// if any, so the grid can raise the resolve card (the CLI ignores it — it is already on stderr); and whether the ssh
/// could not reach the host at all, so the caller skips its own worker connect to the same host instead of paying a
/// second timeout (the vocab ssh runs first, so it is the one that discovers the server is asleep).
struct VocabStep {
    reimply: bool,
    conflict: Option<crate::vocabsync::Conflict>,
    unreachable: bool,
}
fn vocab_step(c: &Cfg, connect_timeout_secs: u32) -> VocabStep {
    let none = |unreachable| VocabStep {
        reimply: false,
        conflict: None,
        unreachable,
    };
    if c.active_source.as_deref().is_some_and(|id| id != "main") {
        return none(false);
    }
    let mut store = match crate::vocabsync::store_for(c) {
        Ok(Some(s)) => s,
        Ok(None) => return none(false),
        Err(e) => {
            eprintln!("vocab: {e}");
            return none(false);
        }
    };
    store.connect_timeout = connect_timeout_secs;
    let env = crate::vocabsync::Env::from_home();
    match crate::vocabsync::sync(&store, &env) {
        Ok(o) => match o {
            crate::vocabsync::Outcome::InSync => none(false),
            crate::vocabsync::Outcome::Seeded => {
                println!("vocab: seeded {} from your rules", store.name());
                none(false)
            }
            crate::vocabsync::Outcome::Pushed => {
                println!("vocab: pushed your rule changes to {}", store.name());
                none(false)
            }
            crate::vocabsync::Outcome::Pulled => {
                println!("vocab: pulled {}'s rule changes", store.name());
                VocabStep {
                    reimply: true,
                    conflict: None,
                    unreachable: false,
                }
            }
            crate::vocabsync::Outcome::Conflict(cf) => {
                eprintln!(
                    "vocab: your rules and {}'s both changed since the last sync — kept yours for this pull; \
                     resolve in the Implications card or with `memetag vocab sync`",
                    store.name()
                );
                for l in &cf.diff.only_local {
                    eprintln!("  only here:   {l}");
                }
                for l in &cf.diff.only_canon {
                    eprintln!("  only server: {l}");
                }
                VocabStep {
                    reimply: false,
                    conflict: Some(cf),
                    unreachable: false,
                }
            }
        },
        // an unreachable host is reported once by the caller (it skips the worker too); other failures are noted here
        Err(e) if crate::vocabsync::is_unreachable(&e) => none(true),
        Err(e) => {
            eprintln!("vocab: sync skipped ({e})");
            none(false)
        }
    }
}

/// The merge itself; `run` and `for_grab` decide whether it may happen.
fn sync(
    c: &Cfg,
    dry: bool,
    thumbs: bool,
    start_ocr: bool,
    force: bool,
    connect_timeout_secs: u32,
) -> Result<Outcome, String> {
    if c.active_source.is_none() {
        if let Some(library) = c.library()? {
            let mut outcome = Outcome::default();
            for source in library
                .sources
                .iter()
                .filter(|s| s.enabled && !s.scope.include.is_empty())
            {
                println!("source: {} ({})", source.name, source.id);
                match sync(
                    &c.for_source(&source.id)?,
                    dry,
                    thumbs,
                    start_ocr,
                    force,
                    connect_timeout_secs,
                ) {
                    Ok(o) => {
                        outcome.written.extend(o.written);
                        outcome.gone.extend(o.gone);
                        outcome.failed += o.failed;
                        outcome.vocab_changed |= o.vocab_changed;
                        if o.vocab_conflict.is_some() {
                            outcome.vocab_conflict = o.vocab_conflict;
                        }
                    }
                    Err(e) => {
                        eprintln!("{}: {e}; cached rows retained", source.name);
                        outcome.failed += 1;
                    }
                }
            }
            return Ok(outcome);
        }
    }
    let db = Db::open_cfg(&c)?;
    if let (Some(library), Some(id)) = (c.library()?, c.active_source.as_deref()) {
        if !library.source(id)?.available() {
            return Err("Source is offline or replaced; use Locate to confirm its location".into());
        }
    }
    // Level the rules with the server before scanning. On a dry run, only say what a sync would do; write nothing.
    let mut vocab_conflict = None;
    let extra_source = c.active_source.as_deref().is_some_and(|id| id != "main");
    let effective_vocab = if extra_source {
        c.vocab
            .path
            .as_ref()
            .map(|p| Vocab::load(p))
            .unwrap_or_else(|| c.vocab.clone())
    } else if dry {
        match crate::vocabsync::store_for(c) {
            Ok(Some(mut s)) => {
                s.connect_timeout = connect_timeout_secs;
                match crate::vocabsync::sync_dry(&s, &crate::vocabsync::Env::from_home()) {
                    Ok(msg) => println!("vocab: {msg}"),
                    Err(e) => eprintln!("vocab: {e}"),
                }
            }
            Ok(None) => {}
            Err(e) => eprintln!("vocab: {e}"),
        }
        c.vocab.clone()
    } else {
        let step = vocab_step(c, connect_timeout_secs);
        // the vocab ssh runs first; if it could not reach the host, the worker's ssh to the same host would only time
        // out again, so stop here with one clear message instead of a second wait (2026-09-26)
        if step.unreachable {
            let host = remote(c, Purpose::Pull)?
                .map(|r| r.host)
                .unwrap_or_else(|| "the server".into());
            return Err(unreachable_msg(&host, "pull"));
        }
        vocab_conflict = step.conflict;
        if step.reimply {
            let v = crate::vocab::Vocab::load(&crate::vocabsync::Env::from_home().vocab_path);
            match db.reimply(&v) {
                Ok(changed) => println!(
                    "vocab: implications reapplied, {} files changed in the index",
                    changed.len()
                ),
                Err(e) => eprintln!("vocab: reapply failed ({e})"),
            }
            v
        } else {
            c.vocab.clone()
        }
    };
    let t0 = std::time::Instant::now();
    let local: Vec<_> = local_entries(&db)?
        .into_iter()
        .filter(|e| db.scope.contains(Path::new(&e.path)))
        .collect();
    let mut src = Source::open(c, connect_timeout_secs)?;
    let worker_scope = match (c.library()?, c.active_source.as_deref()) {
        (Some(library), Some(id)) => library.source(id)?.scope.clone(),
        _ => db.scope.clone(),
    };
    let Reply::List(mut server) = src.call(Request::List {
        root: src.root.clone(),
        scope: Some(worker_scope.clone()),
    })?
    else {
        return Err("Pull worker: unexpected reply to List".into());
    };
    for entry in &server {
        crate::sources::relative(Path::new(&entry.path))?;
    }
    c.ensure_current()?;
    server.retain(|e| worker_scope.contains(Path::new(&e.path)));
    if let Some(id) = &c.active_source {
        for entry in &mut server {
            entry.path = format!("{id}/{}", entry.path);
        }
    }
    if server.is_empty() && !local.is_empty() {
        return Err("Empty source listing; cached files retained. Check that the source is mounted and available".into());
    }
    let mut p = plan(&local, &server);
    if too_many_gone(p.gone.len(), local.len()) && !force {
        eprintln!(
            "pull: {} of {} indexed files are missing from the listing; that is more than housekeeping, so their rows \
             (with OCR text and hashes) are kept. Check the server's mount, or run `memetag pull --force` if it is meant.",
            p.gone.len(),
            local.len()
        );
        p.gone.clear();
    }
    println!(
        "pull: server lists {} files, index has {} — {} new, {} changed, {} gone ({:.1}s)",
        server.len(),
        local.len(),
        p.new.len(),
        p.changed.len(),
        p.gone.len(),
        t0.elapsed().as_secs_f64()
    );
    if p.new.is_empty() && p.changed.is_empty() && p.gone.is_empty() {
        println!("pull: nothing to do — the index matches the server");
        return Ok(Outcome {
            vocab_conflict,
            ..Outcome::default()
        });
    }
    if dry {
        for e in p.new.iter().take(20) {
            println!("  new      {}", e.path);
        }
        if p.new.len() > 20 {
            println!("  new      … and {} more", p.new.len() - 20);
        }
        for (was, now) in p.changed.iter().take(20) {
            println!(
                "  changed  {}  size {}→{}  mtime {:?}→{:?}",
                now.path, was.size, now.size, was.mtime, now.mtime
            );
        }
        if p.changed.len() > 20 {
            println!("  changed  … and {} more", p.changed.len() - 20);
        }
        for g in p.gone.iter().take(20) {
            println!("  gone     {g}");
        }
        if p.gone.len() > 20 {
            println!("  gone     … and {} more", p.gone.len() - 20);
        }
        println!("pull: dry run, nothing written");
        return Ok(Outcome::default());
    }
    // what the gone rows carried, by content id, so a rename keeps its OCR text; only an exact one-to-one id match moves anything
    let gone_ids: HashMap<String, Vec<String>> = if p.gone.is_empty() {
        Default::default()
    } else {
        let mut st = db
            .conn
            .prepare("SELECT id,path FROM files")
            .map_err(|e| e.to_string())?;
        let mut m: HashMap<String, Vec<String>> = Default::default();
        for (id, path) in st
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
        {
            if p.gone.contains(&path) {
                m.entry(id).or_default().push(path);
            }
        }
        m
    };
    // scans for new + changed files, written as they arrive (each row its own transaction, as the batch editor does)
    let (mut written, mut failed, mut touched) = (0usize, 0usize, Vec::<String>::new());
    let mut new_ids: BTreeMap<String, Vec<String>> = Default::default();
    for e in p.new.iter().chain(p.changed.iter().map(|(_, now)| now)) {
        match src.call(Request::Scan {
            root: src.root.clone(),
            path: c
                .active_source
                .as_ref()
                .and_then(|id| e.path.strip_prefix(&format!("{id}/")))
                .unwrap_or(&e.path)
                .into(),
            vocab: effective_vocab.clone(),
        }) {
            Ok(Reply::Scan(mut scan)) => {
                c.key_for_scan(&mut scan);
                if scan.row.path != e.path {
                    failed += 1;
                    eprintln!("skip {}: worker returned a different file", e.path);
                    continue;
                }
                new_ids
                    .entry(scan.row.id.clone())
                    .or_default()
                    .push(scan.row.path.clone());
                c.ensure_current()?;
                db.import_scan(scan)?;
                written += 1;
                touched.push(e.path.clone());
            }
            Ok(_) => {
                failed += 1;
                eprintln!("skip {}: unexpected reply", e.path);
            }
            Err(err) => {
                failed += 1;
                eprintln!("skip {}: {err}", e.path);
            }
        }
        if written % 200 == 0 && written > 0 {
            eprintln!("  {written}/{}", p.new.len() + p.changed.len());
        }
    }
    let mut moved = 0usize;
    if failed > 0 && !p.gone.is_empty() {
        // a scan that failed may be a renamed file's new name: dropping the old row now would lose its text
        eprintln!(
            "pull: {failed} scans failed, so the {} gone rows are kept for next time",
            p.gone.len()
        );
        p.gone.clear();
    }
    if !p.gone.is_empty() {
        c.ensure_current()?;
        let tx = db.conn.unchecked_transaction().map_err(|e| e.to_string())?;
        for (id, olds) in &gone_ids {
            let (Some(news), true) = (new_ids.get(id), olds.len() == 1) else {
                continue;
            };
            if news.len() != 1 {
                continue;
            }
            let (old, new) = (&olds[0], &news[0]);
            for t in ["text", "text_meta", "manual_text", "phash2"] {
                let has: i64 = tx
                    .query_row(
                        &format!("SELECT count(*) FROM {t} WHERE path=?1"),
                        [new],
                        |r| r.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                if has == 0 {
                    tx.execute(&format!("UPDATE {t} SET path=?1 WHERE path=?2"), [new, old])
                        .map_err(|e| e.to_string())?;
                }
            }
            moved += 1;
        }
        for g in &p.gone {
            for t in [
                "tags",
                "files",
                "text",
                "phash2",
                "manual_text",
                "text_meta",
                "proposal_rejects",
            ] {
                tx.execute(&format!("DELETE FROM {t} WHERE path=?1"), [g])
                    .map_err(|e| e.to_string())?;
            }
            crate::grab::forget(c, g);
        }
        tx.commit().map_err(|e| e.to_string())?;
    }
    drop(src);
    println!("pull: {written} rows written ({failed} failed), {} removed, {moved} renames kept their text ({:.1}s)", p.gone.len(), t0.elapsed().as_secs_f64());
    if thumbs && !touched.is_empty() {
        let all = db.all()?;
        let rows: Vec<&FileRow> = all.iter().filter(|r| touched.contains(&r.path)).collect();
        crate::grab::build_thumbs(c, Some(&rows))?;
        if crate::similar::in_use(&db) {
            let touched: Vec<FileRow> = rows.into_iter().cloned().collect();
            crate::similar::ensure(c, &db, &touched, true)?;
        }
    }
    let engine = crate::ocr::current_label(c);
    let unread_all = unread(&db, &engine, false)?;
    if unread_all > 0 {
        println!("pull: {unread_all} images have no {engine} text yet — start the pass when ready: systemctl --user start memetag-ocr.service");
    }
    // the timer's case: kick the resumable pass only when unread images arrived since the pass last wrote (a grab-time pull
    // may have indexed them earlier in the day), so an idle night — or one whose only unread images already failed — leaves the GPU alone
    if start_ocr && unread(&db, &engine, true)? > 0 {
        let ok = Command::new("systemctl")
            .args(["--user", "start", "--no-block", "memetag-ocr.service"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        println!(
            "pull: memetag-ocr.service {}",
            if ok {
                "started"
            } else {
                "could not be started (see journalctl --user -u memetag-ocr.service)"
            }
        );
    }
    Ok(Outcome {
        written: touched,
        gone: p.gone,
        failed,
        vocab_conflict,
        vocab_changed: false, // for_grab compares the final rules with the window's starting snapshot
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn worker_down_names_the_cause() {
        use std::os::unix::process::ExitStatusExt;
        // a wait status encodes the exit code in the high byte
        let st = |code: i32| Some(std::process::ExitStatus::from_raw(code << 8));
        assert!(worker_down("box", "pull", st(255)).contains("Could not reach"));
        assert!(worker_down("box", "pull", st(127)).contains("not installed"));
        assert!(worker_down("box", "pull", st(1)).contains("disconnected"));
        assert!(worker_down("box", "pull", None).contains("disconnected"));
        assert!(
            worker_down("box", "pull", st(255)).contains("box"),
            "names the host"
        );
    }
    fn e(p: &str, s: i64, m: f64) -> Entry {
        Entry {
            path: p.into(),
            size: s,
            mtime: m,
        }
    }
    #[test]
    fn plan_is_an_exact_key_on_size_and_mtime() {
        let local = vec![
            e("a.png", 10, 1.5),
            e("b.png", 20, 2.5),
            e("c.png", 30, 3.5),
            e("d.png", 40, 4.5),
        ];
        let server = vec![
            e("a.png", 10, 1.5),
            e("b.png", 21, 2.5),
            e("c.png", 30, 3.500001),
            e("e.png", 50, 5.5),
        ];
        let p = plan(&local, &server);
        assert_eq!(p.new, vec![e("e.png", 50, 5.5)]);
        assert_eq!(
            p.changed,
            vec![
                (e("b.png", 20, 2.5), e("b.png", 21, 2.5)),
                (e("c.png", 30, 3.5), e("c.png", 30, 3.500001))
            ]
        );
        assert_eq!(p.gone, vec!["d.png".to_string()]);
        assert_eq!(plan(&local, &local), Plan::default());
    }
    #[test]
    fn pull_merges_new_changed_gone_and_carries_text_across_a_rename() {
        let base = std::env::temp_dir().join(format!("memetag-pull-{}", std::process::id()));
        let root = base.join("collection"); // the index and thumbs live beside the collection, never inside it
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let webp = include_bytes!("../tests/fixtures/animated.webp");
        let png = include_bytes!("../tests/fixtures/animated.png");
        std::fs::write(root.join("keep.webp"), webp).unwrap();
        std::fs::write(root.join("old-name.png"), png).unwrap();
        std::fs::write(root.join("sub/doomed.webp"), webp).unwrap();
        let mut c = crate::cfg();
        c.root = root.clone();
        c.sources = None;
        c.active_source = None;
        c.db = base.join("index.sqlite");
        c.thumbs = base.join("thumbs");
        c.vocab = Vocab::default();
        let db = Db::open_cfg(&c).unwrap();
        for f in ["keep.webp", "old-name.png", "sub/doomed.webp"] {
            db.upsert_file(&root, &root.join(f), &c.vocab).unwrap();
        }
        db.conn
            .execute("INSERT INTO text VALUES('old-name.png','THE CAPTION')", [])
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO text_meta VALUES('old-name.png','ollama:test',1.0,0.5)",
                [],
            )
            .unwrap();
        db.conn
            .execute("INSERT INTO text VALUES('sub/doomed.webp','GONE SOON')", [])
            .unwrap();
        drop(db);
        // nothing changed yet: a pull is a no-op
        run(&c, &["--force".into(), "--no-thumbs".into()]).unwrap();
        // rename (same bytes → same provisional id), add a file, delete one, touch one
        std::fs::rename(root.join("old-name.png"), root.join("sub/new-name.png")).unwrap();
        std::fs::write(root.join("fresh.webp"), webp).unwrap();
        std::fs::remove_file(root.join("sub/doomed.webp")).unwrap();
        let later = UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        std::fs::File::options()
            .write(true)
            .open(root.join("keep.webp"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(later))
            .unwrap();
        let db = Db::open_cfg(&c).unwrap();
        let local = local_entries(&db).unwrap();
        let p = plan(&local, &list(&root).unwrap());
        let paths = |v: &Vec<Entry>| v.iter().map(|e| e.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths(&p.new), vec!["fresh.webp", "sub/new-name.png"]);
        assert_eq!(
            p.changed
                .iter()
                .map(|(_, e)| e.path.clone())
                .collect::<Vec<_>>(),
            vec!["keep.webp"]
        );
        assert_eq!(p.gone, vec!["old-name.png", "sub/doomed.webp"]);
        drop(db);
        run(&c, &["--force".into(), "--no-thumbs".into()]).unwrap();
        let db = Db::open_cfg(&c).unwrap();
        let rows = db.all().unwrap();
        let mut have: Vec<&str> = rows.iter().map(|r| r.path.as_str()).collect();
        have.sort();
        assert_eq!(have, vec!["fresh.webp", "keep.webp", "sub/new-name.png"]);
        let renamed = rows.iter().find(|r| r.path == "sub/new-name.png").unwrap();
        assert_eq!(renamed.text, "THE CAPTION");
        assert!(renamed.tags.contains("folder:sub"));
        assert_eq!(
            db.conn
                .query_row(
                    "SELECT engine FROM text_meta WHERE path='sub/new-name.png'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "ollama:test"
        );
        assert_eq!(
            db.conn
                .query_row("SELECT count(*) FROM text", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
        let kept = rows.iter().find(|r| r.path == "keep.webp").unwrap();
        assert_eq!(kept.mtime, 1_600_000_000.0);
        // and once more: nothing left to do
        let local = local_entries(&db).unwrap();
        assert_eq!(plan(&local, &list(&root).unwrap()), Plan::default());
        drop(db);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn ocr_kick_wants_unread_images_newer_than_the_engines_last_write() {
        let dir = std::env::temp_dir().join(format!("memetag-unread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let db = Db::open(&dir.join("i.sqlite")).unwrap();
        let file = |path: &str, indexed_at: f64| {
            db.conn
                .execute(
                    "INSERT INTO files(path,id,kind,indexed_at) VALUES(?1,?1,'image',?2)",
                    rusqlite::params![path, indexed_at],
                )
                .unwrap()
        };
        file("old-failed.jpg", 100.0);
        file("done.jpg", 100.0);
        file("typed.jpg", 100.0);
        file("fresh.jpg", 300.0);
        db.conn
            .execute(
                "INSERT INTO text_meta(path,engine,at) VALUES('done.jpg','e',200.0)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO manual_text(path,body) VALUES('typed.jpg','x')",
                [],
            )
            .unwrap();
        assert_eq!(
            unread(&db, "e", false).unwrap(),
            2,
            "old-failed and fresh have no text from engine e"
        );
        assert_eq!(
            unread(&db, "e", true).unwrap(),
            1,
            "only fresh arrived after e's last write at 200"
        );
        assert_eq!(
            unread(&db, "other", true).unwrap(),
            3,
            "an engine that never wrote sees every unread image as new"
        );
    }
}
