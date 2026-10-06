//! Reviewed tag-only batches. Shared media are edited on their server; only
//! metadata scans cross SSH back to the desktop index. One request is in flight
//! at a time, so Stop/EOF leaves no queued file writes behind.
#[cfg(feature = "gui")]
use crate::widgets::{self, TagInput};
#[cfg(any(feature = "gui", test))]
use crate::{
    autocomplete,
    index::{Db, FileRow},
};
use crate::{containers, index::Scan, tagger, vocab::Vocab, xmp, Cfg};
#[cfg(feature = "gui")]
use eframe::egui;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    io::{BufRead, Write},
    path::{Component, PathBuf},
    process::{Command, Stdio},
};
#[cfg(any(feature = "gui", test))]
use std::{
    collections::{BTreeMap, HashSet},
    io::BufReader,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
};

#[derive(Serialize, Deserialize)]
struct Request {
    root: PathBuf,
    path: String,
    add: BTreeSet<String>,
    remove: BTreeSet<String>,
    vocab: Vocab,
}
#[derive(Serialize, Deserialize)]
struct Reply {
    scan: Scan,
    changed: bool,
}

/// Single-file edits carry only XMP and byte revisions, never the media over stdin. Old helpers
/// reject this request (missing add/remove) rather than silently doing a tag-only operation.
#[derive(Serialize, Deserialize)]
struct SingleWrite {
    write_version: u32,
    root: PathBuf,
    path: String,
    expected: String,
    replacement: String,
    packet: String,
}
#[derive(Serialize, Deserialize)]
struct SingleReply {
    report: crate::writer::Report,
    revision: String,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum WorkerRequest {
    Single(SingleWrite),
    Batch(Request),
}

fn checked_path(root: &std::path::Path, rel: &str) -> Result<PathBuf, String> {
    if !root.is_absolute()
        || rel.is_empty()
        || !std::path::Path::new(rel)
            .components()
            .all(|p| matches!(p, Component::Normal(_)))
    {
        return Err("Invalid batch file path".into());
    }
    let path = root.join(rel).canonicalize().map_err(|e| e.to_string())?;
    if !path.starts_with(root.canonicalize().map_err(|e| e.to_string())?) {
        return Err("File resolves outside the collection".into());
    }
    Ok(path)
}

fn single_write(request: &SingleWrite) -> Result<SingleReply, String> {
    if request.write_version != 1 {
        return Err("Unsupported single-write protocol".into());
    }
    let path = checked_path(&request.root, &request.path)?;
    let before = std::fs::read(&path).map_err(|e| e.to_string())?;
    if crate::writer::revision(&before) != request.expected {
        return Err("File changed on the server; reload and retry. Nothing was written.".into());
    }
    let after = containers::set_xmp(&before, &request.packet)?;
    if crate::writer::revision(&after) != request.replacement {
        return Err(
            "Server produced different metadata bytes; update the helper before retrying.".into(),
        );
    }
    // write_in_place rechecks these bytes while holding the same inode lock used by batch writes.
    let written = crate::writer::write_in_place(&path, &before, &after)?;
    Ok(SingleReply {
        revision: crate::writer::revision(&written.bytes),
        report: written.report,
    })
}

fn apply(request: &Request) -> Result<Reply, String> {
    if !request.root.is_absolute()
        || request.path.is_empty()
        || !std::path::Path::new(&request.path)
            .components()
            .all(|p| matches!(p, Component::Normal(_)))
    {
        return Err("Invalid batch file path".into());
    }
    if !request.add.is_disjoint(&request.remove) {
        return Err("A tag cannot be added and removed together".into());
    }
    let path = request.root.join(&request.path);
    if !path
        .canonicalize()
        .map_err(|e| e.to_string())?
        .starts_with(request.root.canonicalize().map_err(|e| e.to_string())?)
    {
        return Err("File resolves outside the collection".into());
    }
    let mut bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let packet = containers::get_xmp(&bytes)?;
    let tags: BTreeSet<_> = packet
        .as_deref()
        .map(xmp::read)
        .transpose()?
        .unwrap_or_default()
        .tags
        .iter()
        .map(|t| request.vocab.canon(t))
        .collect();
    let add: BTreeSet<_> = request
        .add
        .iter()
        .map(|t| request.vocab.canon(t))
        .filter(|t| !t.is_empty())
        .collect();
    let remove: BTreeSet<_> = request
        .remove
        .iter()
        .map(|t| request.vocab.canon(t))
        .collect();
    if !add.is_disjoint(&remove) {
        return Err("Conflicting tag aliases".into());
    }
    let ops: Vec<_> = tags
        .intersection(&remove)
        .map(|t| format!("-{t}"))
        .chain(add.difference(&tags).map(|t| format!("+{t}")))
        .collect();
    let changed = !ops.is_empty();
    if changed {
        let mut c = crate::cfg();
        c.root = request.root.clone();
        c.sources = None;
        c.active_source = None;
        c.vocab = request.vocab.clone();
        // Empty extra fields are intentional: preserve all OCR and review markers.
        bytes = tagger::apply_bytes_local(&c, &path, bytes, &ops, &[])?.bytes;
    }
    let md = std::fs::metadata(&path)
        .map_err(|e| format!("Metadata write completed, but rescan failed: {e}"))?;
    let scan = crate::index::scan_bytes(&request.root, &path, &bytes, &md, &request.vocab);
    Ok(Reply { scan, changed })
}

pub fn worker() -> Result<(), String> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line.map_err(|e| e.to_string())?;
        match serde_json::from_str::<WorkerRequest>(&line) {
            Ok(WorkerRequest::Single(r)) => serde_json::to_writer(&mut stdout, &single_write(&r)),
            Ok(WorkerRequest::Batch(r)) => serde_json::to_writer(&mut stdout, &apply(&r)),
            Err(e) => {
                serde_json::to_writer(&mut stdout, &Result::<Reply, String>::Err(e.to_string()))
            }
        }
        .map_err(|e| e.to_string())?;
        writeln!(stdout).map_err(|e| e.to_string())?;
        stdout.flush().map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[derive(Deserialize)]
struct Remote {
    local_root: PathBuf,
    root: PathBuf,
    host: String,
    command: String,
}
fn remote(c: &Cfg) -> Result<Option<Remote>, String> {
    if let Some(library) = c.library()? {
        let source = library.source(c.active_source.as_deref().unwrap_or("main"))?;
        if let Some(remote) = &source.remote {
            return Ok(Some(Remote {
                local_root: source.path.clone(),
                root: remote.root.clone(),
                host: remote.host.clone(),
                command: remote.batch_command.clone(),
            }));
        }
    }
    let path = crate::paths::config_dir().join("config.toml");
    if let Ok(text) = std::fs::read_to_string(path) {
        let table: toml::Table = text
            .parse()
            .map_err(|e| format!("Batch configuration: {e}"))?;
        if let Some(value) = table.get("batch_remote") {
            let r: Remote = value
                .clone()
                .try_into()
                .map_err(|e| format!("Batch configuration: {e}"))?;
            if c.root == r.local_root {
                if r.host.is_empty()
                    || r.host.starts_with('-')
                    || r.host.chars().any(char::is_whitespace)
                {
                    return Err("Invalid batch server host".into());
                }
                return Ok(Some(r));
            }
        }
    }
    if requires_remote(&c.root)? {
        return Err(
            "Editing a network collection needs a matching batch_remote configuration".into(),
        );
    }
    Ok(None)
}

pub(crate) fn requires_remote(path: &std::path::Path) -> Result<bool, String> {
    let mounts = std::fs::read_to_string("/proc/self/mountinfo")
        .map_err(|e| format!("Cannot determine collection filesystem: {e}"))?;
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| e.to_string())?
            .join(path)
    };
    Ok(network_root(&absolute, &mounts))
}

/// Inspect mount metadata without touching a possibly stalled network filesystem. The same absolute
/// path can refer to a network mount on a client and a native filesystem on a server; spelling cannot decide routing.
fn network_root(root: &std::path::Path, mounts: &str) -> bool {
    use std::os::unix::ffi::OsStringExt;
    mounts
        .lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let encoded = left.split_whitespace().nth(4)?;
            let mut decoded = Vec::new();
            let mut bytes = encoded.as_bytes().iter().copied().peekable();
            while let Some(b) = bytes.next() {
                if b == b'\\' {
                    let digits: Vec<_> = bytes.by_ref().take(3).collect();
                    if digits.len() != 3 || !digits.iter().all(|d| (b'0'..=b'7').contains(d)) {
                        return None;
                    }
                    let value = u16::from(digits[0] - b'0') * 64
                        + u16::from(digits[1] - b'0') * 8
                        + u16::from(digits[2] - b'0');
                    decoded.push(u8::try_from(value).ok()?);
                } else {
                    decoded.push(b);
                }
            }
            let mount = PathBuf::from(std::ffi::OsString::from_vec(decoded));
            root.starts_with(&mount).then(|| {
                (
                    mount.components().count(),
                    right.split_whitespace().next().unwrap_or(""),
                )
            })
        })
        .max_by_key(|(depth, _)| *depth)
        .is_some_and(|(_, kind)| matches!(kind, "fuse.sshfs" | "nfs" | "nfs4" | "cifs" | "smb3"))
}

/// Editor reads must use the same authoritative server as writes. SSHFS may retain
/// old bytes for minutes, including after our own successful save.
pub fn read_for_edit(c: &Cfg, path: &std::path::Path) -> Result<Vec<u8>, String> {
    let c = c.for_file(path)?;
    match remote(&c)? {
        Some(remote) => read_server_file(&remote, path, std::path::Path::new("ssh")),
        None => std::fs::read(path).map_err(|e| format!("{}: {e}", path.display())),
    }
}

fn read_server_file(
    remote: &Remote,
    path: &std::path::Path,
    ssh: &std::path::Path,
) -> Result<Vec<u8>, String> {
    // Do not canonicalize through the cached mount. Only normal relative components
    // may be mapped onto the configured server root.
    let rel = path
        .strip_prefix(&remote.local_root)
        .map_err(|_| "File is outside the configured remote collection")?;
    if rel.as_os_str().is_empty()
        || !rel.components().all(|p| matches!(p, Component::Normal(_)))
        || !remote.root.is_absolute()
    {
        return Err("Invalid remote editor file path".into());
    }
    let server_path = remote.root.join(rel);
    let server_path = server_path
        .to_str()
        .ok_or("Remote file path is not UTF-8")?;
    let command = format!("cat -- '{}'", server_path.replace('\'', "'\\''"));
    let output = Command::new(ssh)
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=3",
            &remote.host,
            &command,
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Read file from {}: {e}", remote.host))?;
    if !output.status.success() {
        return Err(format!(
            "Could not read fresh file from {}: {}",
            remote.host,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

pub fn write_remote(
    c: &Cfg,
    path: &std::path::Path,
    before: &[u8],
    after: &[u8],
    packet: &str,
) -> Result<Option<crate::writer::Written>, String> {
    let c = c.for_file(path)?;
    let Some(remote) = remote(&c)? else {
        return Ok(None);
    };
    let rel = path
        .strip_prefix(&remote.local_root)
        .map_err(|_| "File is outside the configured remote collection")?;
    crate::sources::relative(rel)?;
    let rel = rel.to_str().ok_or("Remote file path is not UTF-8")?;
    let request = SingleWrite {
        write_version: 1,
        root: remote.root,
        path: rel.into(),
        expected: crate::writer::revision(before),
        replacement: crate::writer::revision(after),
        packet: packet.into(),
    };
    let mut child = Command::new("ssh")
        .args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "-o",
            "ServerAliveInterval=10",
            "-o",
            "ServerAliveCountMax=3",
            &remote.host,
            &remote.command,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Start write server: {e}"))?;
    let sent = (|| -> Result<(), String> {
        let mut stdin = child.stdin.take().ok_or("Missing helper stdin")?;
        serde_json::to_writer(&mut stdin, &request).map_err(|e| e.to_string())?;
        writeln!(stdin).map_err(|e| e.to_string())
    })();
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "Write helper on {} failed: {}. Reload before retrying; the write may have completed.",
            remote.host,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    sent?;
    let reply: Result<SingleReply, String> =
        serde_json::from_slice(&output.stdout).map_err(|e| {
            format!(
                "Invalid write-helper reply ({e}); update the helper, reload the file, and retry."
            )
        })?;
    let reply = reply?;
    if reply.revision != request.replacement || !reply.report.ok() {
        return Err("Remote write verification failed; reload before retrying.".into());
    }
    Ok(Some(crate::writer::Written {
        report: reply.report,
        bytes: after.to_vec(),
    }))
}

#[cfg(feature = "gui")]
enum Event {
    File {
        path: String,
        result: Result<bool, String>,
    },
    Done(Result<Vec<FileRow>, String>),
}
#[cfg(feature = "gui")]
fn run(
    c: Cfg,
    paths: Vec<String>,
    add: BTreeSet<String>,
    remove: BTreeSet<String>,
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<Event>,
    ctx: egui::Context,
) {
    let result = (|| -> Result<Vec<FileRow>, String> {
        if let Some(library) = c.library()? {
            let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for path in paths {
                let (source, _) = library.identify(&path)?;
                groups.entry(source.id.clone()).or_default().push(path);
            }
            for (id, paths) in groups {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                run_source(
                    c.for_source(&id)?,
                    paths,
                    add.clone(),
                    remove.clone(),
                    stop.clone(),
                    tx.clone(),
                    ctx.clone(),
                )?;
            }
            Db::open_cfg(&c)?.all()
        } else {
            run_source(c.clone(), paths, add, remove, stop, tx.clone(), ctx.clone())
        }
    })();
    let _ = tx.send(Event::Done(result));
    ctx.request_repaint();
}

#[cfg(feature = "gui")]
fn run_source(
    c: Cfg,
    paths: Vec<String>,
    add: BTreeSet<String>,
    remove: BTreeSet<String>,
    stop: Arc<AtomicBool>,
    tx: mpsc::Sender<Event>,
    ctx: egui::Context,
) -> Result<Vec<FileRow>, String> {
    let finish = || -> Result<Vec<FileRow>, String> {
        let db = Db::open_cfg(&c)?;
        let target = remote(&c)?;
        let mut child = if let Some(r) = &target {
            // Configured command only. File paths and tags travel as JSON on stdin.
            Some(
                Command::new("ssh")
                    .args([
                        "-o",
                        "BatchMode=yes",
                        "-o",
                        "ConnectTimeout=10",
                        "-o",
                        "ServerAliveInterval=10",
                        "-o",
                        "ServerAliveCountMax=3",
                        &r.host,
                        &r.command,
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .spawn()
                    .map_err(|e| format!("Start batch server: {e}"))?,
            )
        } else {
            None
        };
        let mut input = child.as_mut().and_then(|p| p.stdin.take());
        let mut output = child
            .as_mut()
            .and_then(|p| p.stdout.take())
            .map(BufReader::new);
        for path in paths {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let request = Request {
                root: target
                    .as_ref()
                    .map(|r| r.root.clone())
                    .unwrap_or_else(|| c.root.clone()),
                path: c
                    .active_source
                    .as_ref()
                    .and_then(|id| path.strip_prefix(&format!("{id}/")))
                    .unwrap_or(&path)
                    .into(),
                add: add.clone(),
                remove: remove.clone(),
                vocab: c.vocab.clone(),
            };
            let mut transport_failed = false;
            let result = (|| -> Result<bool, String> {
                c.for_file(&c.file_path(&path)?)?;
                let mut reply = if let (Some(input), Some(output)) = (&mut input, &mut output) {
                    let exchange = (|| -> Result<Result<Reply, String>, String> {
                        serde_json::to_writer(&mut *input, &request).map_err(|e| e.to_string())?;
                        writeln!(input).map_err(|e| e.to_string())?;
                        input.flush().map_err(|e| e.to_string())?;
                        let mut line = String::new();
                        if output.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
                            return Err("Batch server disconnected; check connection and helper installation".into());
                        }
                        serde_json::from_str(&line)
                            .map_err(|e| format!("Batch server response: {e}"))
                    })();
                    match exchange {
                        Ok(value) => value?,
                        Err(e) => {
                            transport_failed = true;
                            return Err(format!(
                                "{e}. This file may have been written; retrying is safe."
                            ));
                        }
                    }
                } else {
                    apply(&request)?
                };
                if reply.scan.row.path != request.path {
                    transport_failed = true;
                    return Err("Batch server returned a different file".into());
                }
                c.ensure_current()?;
                c.key_for_scan(&mut reply.scan);
                db.import_tag_scan(reply.scan).map_err(|e| {
                    format!("File processed, but index refresh failed: {e}. Retry to refresh.")
                })?;
                for tag in &add {
                    db.conn.execute("INSERT INTO tag_history(tag,uses) VALUES(?1,1) ON CONFLICT(tag) DO UPDATE SET uses=uses+1", [tag]).map_err(|e| format!("File processed, but tag history update failed: {e}"))?;
                }
                Ok(reply.changed)
            })();
            if tx.send(Event::File { path, result }).is_err() {
                break;
            }
            ctx.request_repaint();
            if transport_failed {
                break;
            }
        }
        drop(input); // EOF stops the remote worker without interrupting a file write.
        if let Some(mut child) = child {
            let _ = child.wait();
        }
        db.all()
    };
    finish()
}

#[cfg(feature = "gui")]
struct Loaded {
    counts: BTreeMap<String, usize>,
    suggestions: Vec<(String, u64, bool)>,
}
#[cfg(feature = "gui")]
pub enum Action {
    None,
    Close {
        rows: Option<Vec<FileRow>>,
        succeeded: HashSet<String>,
        /// the plan as it was applied (the panel lets the user change it after the Tags card filled it in)
        add: BTreeSet<String>,
        remove: BTreeSet<String>,
    },
}
#[cfg(feature = "gui")]
pub struct BatchUi {
    paths: Vec<String>,
    loaded: Option<Loaded>,
    load_rx: mpsc::Receiver<Result<Loaded, String>>,
    add: BTreeSet<String>,
    remove: BTreeSet<String>,
    add_input: TagInput,
    remove_input: TagInput,
    rx: Option<mpsc::Receiver<Event>>,
    stop: Arc<AtomicBool>,
    completed: usize,
    changed: usize,
    succeeded: HashSet<String>,
    errors: Vec<String>,
    rows: Option<Vec<FileRow>>,
    finished: bool,
    discard: bool,
}
#[cfg(feature = "gui")]
impl BatchUi {
    pub fn new(c: Cfg, paths: Vec<String>, ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let selected: HashSet<_> = paths.iter().cloned().collect();
        std::thread::spawn(move || {
            let result = (|| -> Result<Loaded, String> {
                let db = Db::open_cfg(&c)?;
                let mut counts = BTreeMap::new();
                let mut st = db
                    .conn
                    .prepare("SELECT path,tag FROM tags WHERE source='xmp'")
                    .map_err(|e| e.to_string())?;
                for row in st
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                    .map_err(|e| e.to_string())?
                {
                    let (path, tag) = row.map_err(|e| e.to_string())?;
                    if selected.contains(&path) {
                        *counts.entry(tag).or_insert(0) += 1;
                    }
                }
                Ok(Loaded {
                    counts,
                    suggestions: autocomplete::load(&c),
                })
            })();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        Self {
            paths,
            loaded: None,
            load_rx: rx,
            add: BTreeSet::new(),
            remove: BTreeSet::new(),
            add_input: TagInput::default(),
            remove_input: TagInput::default(),
            rx: None,
            stop: Arc::new(AtomicBool::new(false)),
            completed: 0,
            changed: 0,
            succeeded: HashSet::new(),
            errors: vec![],
            rows: None,
            finished: false,
            discard: false,
        }
    }
    /// A plan filled in by the caller (the Tags card), still reviewed here before it runs.
    pub fn plan(mut self, add: BTreeSet<String>, remove: BTreeSet<String>) -> Self {
        self.add = add;
        self.remove = remove;
        self
    }
    pub fn busy(&self) -> bool {
        self.rx.is_some() && !self.finished
    }
    pub fn show(&mut self, ui: &mut egui::Ui, c: &Cfg) -> Action {
        let ctx = ui.ctx().clone();
        if self.loaded.is_none() && self.errors.is_empty() {
            match self.load_rx.try_recv() {
                Ok(Ok(loaded)) => self.loaded = Some(loaded),
                Ok(Err(e)) => self.errors.push(e),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.errors.push("Batch loading stopped".into())
                }
                _ => ctx.request_repaint_after(std::time::Duration::from_millis(50)),
            }
        }
        if let Some(rx) = &self.rx {
            while !self.finished {
                match rx.try_recv() {
                    Ok(Event::File { path, result }) => {
                        self.completed += 1;
                        match result {
                            Ok(changed) => {
                                self.succeeded.insert(path);
                                self.changed += usize::from(changed);
                            }
                            Err(e) => self.errors.push(format!("{path}: {e}")),
                        }
                    }
                    Ok(Event::Done(result)) => {
                        match result {
                            Ok(rows) => self.rows = Some(rows),
                            Err(e) => self.errors.push(e),
                        };
                        self.finished = true;
                    }
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.errors
                            .push("Batch stopped unexpectedly; reopen to check saved files".into());
                        self.finished = true;
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }
        }
        let busy = self.busy();
        // A clean finish closes the card by itself: it offers no further editing, so Done would only be a click
        // on success. Errors keep it open to be read; the failed files stay selected for a retry.
        let mut close = self.finished && self.errors.is_empty();
        let mut start = false;
        if busy && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.stop.store(true, Ordering::Relaxed);
        }
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading(format!("Edit tags on {} images", self.paths.len()));
                if ui.add_enabled(!busy, egui::Button::new(if self.finished { "Done" } else { "Cancel" })).clicked() { close = true; }
            });
            if busy || self.finished {
                ui.label(format!("{} / {} processed · {} changed · {} already matched", self.completed, self.paths.len(), self.changed, self.succeeded.len() - self.changed));
                ui.add(egui::ProgressBar::new(self.completed as f32 / self.paths.len().max(1) as f32));
                if busy {
                    if ui.add_enabled(!self.stop.load(Ordering::Relaxed), egui::Button::new("Stop after current file")).clicked() { self.stop.store(true, Ordering::Relaxed); }
                    if self.stop.load(Ordering::Relaxed) { ui.label("Stopping after the current file…"); }
                    ctx.request_repaint_after(std::time::Duration::from_millis(50));
                } else { ui.label("Completed files will be deselected. Failed and unprocessed files stay selected for retry."); }
            }
            egui::ScrollArea::vertical().id_salt("batch").show(ui, |ui| {
                for e in &self.errors { ui.colored_label(widgets::ERROR, e); }
                if self.discard {
                    ui.horizontal(|ui| { ui.label("Discard this tag plan?"); if ui.button("Discard").clicked() { self.add.clear(); self.remove.clear(); self.add_input.text.clear(); self.remove_input.text.clear(); close = true; } if ui.button("Keep editing").clicked() { self.discard = false; } });
                }
                let Some(loaded) = &self.loaded else { if self.errors.is_empty() { ui.spinner(); } return; };
                ui.add_enabled_ui(!busy && !self.finished, |ui| {
                    ui.strong("Add to every selected image"); widgets::chips(ui, "", &mut self.add, |_| String::new());
                    if let Some(tag) = self.add_input.show(ui, &loaded.suggestions, "batch-add", "Add to list", &self.add) { let tag = c.vocab.canon(&tag); if !tag.is_empty() { self.remove.remove(&tag); self.add.insert(tag); } }
                    ui.separator(); ui.strong("Remove from every selected image"); widgets::chips(ui, "", &mut self.remove, |_| String::new());
                    // tags every selected image already carries: one click puts one in the Remove list (wishlist, 2026-09-22: mis-tags)
                    let shared: Vec<String> = loaded.counts.iter().filter(|(t, n)| **n == self.paths.len() && !self.remove.contains(*t)).map(|(t, _)| (*t).clone()).collect();
                    if !shared.is_empty() {
                        ui.horizontal_wrapped(|ui| { ui.small("on every selected image:"); for t in shared { if ui.small_button(&t).on_hover_text("Put it in the Remove list").clicked() { self.add.remove(&t); self.remove.insert(t); } } });
                    }
                    if let Some(tag) = self.remove_input.show(ui, &loaded.suggestions, "batch-remove", "Add to list", &self.remove) { let tag = c.vocab.canon(&tag); if !tag.is_empty() { self.add.remove(&tag); self.remove.insert(tag); } }
                    ui.collapsing("Existing embedded tags (indexed counts; click to remove)", |ui| {
                        for (tag, count) in &loaded.counts { if ui.button(format!("{tag} — {count} / {}", self.paths.len())).clicked() { self.add.remove(tag); self.remove.insert(tag.clone()); } }
                    });
                    ui.collapsing("Selected files", |ui| { for path in &self.paths { ui.label(path); } });
                    ui.separator();
                    ui.label(format!("Add: {}", self.add.iter().cloned().collect::<Vec<_>>().join(" · ")));
                    ui.label(format!("Remove: {}", self.remove.iter().cloned().collect::<Vec<_>>().join(" · ")));
                    ui.small("Other tags and OCR stay intact. Folder/implied tags are derived. Modification dates are preserved. Ctrl+S applies; Escape cancels.");
                    let pending = !self.add_input.text.trim().is_empty() || !self.remove_input.text.trim().is_empty();
                    if pending { ui.label("Press Enter or choose a suggestion to put the typed tag in a list."); }
                    let ready = !pending && (!self.add.is_empty() || !self.remove.is_empty());
                    if ui.add_enabled(ready, egui::Button::new(format!("Apply to {} images", self.paths.len()))).clicked() { start = true; }
                    // Ctrl+S and Ctrl+Enter match the editor and are gated exactly like the button,
                    // so a half-typed tag never lands on many files.
                    // and, unlike the button, not disabled by the enclosing add_enabled_ui: a key repeat while a run is
                    // busy (or a second Ctrl+S after it finished) used to start the whole batch again (review, 2026-09-22)
                    start |= ready && !busy && !self.finished && (widgets::chord(&ctx, egui::Key::S) || widgets::chord(&ctx, egui::Key::Enter));
                });
            });
        });
        close |= !busy && crate::widgets::escape(&ctx);
        if close {
            if !self.finished
                && (!self.add.is_empty()
                    || !self.remove.is_empty()
                    || !self.add_input.text.is_empty()
                    || !self.remove_input.text.is_empty())
            {
                self.discard = true;
            } else {
                return Action::Close {
                    rows: self.rows.take(),
                    succeeded: std::mem::take(&mut self.succeeded),
                    add: self.add.clone(),
                    remove: self.remove.clone(),
                };
            }
        }
        if start {
            let (tx, rx) = mpsc::channel();
            self.rx = Some(rx);
            let (c, paths, add, remove, stop) = (
                c.clone(),
                self.paths.clone(),
                self.add.clone(),
                self.remove.clone(),
                self.stop.clone(),
            );
            std::thread::spawn(move || run(c, paths, add, remove, stop, tx, ctx));
        }
        Action::None
    }
}
#[cfg(feature = "gui")]
impl Drop for BatchUi {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editor_reads_server_bytes_despite_stale_mount_and_fails_closed() {
        use std::os::unix::fs::PermissionsExt;
        let temp =
            std::env::temp_dir().join(format!("memetag-fresh-editor-{}", std::process::id()));
        let local = temp.join("cached");
        let server = temp.join("server");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&server).unwrap();
        // Execute the exact remote shell command, with no network or global env changes.
        let ssh = temp.join("ssh");
        std::fs::write(
            &ssh,
            "#!/bin/sh\nfor arg do command=$arg; done\nexec sh -c \"$command\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o700)).unwrap();
        let remote = Remote {
            local_root: local.clone(),
            root: server.clone(),
            host: "fixture".into(),
            command: String::new(),
        };
        let name = "image's café $(false) `false`\n.jpg";
        let path = local.join(name);
        std::fs::write(&path, b"stale cached metadata").unwrap();
        std::fs::write(server.join(name), b"fresh saved metadata").unwrap();
        assert_eq!(
            read_server_file(&remote, &path, &ssh).unwrap(),
            b"fresh saved metadata"
        );
        std::fs::write(server.join(name), b"second saved metadata").unwrap();
        assert_eq!(
            read_server_file(&remote, &path, &ssh).unwrap(),
            b"second saved metadata"
        );
        assert_eq!(std::fs::read(&path).unwrap(), b"stale cached metadata");
        std::fs::remove_file(server.join(name)).unwrap();
        assert!(read_server_file(&remote, &path, &ssh)
            .unwrap_err()
            .contains("Could not read fresh file"));
        assert!(read_server_file(&remote, &local.join("../outside"), &ssh).is_err());
        assert!(read_server_file(&remote, &server.join(name), &ssh).is_err());
        // A connection can fail after producing bytes: never return a partial file.
        std::fs::write(
            &ssh,
            "#!/bin/sh\nprintf partial-file\nprintf disconnected >&2\nexit 255\n",
        )
        .unwrap();
        assert!(read_server_file(&remote, &path, &ssh)
            .unwrap_err()
            .contains("disconnected"));
        std::fs::remove_dir_all(temp).unwrap();
    }
    use std::{
        fs,
        os::unix::fs::MetadataExt,
        time::{Duration, UNIX_EPOCH},
    };
    #[test]
    fn routing_distinguishes_server_zfs_from_client_mounts() {
        let base = "1 0 0:1 / / rw - ext4 /dev/root rw\n";
        let client = format!("{base}2 1 0:2 / /mnt/library rw - fuse.sshfs user@server rw\n");
        let server = format!("{base}2 1 0:2 / /mnt/library rw - zfs pool/share rw\n");
        assert!(network_root(
            std::path::Path::new("/mnt/library/pictures"),
            &client
        ));
        assert!(!network_root(
            std::path::Path::new("/mnt/library/pictures"),
            &server
        ));
        assert!(!network_root(
            std::path::Path::new("/mnt/library-other"),
            &client
        ));
        let nested = format!("{client}3 2 0:3 / /mnt/library/local rw - tmpfs tmpfs rw\n");
        assert!(!network_root(
            std::path::Path::new("/mnt/library/local/a"),
            &nested
        ));
        let spaced = format!("{base}2 1 0:2 / /mnt/my\\040share rw - nfs4 host:/share rw\n");
        assert!(network_root(
            std::path::Path::new("/mnt/my share/a"),
            &spaced
        ));
    }

    #[test]
    fn single_file_protocol_preserves_media_and_rejects_stale_revisions() {
        let root = std::env::temp_dir().join(format!("memetag-single-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("fixture.webp");
        let before = include_bytes!("../tests/fixtures/animated.webp");
        fs::write(&path, before).unwrap();
        let md = fs::metadata(&path).unwrap();
        let fields = [
            ("text".to_string(), "human correction".to_string()),
            ("textSource".to_string(), "manual".to_string()),
        ]
        .into_iter()
        .collect();
        let packet = xmp::merge(None, &["new tag".into()], &fields).unwrap();
        let after = containers::set_xmp(before, &packet).unwrap();
        let mut request = SingleWrite {
            write_version: 1,
            root: root.clone(),
            path: "fixture.webp".into(),
            expected: crate::writer::revision(before),
            replacement: crate::writer::revision(&after),
            packet,
        };
        let encoded = serde_json::to_string(&request).unwrap();
        assert!(
            serde_json::from_str::<Request>(&encoded).is_err(),
            "old helpers must reject new requests"
        );
        let reply = single_write(&request).unwrap();
        assert!(reply.report.ok());
        assert_eq!(reply.revision, request.replacement);
        assert_eq!(fs::read(&path).unwrap(), after);
        assert_eq!(
            fs::metadata(&path).unwrap().modified().unwrap(),
            md.modified().unwrap()
        );
        assert_eq!(fs::metadata(&path).unwrap().ino(), md.ino());
        assert!(single_write(&request)
            .err()
            .unwrap()
            .contains("File changed"));
        assert_eq!(fs::read(&path).unwrap(), after);
        request.path = "../escape.webp".into();
        assert!(single_write(&request).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn tag_only_batches_preserve_media_ocr_and_retry_without_writes() {
        let root = std::env::temp_dir().join(format!("memetag-batch-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("fixture.webp");
        fs::write(&path, include_bytes!("../tests/fixtures/animated.webp")).unwrap();
        let time = UNIX_EPOCH + Duration::new(1_500_000_000, 987_654_321);
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(time))
            .unwrap();
        let mut c = crate::cfg();
        c.root = root.clone();
        c.sources = None;
        c.active_source = None;
        c.db = root.join("index.sqlite");
        c.vocab = Vocab::default();
        tagger::edit_file(
            &c,
            &path,
            &["+keep".into(), "+remove this".into()],
            &[
                ("text", "Reviewed caption".into()),
                ("textSource", "manual".into()),
            ],
        )
        .unwrap();
        let before = fs::read(&path).unwrap();
        let metadata = fs::metadata(&path).unwrap();
        let db = Db::open_cfg(&c).unwrap();
        db.upsert_file(&root, &path, &c.vocab).unwrap();
        db.conn
            .execute(
                "INSERT INTO text VALUES('fixture.webp','late machine result')",
                [],
            )
            .unwrap();
        let mut request = Request {
            root: root.clone(),
            path: "fixture.webp".into(),
            add: BTreeSet::from(["new tag".into()]),
            remove: BTreeSet::from(["remove this".into()]),
            vocab: c.vocab.clone(),
        };
        let reply = apply(&request).unwrap();
        assert!(reply.changed);
        db.import_tag_scan(reply.scan).unwrap();
        let row = db.all().unwrap().pop().unwrap();
        assert!(
            row.tags.contains("keep")
                && row.tags.contains("new tag")
                && !row.tags.contains("remove this")
        );
        assert_eq!(row.text, "Reviewed caption");
        assert_eq!(
            db.conn
                .query_row("SELECT body FROM text", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "late machine result"
        );
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), time);
        assert_eq!(fs::metadata(&path).unwrap().ino(), metadata.ino());
        assert_eq!(
            fs::metadata(&path).unwrap().created().ok(),
            metadata.created().ok()
        );
        assert_eq!(
            containers::strip_xmp(&fs::read(&path).unwrap()).unwrap(),
            containers::strip_xmp(&before).unwrap()
        );
        let saved = fs::read(&path).unwrap();
        assert!(!apply(&request).unwrap().changed);
        assert_eq!(fs::read(&path).unwrap(), saved);
        request.path = "../outside.webp".into();
        assert!(apply(&request).is_err());
        request.path = "fixture.webp".into();
        request.remove.insert("new tag".into());
        assert!(apply(&request).is_err());
        #[cfg(feature = "gui")]
        {
            let (tx, rx) = mpsc::channel();
            run(
                c.clone(),
                vec!["fixture.webp".into()],
                BTreeSet::from(["must not write".into()]),
                BTreeSet::new(),
                Arc::new(AtomicBool::new(true)),
                tx,
                egui::Context::default(),
            );
            assert!(matches!(rx.recv().unwrap(), Event::Done(Ok(_))));
            assert_eq!(fs::read(&path).unwrap(), saved);
            let (tx, rx) = mpsc::channel();
            run(
                c,
                vec!["missing.webp".into(), "fixture.webp".into()],
                BTreeSet::from(["after failure".into()]),
                BTreeSet::new(),
                Arc::new(AtomicBool::new(false)),
                tx,
                egui::Context::default(),
            );
            assert!(matches!(
                rx.recv().unwrap(),
                Event::File { result: Err(_), .. }
            ));
            assert!(matches!(
                rx.recv().unwrap(),
                Event::File {
                    result: Ok(true),
                    ..
                }
            ));
            assert!(matches!(rx.recv().unwrap(), Event::Done(Ok(_))));
            let row = db.all().unwrap().pop().unwrap();
            assert!(row.tags.contains("after failure"));
            assert_eq!(row.text, "Reviewed caption");
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), time);
            drop(db);
            fs::remove_dir_all(root).unwrap();
        }
    }
}
