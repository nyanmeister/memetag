//! The grab window: left-click copies and closes; middle-click copies and stays; right-click opens an external viewer.
//! Video tiles animate through a storyboard strip (STRIP_FRAMES frames sampled across the video), like YouTube hover previews.
//! Conventions the cards and controls follow (kinds of card, Escape order, toasts, colours) are in `widgets`.
use crate::grab;
use crate::index::{Db, FileRow};
use crate::{query, widgets, Cfg};
use eframe::egui;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

const TILE: f32 = 200.0;
/// finished decodes uploaded to the GPU per frame (a 5 MB strip uploads in ~1 ms; the decode itself is off-thread)
const UPLOADS_PER_FRAME: usize = 4;
/// a texture is only evictable after this many frames unseen, so the visible screen never thrashes
const EVICT_AGE: u64 = 120;

struct Tex {
    handle: egui::TextureHandle,
    frames: u32,
    rate: f64,
    size: egui::Vec2,
    bytes: usize,
    last_used: u64,
}

/// A tile decoded on the worker pool: PNG → RGBA takes ~30 ms for a 24-frame strip (6144×256), far too long for the
/// frame loop, so the UI thread never decodes — it only uploads what the workers hand back. `None` = undecodable.
struct Decoded {
    id: String,
    img: Option<(egui::ColorImage, u32, f64, egui::Vec2)>,
}

/// Everything background threads tell the UI: viewer results, clipboard results, the OCR done-set refresh.
enum Msg {
    Toast(String),
    Copied {
        ok: bool,
        close: bool,
        message: String,
    },
    OcrDone(HashSet<String>),
    Pulled(crate::pull::Outcome),
    /// a vocab sync (a Save/rename push, or a grab's pull) found both sides changed: raise the resolve card
    VocabConflict(crate::vocabsync::Conflict),
    /// a resolution finished on a worker thread: fold the new rules in (take-theirs, merge) or just report (keep-mine)
    VocabResolved(Result<crate::vocabconflict::Resolved, String>),
    VocabRefresh,
    LibraryRows(Result<Vec<FileRow>, String>),
    Ran {
        cmd: &'static str,
        report: String,
    },
    Hashed(Result<HashMap<String, Vec<u8>>, String>),
    Grouped {
        gen: u64,
        groups: Vec<Vec<String>>,
        scope: usize,
    },
}

/// Paths the configured OCR engine has finished, plus human-reviewed ones: the tiles NOT in this set wear the pending badge
/// (a temporary pending mark).
fn ocr_done_set(db: &std::path::Path, engine: &str) -> Result<HashSet<String>, String> {
    let db = Db::open(db)?;
    let mut set = HashSet::new();
    let mut st = db
        .conn
        .prepare("SELECT path FROM text_meta WHERE engine=?1 UNION SELECT path FROM manual_text")
        .map_err(|e| e.to_string())?;
    for p in st
        .query_map([engine], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .flatten()
    {
        set.insert(p);
    }
    Ok(set)
}

fn decode_tile(c: &Cfg, row: &FileRow) -> Option<(egui::ColorImage, u32, f64, egui::Vec2)> {
    let (path, frames, dur) = match grab::ensure_strip(c, row) {
        Some((p, n, d)) => (p, n, d),
        None => (grab::ensure_thumbs(c, &[row]).pop()?.0, 1, 1.0),
    };
    // natural playback rate = sampled frames over real duration, capped by the configured ceiling; never slower than 2 fps
    let rate = if frames <= 1 {
        0.0
    } else if row.kind == "video" {
        (frames as f64 / dur.max(0.05)).clamp((c.preview_fps * 0.66).max(2.0), c.preview_fps)
    }
    // videos scrub fast, like YouTube
    else {
        (frames as f64 / dur.max(0.05)).clamp(2.0, c.preview_fps)
    }; // GIFs keep their own speed
    let img = image::open(&path).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Some((
        egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], img.as_raw()),
        frames,
        rate,
        egui::vec2(w as f32, h as f32),
    ))
}

struct App {
    cfg: Cfg,
    rows: Vec<FileRow>,
    q: String,
    err: Option<String>,
    hits: Vec<usize>,
    /// The last query that parsed: while a broken one is being typed, its results are what the grid shows,
    /// evaluated afresh whenever the rows change.
    last_ok: Option<query::Expr>,
    tex: HashMap<String, Option<Tex>>,
    toast: Option<(String, Instant)>,
    first: bool,
    frame_no: u64,
    tex_bytes: usize,
    pending: HashSet<String>,
    req_tx: Option<mpsc::Sender<FileRow>>,
    res_rx: Option<mpsc::Receiver<Decoded>>,
    msg_tx: mpsc::Sender<Msg>,
    msg_rx: mpsc::Receiver<Msg>,
    copying: bool,
    pulling: bool,
    ocr_done: HashSet<String>,
    ocr_engine: String,
    ocr_pending: usize,
    editing: Option<(usize, crate::editor::EditorUi)>,
    selected: HashSet<usize>,
    select_mode: bool,
    batch: Option<crate::batch::BatchUi>,
    search_complete: crate::search_complete::SearchComplete,
    help_open: bool,
    ocr_help_open: bool,
    implications: Option<crate::implications::ImplicationsUi>,
    /// The resolve card for a vocab sync conflict (both machines changed the rules): raised by a Save/rename push or a
    /// grab's pull that found a divergence, closed when a choice has been applied.
    vocab_conflict: Option<crate::vocabconflict::VocabConflictUi>,
    tags_card: Option<crate::tags_card::TagsUi>,
    /// A rename from the Tags card in flight in the batch panel: (old, new, files in the plan). When the batch
    /// closes with every file done, the vocabulary follows; a partial run leaves the rules alone for the retry.
    pending_rename: Option<(String, String, usize)>,
    /// Tagging assistant: the progress line under the select bar, untagged tiles outlined, tagged ones dimmed
    assist: bool,
    /// Files in the current results with hand-written tags but fewer than `THIN_BELOW`.
    thin_hits: usize,
    thin_all: usize,
    tagged_all: usize,
    tagged_hits: usize,
    /// Window size (points at zoom 1) and zoom factor as last seen, written to `window.toml` beside the index on exit
    /// so the next grab opens the same (wishlist, 2026-09-22).
    win_seen: Option<(f32, f32, f32)>,
    // similar: — the hash resolver, built on a thread the first time a query asks (None again after a pull adds rows)
    similar: Option<crate::similar::Lookup>,
    hashing: bool,
    // the Duplicates card; `dedup_gen` tells a late grouping thread's answer from the current one
    dedup: Option<crate::dedup::DedupUi>,
    propose: Option<crate::propose_card::ProposeUi>,
    library_menu: Option<crate::library_ui::LibraryUi>,
    dedup_gen: u64,
    /// what the OCR card shows under each command it ran (keyed by the command text; `None` while it runs)
    ocr_runs: HashMap<&'static str, Option<String>>,
}

/// Key in `ocr_runs` for the service-state line the card fetches when it opens.
const SERVICE_STATE: &str = "state";

impl App {
    /// Everything not handed in starts empty or off; one field per line, so adding state is one line here and one in the struct.
    fn new(
        c: &Cfg,
        rows: Vec<FileRow>,
        q: &str,
        msg_tx: mpsc::Sender<Msg>,
        msg_rx: mpsc::Receiver<Msg>,
        ocr_done: HashSet<String>,
        ocr_engine: String,
        search_complete: crate::search_complete::SearchComplete,
    ) -> App {
        App {
            cfg: c.clone(),
            pulling: false,
            rows,
            q: q.to_string(),
            msg_tx,
            msg_rx,
            ocr_done,
            ocr_engine,
            search_complete,
            err: None,
            hits: vec![],
            last_ok: None,
            tex: HashMap::new(),
            toast: None,
            first: true,
            frame_no: 0,
            tex_bytes: 0,
            pending: HashSet::new(),
            req_tx: None,
            res_rx: None,
            copying: false,
            ocr_pending: 0,
            editing: None,
            selected: HashSet::new(),
            select_mode: false,
            batch: None,
            help_open: false,
            ocr_help_open: false,
            implications: None,
            vocab_conflict: None,
            tags_card: None,
            pending_rename: None,
            assist: false,
            thin_hits: 0,
            thin_all: 0,
            tagged_all: 0,
            tagged_hits: 0,
            win_seen: None,
            ocr_runs: HashMap::new(),
            similar: None,
            hashing: false,
            dedup: None,
            propose: None,
            library_menu: None,
            dedup_gen: 0,
        }
    }
    /// Derpibooru-style syntax card behind the ? button. Escape closes it before the window.
    fn show_help(&mut self, ctx: &egui::Context) {
        let max = ctx.content_rect().size() - egui::vec2(40.0, 60.0);
        egui::Window::new("Search syntax").open(&mut self.help_open).collapsible(false).resizable(true).vscroll(true)
            .default_size(egui::vec2(800.0, 700.0).min(max)).max_size(max).show(ctx, |ui| {
            egui::Grid::new("syntax-help").num_columns(2).spacing([24.0, 4.0]).striped(true).show(ui, |ui| {
                for (example, meaning) in crate::query::SYNTAX_HELP {
                    if example.is_empty() { ui.end_row(); ui.label(egui::RichText::new(*meaning).strong()); ui.end_row(); continue; }
                    ui.label(egui::RichText::new(*example).monospace());
                    ui.scope(|ui| { ui.set_max_width(620.0); ui.add(egui::Label::new(*meaning).wrap()); }); // long explanations wrap instead of widening the card
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
            ui.small("Fields and operators are case-insensitive. Syntax after derpibooru.org/pages/search_syntax.");
        });
    }
    /// How to run the OCR pass, behind the OCR button and the "awaiting OCR" count; the text comes from `ocr::help`.
    /// Every command has Run and Copy beside it; a quiet command's output lands under it, a long one opens a terminal.
    fn show_ocr_help(&mut self, ctx: &egui::Context) {
        if !self.ocr_help_open {
            return;
        }
        let max = ctx.content_rect().size() - egui::vec2(40.0, 60.0);
        let sections = crate::ocr::help(&self.cfg, self.ocr_pending);
        let mut run: Option<(&'static str, crate::ocr::Run)> = None;
        let mut copy: Option<&'static str> = None;
        let mut open = self.ocr_help_open;
        egui::Window::new("OCR")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .vscroll(true)
            .default_size(egui::vec2(720.0, 620.0).min(max))
            .max_size(max)
            .show(ctx, |ui| {
                for (heading, lines) in sections {
                    ui.label(egui::RichText::new(heading).strong());
                    ui.add_space(2.0);
                    if heading.starts_with("With systemd") {
                        match self.ocr_runs.get(SERVICE_STATE) {
                            Some(Some(s)) => {
                                ui.label(format!("memetag-ocr.service is {s}."));
                            }
                            _ => {
                                ui.label("memetag-ocr.service: checking…");
                            }
                        }
                    }
                    for line in lines {
                        match line {
                            crate::ocr::HelpLine::Text(t) => {
                                ui.label(t);
                            }
                            crate::ocr::HelpLine::Cmd(c, how) => {
                                let running = matches!(self.ocr_runs.get(c), Some(None));
                                ui.horizontal(|ui| {
                                    ui.code(c);
                                    let hint = if how == crate::ocr::Run::Terminal {
                                        "Open in a terminal window"
                                    } else {
                                        "Run now; the outcome shows below"
                                    };
                                    if ui
                                        .add_enabled(!running, egui::Button::new("Run").small())
                                        .on_hover_text(hint)
                                        .clicked()
                                    {
                                        run = Some((c, how));
                                    }
                                    if ui
                                        .add(egui::Button::new("Copy").small())
                                        .on_hover_text("Copy the command")
                                        .clicked()
                                    {
                                        copy = Some(c);
                                    }
                                    if running {
                                        ui.spinner();
                                    }
                                });
                                if let Some(Some(report)) = self.ocr_runs.get(c) {
                                    ui.indent(c, |ui| {
                                        ui.label(egui::RichText::new(report).monospace().weak());
                                    });
                                }
                            }
                        }
                    }
                    ui.add_space(10.0);
                }
            });
        self.ocr_help_open = open;
        if let Some((cmd, how)) = run {
            self.run_ocr_cmd(cmd, how, ctx);
        }
        if let Some(cmd) = copy {
            self.copy_text(cmd.to_string(), ctx);
        }
    }
    /// Run one card command. Quiet: on a thread, output back through `Msg::Ran`. Terminal: spawn and toast the terminal's name.
    fn run_ocr_cmd(&mut self, cmd: &'static str, how: crate::ocr::Run, ctx: &egui::Context) {
        if how == crate::ocr::Run::Terminal {
            self.toast = Some((
                match crate::ocr::run_in_terminal(cmd) {
                    Ok(t) => format!("Opened in {t}"),
                    Err(e) => e,
                },
                Instant::now(),
            ));
            return;
        }
        self.ocr_runs.insert(cmd, None);
        let (tx, ctx) = (self.msg_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let report = if cmd == SERVICE_STATE {
                crate::ocr::service_state()
            } else {
                crate::ocr::run_quiet(cmd)
            };
            if tx.send(Msg::Ran { cmd, report }).is_ok() {
                ctx.request_repaint();
            }
        });
    }
    /// Text to the clipboard through our own owner, so it is still there after the window closes.
    fn copy_text(&mut self, text: String, ctx: &egui::Context) {
        if text.is_empty() {
            return;
        }
        let (tx, ctx) = (self.msg_tx.clone(), ctx.clone());
        let what = if text.contains('\n') || text.chars().count() > 40 {
            format!("Copied {} characters of text", text.chars().count())
        } else {
            format!("Copied \"{text}\"")
        };
        std::thread::spawn(move || {
            let message = match crate::clipboard::own_text("CLIPBOARD", &text) {
                Ok(()) => what,
                Err(e) => format!("Copy failed: {e}"),
            };
            if tx.send(Msg::Toast(message)).is_ok() {
                ctx.request_repaint();
            }
        });
    }
    fn open_external(&mut self, index: usize, ctx: &egui::Context) {
        let path = match self.cfg.file_path(&self.rows[index].path) {
            Ok(p) => p,
            Err(e) => {
                self.toast = Some((e, Instant::now()));
                return;
            }
        };
        let video = self.rows[index].kind == "video";
        let tx = self.msg_tx.clone();
        let ctx = ctx.clone();
        self.toast = Some(("Opening original…".into(), Instant::now()));
        std::thread::spawn(move || {
            let report = |message: String| {
                let _ = tx.send(Msg::Toast(message));
                ctx.request_repaint();
            };
            match crate::viewer::launch(&path, video) {
                Ok((viewer, mut child)) => {
                    report(format!("Opened in {}", viewer.name()));
                    match child.wait() {
                        Ok(status) if !status.success() => {
                            report(format!("{} exited with {status}", viewer.name()))
                        }
                        Err(e) => report(format!("{}: {e}", viewer.name())),
                        _ => {}
                    }
                }
                Err(e) => report(format!("Open failed: {e}")),
            }
        });
    }

    /// The card fetches the service state each time it opens, so start/stop have a current line to read against.
    /// The Implications card. Applied rules land in the config the rest of the window uses (batch edits, the editor's
    /// canonicalisation) and in the open rows, by path, so the grid and completion reflect them without a reload.
    fn show_implications(&mut self, ctx: &egui::Context) {
        let Some(im) = &mut self.implications else {
            return;
        };
        match im.show(ctx, &self.cfg, &self.rows) {
            crate::implications::Action::None => {}
            crate::implications::Action::Close => {
                self.implications = None;
            }
            crate::implications::Action::Applied { vocab, changed } => {
                let n = changed.len();
                self.apply_vocab(
                    ctx,
                    vocab,
                    changed,
                    format!("Implications saved; {n} files changed"),
                    None,
                );
            }
        }
    }
    /// A locally saved vocabulary (an Implications Save or a Tags rename): take the rules over, refresh what shows
    /// them, then push them to the server so every machine gets the change (a conflict raises the resolve card).
    fn apply_vocab(
        &mut self,
        ctx: &egui::Context,
        vocab: crate::vocab::Vocab,
        changed: Vec<(String, std::collections::BTreeSet<String>)>,
        message: String,
        rename: Option<(&str, &str)>,
    ) {
        self.absorb_vocab(vocab, changed, message, rename);
        self.push_vocab_async(ctx);
    }
    /// Fold a vocabulary and the rows `Db::reimply` changed into the open window, without pushing. Shared by the local
    /// edits (through `apply_vocab`) and by a conflict resolution that already settled with the server.
    fn absorb_vocab(
        &mut self,
        vocab: crate::vocab::Vocab,
        changed: Vec<(String, std::collections::BTreeSet<String>)>,
        message: String,
        rename: Option<(&str, &str)>,
    ) {
        self.cfg.vocab = vocab;
        let by_path: HashMap<String, usize> = self
            .rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.path.clone(), i))
            .collect();
        for (p, tags) in changed {
            if let Some(&i) = by_path.get(&p) {
                self.rows[i].tags = tags;
            }
        }
        if let Some(im) = &mut self.implications {
            // an open card holds its own copy of the rules: a rename saved behind its back must reach it, or its
            // next Save writes the old rules over the new file (review, 2026-09-22)
            im.follow(&self.cfg.vocab, rename);
            im.refresh(&self.rows);
        }
        self.search_complete = crate::search_complete::SearchComplete::new(&self.cfg, &self.rows);
        self.refilter();
        self.toast = Some((message, Instant::now()));
    }
    /// After a local rule edit, push it to the server on a thread. A clean push (or a seed) is silent — the Save
    /// already toasted. A conflict comes back as `Msg::VocabConflict`, a transport failure as a toast.
    fn push_vocab_async(&self, ctx: &egui::Context) {
        let Ok(Some(store)) = crate::vocabsync::store_for(&self.cfg) else {
            return; // a local root, or no [pull_remote]: nothing to push to
        };
        let (tx, ctx) = (self.msg_tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            let env = crate::vocabsync::Env::from_home();
            let msg = match crate::vocabsync::sync(&store, &env) {
                Ok(crate::vocabsync::Outcome::Conflict(cf)) => Some(Msg::VocabConflict(cf)),
                Ok(crate::vocabsync::Outcome::Pulled) => Some(Msg::VocabRefresh),
                Ok(_) => None,
                Err(e) => Some(Msg::Toast(format!(
                    "vocab: your rules were saved but not synced to the server ({e})"
                ))),
            };
            if let Some(m) = msg {
                if tx.send(m).is_ok() {
                    ctx.request_repaint();
                }
            }
        });
    }
    /// Carry a conflict resolution out on a thread (ssh write, then reimply if the local file changed) and fold the
    /// result back through `Msg::VocabResolved`.
    fn resolve_vocab_async(
        &self,
        resolution: crate::vocabconflict::Resolution,
        ctx: &egui::Context,
    ) {
        let Ok(Some(store)) = crate::vocabsync::store_for(&self.cfg) else {
            return;
        };
        let (tx, ctx, db) = (self.msg_tx.clone(), ctx.clone(), self.cfg.db.clone());
        std::thread::spawn(move || {
            let env = crate::vocabsync::Env::from_home();
            let res = crate::vocabconflict::resolve(&store, &env, &db, resolution);
            if tx.send(Msg::VocabResolved(res)).is_ok() {
                ctx.request_repaint();
            }
        });
    }
    /// Open the resolve card on a conflict, or refresh it if one is already up, and point the user at it.
    fn raise_vocab_conflict(&mut self, cf: crate::vocabsync::Conflict) {
        match &mut self.vocab_conflict {
            Some(card) => card.update(cf),
            None => self.vocab_conflict = Some(crate::vocabconflict::VocabConflictUi::new(cf)),
        }
        self.toast = Some((
            "Rules differ from the server — resolve in the card".into(),
            Instant::now(),
        ));
    }
    /// The vocab conflict resolve card, when one is up.
    fn show_vocab_conflict(&mut self, ctx: &egui::Context) {
        let Some(card) = &mut self.vocab_conflict else {
            return;
        };
        match card.show(ctx) {
            crate::vocabconflict::Action::None => {}
            crate::vocabconflict::Action::Close => self.vocab_conflict = None,
            crate::vocabconflict::Action::Resolve(r) => self.resolve_vocab_async(r, ctx),
        }
    }
    /// The files no longer carry `from`: update rules and aliases that mention it to say `to`. Reads vocab.toml afresh
    /// so rules edited by hand since the window opened survive, as the Implications card does; the file goes first,
    /// then the index, the same order as that card's Save.
    fn finish_rename(&mut self, ctx: &egui::Context, from: &str, to: &str) {
        let mut vocab = match &self.cfg.vocab.path {
            Some(p) => crate::vocab::Vocab::load(p),
            None => self.cfg.vocab.clone(),
        };
        if let Some(e) = &vocab.broken {
            self.toast = Some((
                format!("Rules still mention \"{from}\": vocab.toml did not parse ({e})"),
                Instant::now(),
            ));
            return;
        }
        let renamed = vocab.rename(from, to);
        if !renamed.changed {
            return;
        }
        let left = if renamed.not_carried.is_empty() {
            String::new()
        } else {
            format!(
                "; not carried onto \"{to}\": implies {} (see Implications)",
                renamed.not_carried.join(", ")
            )
        };
        let applied = vocab.save().and_then(|_| {
            Db::open_cfg(&self.cfg).and_then(|db| {
                db.reimply(&vocab)
                    .map_err(|e| format!("vocab.toml saved, but the index was not updated: {e}"))
            })
        });
        match applied {
            Ok(changed) => {
                let n = changed.len();
                self.apply_vocab(
                    ctx,
                    vocab,
                    changed,
                    format!(
                        "Rules mentioning \"{from}\" now say \"{to}\"; {n} files changed{left}"
                    ),
                    Some((from, to)),
                );
            }
            Err(e) => {
                self.toast = Some((
                    format!("Rules still mention \"{from}\": {e}"),
                    Instant::now(),
                ));
            }
        }
    }
    /// One Escape closes the topmost thing only, front to back: the search's suggestion popup, then the cards in the
    /// order they stack (Implications over OCR over Search syntax), then the window itself.
    fn escape(&mut self, ctx: &egui::Context) {
        if !crate::widgets::escape(ctx) {
            return;
        }
        if self.search_complete.dismiss(&self.q) {
            return;
        }
        if let Some(im) = &mut self.implications {
            im.request_close();
            return;
        }
        if let Some(t) = &mut self.tags_card {
            t.request_close();
            return;
        }
        if let Some(p) = &mut self.propose {
            p.request_close();
            return;
        }
        if let Some(d) = &mut self.dedup {
            d.request_close();
            return;
        }
        if self.ocr_help_open {
            self.ocr_help_open = false;
            return;
        }
        if self.help_open {
            self.help_open = false;
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
    fn toggle_ocr_help(&mut self, ctx: &egui::Context) {
        self.ocr_help_open = !self.ocr_help_open;
        if self.ocr_help_open {
            self.run_ocr_cmd(SERVICE_STATE, crate::ocr::Run::Quiet, ctx);
        }
    }
    fn budget(&self) -> usize {
        self.cfg.texture_budget_mb * 1024 * 1024
    }
    /// Decode workers: a quarter of the machine like the index (at least 2). Each finished tile wakes the UI.
    fn start_decode_workers(&mut self, ctx: egui::Context) {
        self.req_tx = None;
        self.res_rx = None;
        let (req_tx, req_rx) = mpsc::channel::<FileRow>();
        let (res_tx, res_rx) = mpsc::channel::<Decoded>();
        let req_rx = std::sync::Arc::new(std::sync::Mutex::new(req_rx));
        for _ in 0..self.cfg.index_threads.max(2) {
            let (rx, tx, ctx, cfg) = (
                req_rx.clone(),
                res_tx.clone(),
                ctx.clone(),
                self.cfg.clone(),
            );
            std::thread::spawn(move || loop {
                let row = match rx.lock().map(|g| g.recv()) {
                    Ok(Ok(r)) => r,
                    _ => return,
                };
                let img = decode_tile(&cfg, &row);
                if tx
                    .send(Decoded {
                        id: row.id.clone(),
                        img,
                    })
                    .is_err()
                {
                    return;
                }
                ctx.request_repaint();
            });
        }
        self.req_tx = Some(req_tx);
        self.res_rx = Some(res_rx);
    }
    fn start_workers(&mut self, ctx: egui::Context) {
        self.start_decode_workers(ctx.clone());
        // the merge with the server runs beside the window, not before it; the outcome lands through the message channel
        let (tx, ctx2, cfg) = (self.msg_tx.clone(), ctx.clone(), self.cfg.clone());
        self.pulling = true;
        std::thread::spawn(move || {
            let o = crate::pull::for_grab(&cfg);
            if tx.send(Msg::Pulled(o)).is_ok() {
                ctx2.request_repaint();
            }
        });
        // the OCR service writes the index while the window is open: refresh the done-set every 15 s so badges fall off as it works
        let (tx, ctx, db, engine) = (
            self.msg_tx.clone(),
            ctx.clone(),
            self.cfg.db.clone(),
            self.ocr_engine.clone(),
        );
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(15));
            if let Ok(set) = ocr_done_set(&db, &engine) {
                if tx.send(Msg::OcrDone(set)).is_err() {
                    return;
                }
                ctx.request_repaint();
            }
        });
    }
    /// Fold a finished pull into the open grid: rows rewritten by the merge are reloaded, gone ones dropped, new ones sorted
    /// into place (newest first, as `Db::all` orders), and the selection follows its paths through the reshuffle.
    fn apply_pull(&mut self, o: crate::pull::Outcome) {
        if o.vocab_changed {
            self.refresh_vocab();
        }
        if o.written.is_empty() && o.gone.is_empty() {
            return;
        }
        let db = match Db::open_cfg(&self.cfg) {
            Ok(d) => d,
            Err(e) => {
                self.toast = Some((format!("pull: {e}"), Instant::now()));
                return;
            }
        };
        let selected: HashSet<String> = self
            .selected
            .iter()
            .map(|&i| self.rows[i].path.clone())
            .collect();
        let before: HashSet<String> = self.rows.iter().map(|r| r.path.clone()).collect();
        let drop: HashSet<&str> = o
            .written
            .iter()
            .chain(o.gone.iter())
            .map(String::as_str)
            .collect();
        self.rows.retain(|r| !drop.contains(r.path.as_str()));
        self.similar = None; // new or changed rows have no hash in the resolver; the next similar: query rebuilds it (only the missing ones get hashed)
        if let Some(d) = &mut self.dedup {
            d.remap(&self.rows);
        }
        if let Some(p) = &mut self.propose {
            p.remap(&self.rows);
        }
        let mut fresh = 0usize;
        for p in &o.written {
            if let Ok(Some(r)) = db.row(p) {
                if !db.scope.contains(Path::new(&r.path)) {
                    continue;
                }
                if !before.contains(p) {
                    fresh += 1;
                }
                self.rows.push(r);
            }
        }
        self.rows.sort_by(|a, b| {
            b.created_at
                .partial_cmp(&a.created_at)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.path.cmp(&b.path))
        });
        self.selected = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| selected.contains(&r.path))
            .map(|(i, _)| i)
            .collect();
        self.search_complete.refresh(&self.rows);
        self.refilter();
        let mut parts = vec![];
        if fresh > 0 {
            parts.push(format!("{fresh} new"));
        }
        if o.written.len() > fresh {
            parts.push(format!("{} changed", o.written.len() - fresh));
        }
        if !o.gone.is_empty() {
            parts.push(format!("{} gone", o.gone.len()));
        }
        self.toast = Some((
            format!("Pulled from the server: {}", parts.join(", ")),
            Instant::now(),
        ));
    }
    /// Read current state when consuming the message, not its potentially stale worker snapshot.
    /// Reapply here as well: a previous pull may have saved the rules then failed to update SQLite.
    fn refresh_vocab(&mut self) {
        let result = (|| {
            let path = self
                .cfg
                .vocab
                .path
                .as_ref()
                .ok_or("No vocabulary file configured")?;
            let _lock = crate::vocab::Vocab::lock(path)?;
            let vocab = crate::vocab::Vocab::load(path);
            if let Some(e) = &vocab.broken {
                return Err(e.clone());
            }
            let db = Db::open_cfg(&self.cfg)?;
            db.reimply(&vocab)?;
            // The worker may already have reapplied the rules, so the delta alone is insufficient.
            let tags = db.all()?.into_iter().map(|r| (r.path, r.tags)).collect();
            Ok((vocab, tags))
        })();
        match result {
            Ok((vocab, tags)) => {
                self.absorb_vocab(vocab, tags, "Rules refreshed from the server".into(), None)
            }
            Err(e) => self.toast = Some((format!("Rules refresh failed: {e}"), Instant::now())),
        }
    }
    fn awaits_ocr(&self, i: usize) -> bool {
        let r = &self.rows[i];
        r.kind == "image" && !self.ocr_done.contains(&r.path)
    }
    fn recount_ocr(&mut self) {
        self.ocr_pending = self.hits.iter().filter(|&&i| self.awaits_ocr(i)).count();
    }
    /// "Tagged" for the assistant means tags explicitly written into the file; folder and implied tags do not count.
    fn recount_tagged(&mut self) {
        self.tagged_all = self.rows.iter().filter(|r| hand_tagged(r)).count();
        self.tagged_hits = self
            .hits
            .iter()
            .filter(|&&i| hand_tagged(&self.rows[i]))
            .count();
        self.thin_hits = self
            .hits
            .iter()
            .filter(|&&i| tier(&self.rows[i]) == Tier::Thin)
            .count();
        self.thin_all = self.rows.iter().filter(|r| tier(r) == Tier::Thin).count();
    }
    /// Narrow the search to files with a few hand-written tags but fewer than the assistant's threshold.
    fn show_thin(&mut self) {
        let q = self.q.trim();
        let thin = format!("tag_count.gt:0, tag_count.lt:{THIN_BELOW}");
        self.q = if q.is_empty() {
            thin
        } else {
            format!("({q}), {thin}")
        };
        self.refilter();
    }
    /// Open the editor on the next file needing tags in the current results, going on from `slot` (the position
    /// the file being left had in the results, so it still works when that file just dropped out of them), and
    /// wrapping round to the top. `current` is never chosen again.
    fn open_next(&mut self, current: usize, slot: Option<usize>, ctx: &egui::Context) {
        let start = slot.unwrap_or(0).min(self.hits.len());
        let (tail, head) = self.hits.split_at(start);
        let next = head
            .iter()
            .chain(tail.iter())
            .copied()
            .find(|&i| i != current && tier(&self.rows[i]) != Tier::Tagged);
        match next {
            Some(i) => {
                self.editing = Some((
                    i,
                    crate::editor::EditorUi::new(
                        self.cfg.clone(),
                        self.rows[i].path.clone(),
                        ctx.clone(),
                    ),
                ));
            }
            None => {
                self.toast = Some((
                    "Nothing left to tag in these results".into(),
                    Instant::now(),
                ));
            }
        }
    }
    /// Narrow the search to files with no hand-written tags, keeping whatever the query already said.
    fn show_untagged(&mut self) {
        let q = self.q.trim();
        self.q = if q.is_empty() {
            "tag_count:0".into()
        } else {
            format!("({q}), tag_count:0")
        };
        self.refilter();
    }
    /// Upload finished decodes, a few per frame; keep repainting while anything is still in flight.
    fn pump(&mut self, ctx: &egui::Context) {
        let mut done = vec![];
        if let Some(rx) = &self.res_rx {
            while done.len() < UPLOADS_PER_FRAME {
                match rx.try_recv() {
                    Ok(d) => done.push(d),
                    Err(_) => break,
                }
            }
        }
        let now = self.frame_no;
        for d in done {
            self.pending.remove(&d.id);
            let tex = d.img.map(|(ci, frames, rate, size)| {
                let bytes = ci.width() * ci.height() * 4;
                Tex {
                    handle: ctx.load_texture(d.id.clone(), ci, egui::TextureOptions::LINEAR),
                    frames,
                    rate,
                    size,
                    bytes,
                    last_used: now,
                }
            });
            if let Some(t) = &tex {
                self.tex_bytes += t.bytes;
            }
            self.tex.insert(d.id, tex);
        }
        self.evict();
        if !self.pending.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
    fn refilter(&mut self) {
        let mut expr = match query::parse(&self.q) {
            Ok(e) => {
                self.err = None;
                self.last_ok = Some(e.clone());
                e
            }
            Err(e) => {
                self.err = Some(e);
                // The last query that parsed keeps its results while this one is typed, evaluated afresh: a pull
                // can reorder or shrink the rows meanwhile, and hit indices kept from before pointed at other tiles,
                // or past the end (review, 2026-09-22).
                match &self.last_ok {
                    Some(e) => e.clone(),
                    None => {
                        self.hits.clear();
                        self.recount_ocr();
                        self.recount_tagged();
                        return;
                    }
                }
            }
        };
        if let Err(e) = crate::sources::resolve_query(&self.cfg, &mut expr) {
            self.err = Some(e);
            self.hits.clear();
            self.recount_ocr();
            self.recount_tagged();
            return;
        }
        let v = &self.cfg.vocab;
        let alias = |t: &str| v.canon(t);
        let wanted = query::similar_terms(&expr);
        if !wanted.is_empty() && self.similar.is_none() {
            self.hits.clear();
            self.start_hashing();
            self.recount_ocr();
            self.recount_tagged();
            return;
        }
        if let Some(l) = &mut self.similar {
            if let Some(e) = l.prepare(&wanted).into_iter().next() {
                self.err.get_or_insert(e);
            }
        }
        let lookup = &self.similar;
        let similar = |img: &str, path: &str| lookup.as_ref()?.distance(img, path);
        let mut hits: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, f)| query::eval(&expr, &query::Item::of(f, &similar), &alias))
            .map(|(i, _)| i)
            .collect();
        // an image search lists the closest first (stable, so ties stay newest first); Enter then copies the best match
        if let Some(img) = wanted.first() {
            hits.sort_by_key(|&i| similar(img, &self.rows[i].path).unwrap_or(u32::MAX));
        }
        self.hits = hits;
        self.recount_ocr();
        self.recount_tagged();
        if self.dedup.is_some() {
            self.compute_dupes();
        }
    }
    /// Fill the perceptual-hash cache on a thread (the first similar: query of a launch; seconds from the local thumbnails).
    fn start_hashing(&mut self) {
        if self.hashing {
            return;
        }
        self.hashing = true;
        self.toast = Some((
            "Image search: hashing thumbnails, the results follow in a moment…".into(),
            Instant::now(),
        ));
        let (tx, cfg, rows) = (self.msg_tx.clone(), self.cfg.clone(), self.rows.clone());
        std::thread::spawn(move || {
            let r =
                Db::open_cfg(&cfg).and_then(|db| crate::similar::ensure(&cfg, &db, &rows, false));
            let _ = tx.send(Msg::Hashed(r));
        });
    }
    /// The search becomes "files that look like this tile"; the path is quoted so parentheses and commas in it stay literal.
    fn show_similar(&mut self, i: usize) {
        self.search_similar_to(&self.rows[i].path.clone());
    }
    fn search_similar_to(&mut self, path: &str) {
        self.q = format!("similar:{}", query::quote_tag(path));
        self.refilter();
    }
    /// The camera button: search for whatever image is on the clipboard now (a value already resolved is forgotten first).
    fn search_clipboard(&mut self) {
        if let Some(l) = &mut self.similar {
            l.forget(crate::similar::CLIPBOARD);
        }
        self.q = format!("similar:{}", crate::similar::CLIPBOARD);
        self.refilter();
    }
    /// Group the current hits by look-alikes on a thread for the Duplicates card; needs the hashes, so it may wait for `start_hashing` first.
    fn compute_dupes(&mut self) {
        let Some(lookup) = &self.similar else {
            self.start_hashing();
            return;
        };
        let items: Vec<(String, Vec<u8>)> = self
            .hits
            .iter()
            .map(|&i| &self.rows[i])
            .filter_map(|r| lookup.hash(&r.path).map(|h| (r.path.clone(), h.clone())))
            .collect();
        self.dedup_gen += 1;
        let (gen, scope, tx) = (self.dedup_gen, self.hits.len(), self.msg_tx.clone());
        std::thread::spawn(move || {
            let groups = crate::similar::groups(
                &items.iter().map(|(_, h)| h.clone()).collect::<Vec<_>>(),
                crate::similar::DEFAULT_DISTANCE,
            );
            let groups = groups
                .into_iter()
                .map(|g| g.into_iter().map(|i| items[i].0.clone()).collect())
                .collect();
            let _ = tx.send(Msg::Grouped { gen, groups, scope });
        });
    }
    fn show_tags_card(&mut self, ctx: &egui::Context) {
        let Some(card) = &mut self.tags_card else {
            return;
        };
        match card.show(ctx, &self.cfg) {
            crate::tags_card::Action::None => {}
            crate::tags_card::Action::Close => self.tags_card = None,
            crate::tags_card::Action::Batch {
                paths,
                add,
                remove,
                rename,
            } => {
                self.pending_rename = rename.map(|(from, to)| (from, to, paths.len()));
                self.batch = Some(
                    crate::batch::BatchUi::new(self.cfg.clone(), paths, ctx.clone())
                        .plan(add, remove),
                );
            }
        }
    }
    fn show_propose(&mut self, ctx: &egui::Context) {
        let Some(mut card) = self.propose.take() else {
            return;
        };
        let textures: HashMap<usize, (egui::TextureId, egui::Vec2, u32)> = card
            .visible()
            .into_iter()
            .filter_map(|i| {
                self.texture(i).map(|(tid, frames, _, size)| {
                    (i, (tid, egui::vec2(size.x / frames as f32, size.y), frames))
                })
            })
            .collect();
        let action = card.show(ctx, &self.rows, &textures);
        self.propose = Some(card);
        match action {
            crate::propose_card::Action::Close => self.propose = None,
            crate::propose_card::Action::None => {}
            crate::propose_card::Action::Open(i) => self.open_external(i, ctx),
            crate::propose_card::Action::Copy(i) => self.copy(i, false, ctx),
            crate::propose_card::Action::Edit(i) => {
                self.editing = Some((
                    i,
                    crate::editor::EditorUi::new(
                        self.cfg.clone(),
                        self.rows[i].path.clone(),
                        ctx.clone(),
                    ),
                ));
            }
            crate::propose_card::Action::Apply {
                tag,
                accept,
                reject,
            } => {
                // the rejections first, so a batch that never runs still leaves the ranking sharper
                if !reject.is_empty() {
                    if let Err(e) = Db::open_cfg(&self.cfg)
                        .and_then(|db| crate::propose::reject(&db, &tag, &reject))
                    {
                        self.toast = Some((format!("Rejections not saved: {e}"), Instant::now()));
                    }
                }
                self.propose = None;
                if !accept.is_empty() && self.batch.is_none() {
                    self.pending_rename = None;
                    self.batch = Some(
                        crate::batch::BatchUi::new(self.cfg.clone(), accept, ctx.clone())
                            .plan([tag].into_iter().collect(), Default::default()),
                    );
                }
            }
        }
        if !self.pending.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
    fn show_dedup(&mut self, ctx: &egui::Context) {
        let Some(mut card) = self.dedup.take() else {
            return;
        };
        let textures: HashMap<usize, (egui::TextureId, egui::Vec2, u32)> = card
            .visible()
            .into_iter()
            .filter_map(|i| {
                self.texture(i).map(|(tid, frames, _, size)| {
                    (i, (tid, egui::vec2(size.x / frames as f32, size.y), frames))
                })
            })
            .collect();
        let action = card.show(ctx, &self.rows, &self.selected, &textures);
        // back in place before the action runs: "Show in grid" refilters, and the regrouping only happens for a
        // card that is there to see it (review, 2026-09-22: it kept the previous search's groups)
        self.dedup = Some(card);
        match action {
            crate::dedup::Action::Close => {
                self.dedup = None;
                return;
            }
            crate::dedup::Action::None => {}
            crate::dedup::Action::Toggle(i) => {
                if !self.selected.insert(i) {
                    self.selected.remove(&i);
                }
            }
            crate::dedup::Action::Select(v) => self.selected.extend(v),
            crate::dedup::Action::Open(i) => self.open_external(i, ctx),
            crate::dedup::Action::Search(p) => self.search_similar_to(&p),
        }
        if !self.pending.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(16));
        }
    }
    /// The tile's texture if it is ready; otherwise queue one decode for it and return None (the cell draws empty).
    fn texture(&mut self, i: usize) -> Option<(egui::TextureId, u32, f64, egui::Vec2)> {
        let now = self.frame_no;
        let id = self.rows[i].id.clone();
        if let Some(slot) = self.tex.get_mut(&id) {
            return slot.as_mut().map(|t| {
                t.last_used = now;
                (t.handle.id(), t.frames, t.rate, t.size)
            });
        }
        if !self.pending.contains(&id) {
            if let Some(tx) = &self.req_tx {
                if tx.send(self.rows[i].clone()).is_ok() {
                    self.pending.insert(id);
                }
            }
        }
        None
    }
    /// Drop the least-recently-drawn textures until under budget (never the ones drawn this frame).
    fn evict(&mut self) {
        let budget = self.budget();
        if self.tex_bytes <= budget {
            return;
        }
        let now = self.frame_no;
        let mut victims: Vec<(u64, String, usize)> = self
            .tex
            .iter()
            .filter_map(|(k, v)| {
                v.as_ref()
                    .filter(|t| t.last_used + EVICT_AGE < now)
                    .map(|t| (t.last_used, k.clone(), t.bytes))
            })
            .collect();
        victims.sort();
        for (_, k, b) in victims {
            if self.tex_bytes <= budget {
                break;
            }
            self.tex.remove(&k);
            self.tex_bytes -= b;
        } // TextureHandle drop frees the GPU copy too
    }
    /// Clipboard work (read the original over the mount, maybe re-encode, hand to xclip) runs on its own thread;
    /// the result comes back as `Msg::Copied`, and copy-and-close closes the window only after a successful copy.
    fn copy(&mut self, i: usize, close: bool, ctx: &egui::Context) {
        if self.copying {
            return;
        }
        let target = match self.cfg.file_path(&self.rows[i].path) {
            Ok(p) => p,
            Err(e) => {
                self.toast = Some((e, Instant::now()));
                return;
            }
        };
        let (tx, ctx) = (self.msg_tx.clone(), ctx.clone());
        self.copying = true;
        self.toast = Some(("Copying…".into(), Instant::now()));
        std::thread::spawn(move || {
            let (ok, message) = match grab::clip_path(&target) {
                Ok((n, m)) => (true, format!("Copied {n} as {m}")),
                Err(e) => (false, format!("Copy failed: {e}")),
            };
            let _ = tx.send(Msg::Copied { ok, close, message });
            ctx.request_repaint();
        });
    }
}

impl eframe::App for App {
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if let Some((w, h, zoom)) = self.win_seen {
            let path = self.cfg.db.with_file_name("window.toml");
            let _ = std::fs::write(
                &path,
                format!("# written by memetag on exit; the next grab opens like this\nwidth = {w:.0}\nheight = {h:.0}\nzoom = {zoom}\n"),
            );
        }
    }
    fn ui(&mut self, ui: &mut egui::Ui, _f: &mut eframe::Frame) {
        ui.add_enabled_ui(self.library_menu.is_none(), |ui| self.frame(ui));
        if let Some(menu) = &mut self.library_menu {
            match menu.show(ui.ctx()) {
                crate::library_ui::Action::None => {}
                crate::library_ui::Action::Close => self.library_menu = None,
                crate::library_ui::Action::Saved => {
                    self.library_menu = None;
                    self.cfg = crate::cfg();
                    self.tex.clear();
                    self.tex_bytes = 0;
                    self.pending.clear();
                    self.dedup_gen += 1;
                    self.start_decode_workers(ui.ctx().clone());
                    let c = self.cfg.clone();
                    let tx = self.msg_tx.clone();
                    let ctx = ui.ctx().clone();
                    std::thread::spawn(move || {
                        let result = Db::open_cfg(&c).and_then(|db| db.all());
                        let _ = tx.send(Msg::LibraryRows(result));
                        ctx.request_repaint();
                    });
                }
            }
        }

        {
            let ctx = ui.ctx();
            let zoom = ctx.zoom_factor();
            if let Some(size) = ctx.input(|i| i.viewport().inner_rect.map(|r| r.size())) {
                if size.x > 0.0 && size.y > 0.0 {
                    self.win_seen = Some((size.x * zoom, size.y * zoom, zoom));
                }
            }
        }
        // Ctrl+C in the search field, the editor or a selected label: take the text off egui's hands and serve it
        // from our own clipboard owner, which outlives the window (eframe's would vanish with the process).
        let ctx = ui.ctx().clone();
        let mut copied = None;
        ctx.output_mut(|o| {
            o.commands.retain(|c| match c {
                egui::OutputCommand::CopyText(t) => {
                    copied = Some(t.clone());
                    false
                }
                _ => true,
            })
        });
        if let Some(t) = copied {
            self.copy_text(t, &ctx);
        }
    }
}

impl App {
    fn frame(&mut self, ui: &mut egui::Ui) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        self.frame_no += 1;
        self.pump(ctx);
        while let Ok(m) = self.msg_rx.try_recv() {
            match m {
                Msg::LibraryRows(result) => match result {
                    Ok(rows) => {
                        self.rows = rows;
                        self.selected.clear();
                        self.editing = None;
                        self.batch = None;
                        self.dedup = None;
                        self.propose = None;
                        self.similar = None;
                        self.search_complete.refresh(&self.rows);
                        self.refilter();
                        self.toast = Some(("Folder selection saved".into(), Instant::now()));
                    }
                    Err(e) => self.toast = Some((e, Instant::now())),
                },
                Msg::Toast(s) => self.toast = Some((s, Instant::now())),
                Msg::Copied { ok, close, message } => {
                    self.copying = false;
                    self.toast = Some((message, Instant::now()));
                    if ok && close {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                    }
                }
                Msg::OcrDone(set) => {
                    self.ocr_done = set;
                    self.recount_ocr();
                }
                Msg::Ran { cmd, report } => {
                    self.ocr_runs.insert(cmd, Some(report));
                    if cmd != SERVICE_STATE {
                        self.run_ocr_cmd(SERVICE_STATE, crate::ocr::Run::Quiet, ctx);
                    }
                }
                // the editor and the batch view hold row indices: let the outcome wait in the channel until they are closed
                Msg::Pulled(mut o) => {
                    self.pulling = false;
                    if self.editing.is_some() || self.batch.is_some() {
                        let _ = self.msg_tx.send(Msg::Pulled(o));
                        break;
                    }
                    let conflict = o.vocab_conflict.take();
                    self.apply_pull(o);
                    if let Some(cf) = conflict {
                        self.raise_vocab_conflict(cf);
                    }
                }
                Msg::VocabConflict(cf) => self.raise_vocab_conflict(cf),
                Msg::VocabRefresh => {
                    if self.editing.is_some() || self.batch.is_some() {
                        let _ = self.msg_tx.send(Msg::VocabRefresh);
                        break;
                    }
                    self.refresh_vocab();
                }
                Msg::VocabResolved(r) => {
                    if self.editing.is_some() || self.batch.is_some() {
                        let _ = self.msg_tx.send(Msg::VocabResolved(r));
                        break;
                    }
                    self.vocab_conflict = None;
                    match r {
                        Ok(res) => match res.fold {
                            Some(_) => {
                                self.refresh_vocab();
                            }
                            None => self.toast = Some((res.message, Instant::now())),
                        },
                        Err(e) => self.toast = Some((format!("vocab: {e}"), Instant::now())),
                    }
                }
                Msg::Hashed(r) => {
                    self.hashing = false;
                    match r {
                        Ok(m) => {
                            self.similar = Some(crate::similar::Lookup::new(m));
                            self.refilter();
                            if self.dedup.as_ref().is_some_and(|d| d.waiting()) {
                                self.compute_dupes();
                            }
                        }
                        Err(e) => self.toast = Some((format!("Image search: {e}"), Instant::now())),
                    }
                }
                Msg::Grouped { gen, groups, scope } => {
                    if gen == self.dedup_gen {
                        if let Some(d) = &mut self.dedup {
                            d.set(groups, scope, &self.rows);
                        }
                    }
                }
            }
        }
        if let Some(batch) = &mut self.batch {
            match batch.show(ui, &self.cfg) {
                crate::batch::Action::None => {}
                crate::batch::Action::Close {
                    rows,
                    succeeded,
                    add,
                    remove,
                } => {
                    self.selected
                        .retain(|i| !succeeded.contains(&self.rows[*i].path));
                    if let Some(rows) = rows {
                        let mut updated: HashMap<_, _> =
                            rows.into_iter().map(|r| (r.path.clone(), r)).collect();
                        for row in &mut self.rows {
                            if let Some(new) = updated.remove(&row.path) {
                                *row = new;
                            }
                        }
                    }
                    self.batch = None;
                    self.search_complete.refresh(&self.rows);
                    self.refilter();
                    if let Some((from, to, planned)) = self.pending_rename.take() {
                        let as_planned = add.len() == 1
                            && add.contains(&to)
                            && remove.len() == 1
                            && remove.contains(&from);
                        if !as_planned {
                            // the plan is editable in the batch panel; whatever it became, it is no longer a rename
                            if !succeeded.is_empty() {
                                self.toast = Some((
                                    format!(
                                        "The plan was changed, so rules still mention \"{from}\""
                                    ),
                                    Instant::now(),
                                ));
                            }
                        } else if succeeded.len() == planned {
                            self.finish_rename(ctx, &from, &to);
                        } else if !succeeded.is_empty() {
                            self.toast = Some((
                                format!(
                                    "Some files still carry \"{from}\"; the rules follow once a rename reaches them all"
                                ),
                                Instant::now(),
                            ));
                        }
                    }
                    if let Some(t) = &mut self.tags_card {
                        t.reload(&self.cfg);
                    }
                }
            }
            return;
        }
        if let Some(index) = self.editing.as_ref().map(|(i, _)| *i) {
            let thumb = self
                .texture(index)
                .map(|(id, frames, _, size)| (id, size, frames));
            let action = self.editing.as_mut().unwrap().1.show(ui, &self.cfg, thumb);
            match action {
                crate::editor::Action::None => {}
                crate::editor::Action::Cancel => self.editing = None,
                crate::editor::Action::OpenOriginal => self.open_external(index, ctx),
                crate::editor::Action::Saved(row, next) => {
                    let slot = self.hits.iter().position(|&h| h == index);
                    self.rows[index] = row;
                    self.editing = None;
                    self.search_complete.refresh(&self.rows);
                    self.refilter();
                    if let Some(p) = &mut self.propose {
                        p.remap(&self.rows); // a file that now carries the tag leaves the proposals
                    }
                    self.toast = Some(("Saved tags and text".into(), Instant::now()));
                    if next {
                        self.open_next(index, slot, ctx);
                    }
                }
                crate::editor::Action::Next => {
                    let slot = self.hits.iter().position(|&h| h == index);
                    self.editing = None;
                    self.open_next(index, slot, ctx);
                }
            }
            return;
        }
        // Consume plain navigation keys before the always-focused search field.
        // Modified Home/End remain available for editing/selecting query text.
        let navigation = ctx.input_mut(|input| {
            if input.modifiers != egui::Modifiers::NONE {
                return None;
            }
            [
                egui::Key::PageUp,
                egui::Key::PageDown,
                egui::Key::Home,
                egui::Key::End,
            ]
            .into_iter()
            .find(|key| input.consume_key(egui::Modifiers::NONE, *key))
        });
        let mut copy_first = false;
        egui::Panel::top("query").show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("memetag").strong());
                // Right-to-left so the help button keeps its spot at the edge and the search field takes the rest.
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.add_enabled(self.editing.is_none() && self.batch.is_none() && !self.pulling, egui::Button::new("Folders")).clicked() {
                        match crate::library_ui::LibraryUi::new(&self.cfg) {
                            Ok(menu) => self.library_menu = Some(menu),
                            Err(e) => self.toast = Some((e, Instant::now())),
                        }
                    }
                    if ui.selectable_label(self.help_open,
 "?").on_hover_text("Search syntax").clicked() { self.help_open = !self.help_open; }
                    if ui.selectable_label(self.ocr_help_open, "OCR").on_hover_text("How to run the text-reading pass").clicked() { self.toggle_ocr_help(ctx); }
                    if ui.button("📷").on_hover_text("Search for the image on the clipboard (similar:clipboard)").clicked() { self.search_clipboard(); }
                    let (changed, copy) = self.search_complete.show(ui, &self.cfg, &mut self.q, &mut self.first);
                    if changed { self.refilter(); } copy_first = copy;
                });
            });
            ui.horizontal(|ui| {
                match &self.err {
                    Some(e) => { ui.colored_label(widgets::ERROR, format!("query: {e}")); }
                    None => {
                        ui.label(format!("{} of {} files", self.hits.len(), self.rows.len()));
                        if self.ocr_pending > 0 { ui.label("·"); if ui.link(format!("{} awaiting OCR", self.ocr_pending)).on_hover_text("How to run the text-reading pass").clicked() { self.toggle_ocr_help(ctx); } }
                        ui.label(format!("· previews up to {} frames, native speed capped at {} fps · textures {} MB of {}", self.cfg.strip_frames, self.cfg.preview_fps, self.tex_bytes / (1024 * 1024), self.cfg.texture_budget_mb));
                    }
                }
                if let Some((msg, t)) = &self.toast { if t.elapsed() < Duration::from_secs(3) { ui.separator(); ui.colored_label(widgets::OK, msg); } }
            });
            ui.horizontal(|ui| {
                ui.toggle_value(&mut self.select_mode, "Select").on_hover_text("Select mode: left-click toggles selection. Shift+middle-click works in either mode.");
                let hidden = self.selected.len() - self.hits.iter().filter(|i| self.selected.contains(i)).count();
                ui.label(if hidden == 0 { format!("{} selected", self.selected.len()) } else { format!("{} selected ({hidden} hidden by search)", self.selected.len()) });
                if ui.add_enabled(!self.selected.is_empty(), egui::Button::new("Clear")).clicked() { self.selected.clear(); }
                if ui.add_enabled(!self.selected.is_empty(), egui::Button::new("Edit tags")).clicked() {
                    let mut indices: Vec<_> = self.selected.iter().copied().collect(); indices.sort_unstable();
                    self.pending_rename = None; // a plan of the user's own, not a rename from the Tags card
                    self.batch = Some(crate::batch::BatchUi::new(self.cfg.clone(), indices.into_iter().map(|i| self.rows[i].path.clone()).collect(), ctx.clone()));
                }
                if ui.add_enabled(!self.selected.is_empty() && self.propose.is_none(), egui::Button::new("Propose")).on_hover_text("Files without a tag these share, ranked by likeness to them; accept the ones that fit").clicked() {
                    let mut indices: Vec<_> = self.selected.iter().copied().collect(); indices.sort_unstable();
                    self.propose = Some(crate::propose_card::ProposeUi::new(self.cfg.clone(), indices.into_iter().map(|i| self.rows[i].path.clone()).collect(), ctx.clone()));
                }
                if self.select_mode || !self.selected.is_empty() {
                    if ui.add_enabled(self.err.is_none() && !self.hits.is_empty(), egui::Button::new(format!("Select all {} search results", self.hits.len()))).clicked() { self.selected.extend(self.hits.iter().copied()); }
                } else { ui.small("Shift+middle-click to select"); }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.selectable_label(self.implications.is_some(), "Implications").on_hover_text("Rules like \"twitter post ⇒ twitter\": a general tag every file with a specific one gets for free").clicked() {
                        match self.implications.take() { Some(mut im) => { im.request_close(); self.implications = Some(im); } None => self.implications = Some(crate::implications::ImplicationsUi::new(&self.cfg, &self.rows)) }
                    }
                    if ui.selectable_label(self.dedup.is_some(), "Duplicates").on_hover_text("The search results grouped by look-alikes, side by side: copies, other sizes, captioned versions").clicked() {
                        match self.dedup.take() { Some(mut d) => { d.request_close(); self.dedup = Some(d); } None => { self.dedup = Some(crate::dedup::DedupUi::new()); self.compute_dupes(); } }
                    }
                    if ui
                        .selectable_label(self.tags_card.is_some(), "Tags")
                        .on_hover_text("Every tag you wrote, with counts; rename, merge or delete one across all its files")
                        .clicked()
                    {
                        match self.tags_card.take() {
                            Some(mut t) => {
                                t.request_close();
                                self.tags_card = Some(t);
                            }
                            None => self.tags_card = Some(crate::tags_card::TagsUi::new(&self.cfg)),
                        }
                    }
                    ui.toggle_value(&mut self.assist, "Tagging assistant").on_hover_text("How much of the collection carries tags you wrote. Untagged tiles get an orange outline, thinly tagged ones (fewer than 3 tags) a yellow one, tagged ones dim. Folder and implied tags do not count.");
                });
            });
            if self.assist {
                ui.horizontal(|ui| {
                    let pct = |n: usize, of: usize| if of == 0 { 0.0 } else { 100.0 * n as f64 / of as f64 };
                    ui.label(format!("Tagged by hand: {} of {} files ({:.1}%, {:.1}% thinly)", self.tagged_all, self.rows.len(), pct(self.tagged_all, self.rows.len()), pct(self.thin_all, self.rows.len())))
                        .on_hover_text(format!("Thinly: hand-written tags, but fewer than {THIN_BELOW}"));
                    if self.err.is_none() && self.hits.len() != self.rows.len() {
                        ui.label("·"); ui.label(format!("in these results: {} of {} ({:.1}%)", self.tagged_hits, self.hits.len(), pct(self.tagged_hits, self.hits.len())));
                    }
                    let untagged = self.hits.len() - self.tagged_hits;
                    if self.err.is_none() && untagged > 0 && untagged != self.hits.len() {
                        ui.label("·");
                        if ui
                            .link(format!("show only the {untagged} untagged"))
                            .on_hover_text("Adds tag_count:0 to the search")
                            .clicked()
                        {
                            self.show_untagged();
                        }
                    }
                    let thin = self.thin_hits;
                    if self.err.is_none() && thin > 0 && thin != self.hits.len() {
                        ui.label("·");
                        if ui
                            .link(format!("show only the {thin} thinly tagged"))
                            .on_hover_text(format!(
                                "Files with hand-written tags but fewer than {THIN_BELOW}; adds tag_count.gt:0, tag_count.lt:{THIN_BELOW} to the search"
                            ))
                            .clicked()
                        {
                            self.show_thin();
                        }
                    }
                });
            }
        });
        if copy_first {
            if let Some(&i) = self.hits.first() {
                self.copy(i, false, ctx);
            }
        }
        self.show_help(ctx);
        self.show_ocr_help(ctx);
        self.show_implications(ctx);
        self.show_vocab_conflict(ctx);
        self.show_tags_card(ctx);
        self.show_dedup(ctx);
        self.show_propose(ctx);
        self.escape(ctx); // after the cards drew, so a card's own text field gets first refusal of the key
        let t = ctx.input(|i| i.time);
        let mut any_video = false;
        let mut clicked: Option<(usize, bool)> = None;
        let mut preview = None;
        let mut edit = None;
        let mut similar_to = None;
        let mut next_wake = f64::INFINITY;
        egui::CentralPanel::default().show(ui, |ui| {
            let page_height = (ui.available_height() - 32.0).max(TILE);
            // Own id: the editor and batch views draw a scroll area in the same panel slot, and an unnamed
            // one shares its persistent state with theirs — the editor's short page clamped the grid back to the top.
            egui::ScrollArea::vertical().id_salt("grid").show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                    let hits = self.hits.clone();
                    for i in hits {
                        // fixed cell first; only tiles actually on screen are requested; decoding happens on the worker pool
                        let (cell, resp) = ui.allocate_exact_size(egui::vec2(TILE, TILE), egui::Sense::click());
                        if !ui.is_rect_visible(cell) { continue; }
                        let shift = ui.input(|i| i.modifiers.shift);
                        let selection_hit = (resp.clicked_by(egui::PointerButton::Middle) && shift) || (self.select_mode && resp.clicked() && !shift);
                        if selection_hit && !self.selected.insert(i) { self.selected.remove(&i); }
                        let edit_rect = egui::Rect::from_min_size(cell.left_top() + egui::vec2(4.0, 4.0), egui::vec2(42.0, 24.0));
                        let mut edit_hit = resp.clicked() && ui.input(|i| i.modifiers.shift);
                        if edit_hit { edit = Some(i); }
                        if resp.secondary_clicked() { preview = Some(i); }
                        let Some((tid, frames, rate, tex_size)) = self.texture(i) else {
                            ui.painter().rect_filled(cell, 4.0, ui.visuals().widgets.inactive.bg_fill);
                            if self.tex.get(&self.rows[i].id).map(|t| t.is_none()).unwrap_or(false) { paint_undecodable(ui, cell, &self.rows[i]); } // decode failed: say what it is instead of a blank tile
                            if self.awaits_ocr(i) { paint_ocr_pending(ui, cell); }
                            if self.assist { paint_assist(ui, cell, tier(&self.rows[i])); }
                            if self.selected.contains(&i) { paint_selection(ui, cell); } continue };
                        let row = &self.rows[i];
                        let frame_size = egui::vec2(tex_size.x / frames as f32, tex_size.y);
                        let scale = (TILE / frame_size.x).min(TILE / frame_size.y).min(4.0);
                        let draw = egui::Rect::from_center_size(cell.center(), frame_size * scale);
                        let mut uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                        if frames > 1 { any_video = true; next_wake = next_wake.min(((t * rate).floor() + 1.0) / rate - t); let k = ((t * rate) as u32) % frames; uv = egui::Rect::from_min_max(egui::pos2(k as f32 / frames as f32, 0.0), egui::pos2((k + 1) as f32 / frames as f32, 1.0)); }
                        let bg = if resp.hovered() { ui.visuals().widgets.hovered.bg_fill } else { ui.visuals().widgets.inactive.bg_fill };
                        ui.painter().rect_filled(cell, 4.0, bg);
                        egui::Image::from_texture(egui::load::SizedTexture::new(tid, draw.size())).uv(uv).paint_at(ui, draw);
                        if self.assist { paint_assist(ui, cell, tier(row)); } // before the Edit button, so the dimming never covers it
                        // Hover controls must not advance the wrapped grid's layout cursor.
                        if ui.rect_contains_pointer(cell) && ui.place(edit_rect, egui::Button::new("Edit")).clicked() { edit_hit = true; edit = Some(i); }
                        let similar_rect = egui::Rect::from_min_size(edit_rect.right_top() + egui::vec2(4.0, 0.0), egui::vec2(58.0, 24.0));
                        if row.kind == "image" && ui.rect_contains_pointer(cell) && ui.place(similar_rect, egui::Button::new("Similar")).on_hover_text("Search for files that look like this one (copies, other sizes, captioned versions)").clicked() { edit_hit = true; similar_to = Some(i); }
                        if resp.hovered() { ui.painter().rect_stroke(cell, 4.0, ui.visuals().widgets.hovered.fg_stroke, egui::StrokeKind::Inside); }
                        if self.selected.contains(&i) { paint_selection(ui, cell); }
                        if frames > 1 { ui.painter().text(cell.right_bottom() - egui::vec2(6.0, 4.0), egui::Align2::RIGHT_BOTTOM, if row.kind == "video" { "▶" } else { "GIF" }, egui::FontId::proportional(if row.kind == "video" { 14.0 } else { 11.0 }), egui::Color32::WHITE); }
                        if self.awaits_ocr(i) { paint_ocr_pending(ui, cell); }
                        let r = resp.on_hover_ui(|ui| {
                            ui.set_max_width(440.0);
                            ui.label(egui::RichText::new(&row.path).strong());
                            ui.label(format!("{} × {} · {}", row.width, row.height, row.format));
                            ui.label(format!("Downloaded {}", crate::index::fmt_time(row.mtime)));
                            ui.separator();
                            ui.label("Right-click to open original in feh/mpv");
                            ui.label(egui::RichText::new("Tags").strong());
                            if row.tags.is_empty() { ui.label("No tags yet"); }
                            else { ui.label(row.tags.iter().map(String::as_str).collect::<Vec<_>>().join(" · ")); }
                            match (self.assist, tier(row)) {
                                (true, Tier::Untagged) => { ui.colored_label(widgets::UNTAGGED, "No tags written by hand yet"); }
                                (true, Tier::Thin) => { ui.colored_label(widgets::THIN, format!("Only {} written by hand", plural(row.xmp_tag_count, "tag"))); }
                                _ => {}
                            }
                            ui.label(egui::RichText::new("OCR text").strong());
                            if row.text.is_empty() { ui.label("No OCR text available"); }
                            else {
                                let mut caption: String = row.text.chars().take(1200).collect();
                                if caption.len() < row.text.len() { caption.push('…'); }
                                ui.label(caption);
                            }
                            if row.kind == "image" && !self.ocr_done.contains(&row.path) { ui.label(format!("Awaiting OCR by {}", self.ocr_engine)); }
                            ui.separator();
                            ui.label(if row.kind == "video" { "Copies a file link" }
                                     else if row.format == "gif" { "Copies the whole GIF with animation" }
                                     else if row.format == "webp" { "Copies a PNG still" }
                                     else { "Copies the full-size image" });
                            ui.label(if self.select_mode { "Left-click: toggle selection · Middle-click: copy & stay" } else { "Left-click: copy & close · Middle-click: copy & stay" });
                            ui.label("Shift+left-click: edit tags and OCR text");
                            ui.label("Shift+middle-click: toggle selection · Select mode: left-click toggles selection");
                        });
                        if r.clicked() && !edit_hit && !selection_hit { clicked = Some((i, true)); }
                        else if r.clicked_by(egui::PointerButton::Middle) && !selection_hit { clicked = Some((i, false)); }
                    }
                });
                if let Some(key) = navigation {
                    // Use real content bounds for Home/End: overshooting an estimated
                    // height would hit the boundary before the easing completes.
                    let animation = egui::style::ScrollAnimation::duration(0.25);
                    match key {
                        egui::Key::Home => ui.scroll_to_rect_animation(ui.min_rect(), Some(egui::Align::TOP), animation),
                        egui::Key::End => ui.scroll_to_rect_animation(ui.min_rect(), Some(egui::Align::BOTTOM), animation),
                        egui::Key::PageUp => ui.scroll_with_delta_animation(egui::vec2(0.0, page_height), animation),
                        egui::Key::PageDown => ui.scroll_with_delta_animation(egui::vec2(0.0, -page_height), animation),
                        _ => {},
                    }
                }
            });
        });
        if let Some(i) = similar_to {
            self.show_similar(i);
        } else if let Some(i) = edit {
            self.editing = Some((
                i,
                crate::editor::EditorUi::new(
                    self.cfg.clone(),
                    self.rows[i].path.clone(),
                    ctx.clone(),
                ),
            ));
        } else if let Some(i) = preview {
            self.open_external(i, ctx);
        } else if let Some((i, close)) = clicked {
            self.copy(i, close, ctx);
        }
        if any_video {
            ctx.request_repaint_after(Duration::from_secs_f64(next_wake.clamp(0.004, 1.0)));
        }
        // wake at the earliest tile's next frame boundary
        else if self
            .toast
            .as_ref()
            .map(|(_, t)| t.elapsed() < Duration::from_secs(3))
            .unwrap_or(false)
        {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }
}

/// Small "OCR" pill in the tile's bottom-left corner: this image has not been read by the configured engine yet.
fn paint_ocr_pending(ui: &egui::Ui, cell: egui::Rect) {
    let pill = egui::Rect::from_min_size(
        cell.left_bottom() + egui::vec2(5.0, -21.0),
        egui::vec2(34.0, 16.0),
    );
    ui.painter().rect_filled(
        pill,
        3.0,
        egui::Color32::from_rgba_unmultiplied(0, 0, 0, 150),
    );
    ui.painter().text(
        pill.center(),
        egui::Align2::CENTER_CENTER,
        "OCR",
        egui::FontId::proportional(10.0),
        widgets::OCR_PENDING,
    );
}
fn hand_tagged(r: &FileRow) -> bool {
    r.xmp_tag_count > 0
}
/// Fewer hand-written tags than this and the Tagging assistant still counts the file as work to do.
const THIN_BELOW: i64 = 3;
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tier {
    Untagged,
    Thin,
    Tagged,
}
fn tier(r: &FileRow) -> Tier {
    match r.xmp_tag_count {
        0 => Tier::Untagged,
        n if n < THIN_BELOW => Tier::Thin,
        _ => Tier::Tagged,
    }
}
fn plural(n: i64, word: &str) -> String {
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {word}s")
    }
}
/// Tagging assistant marks: an untagged tile gets an orange outline, a thinly tagged one a yellow one; a tagged
/// one is dimmed so the work left stands out.
fn paint_assist(ui: &egui::Ui, cell: egui::Rect, tier: Tier) {
    let outline = |color| {
        ui.painter().rect_stroke(
            cell.shrink(1.0),
            4.0,
            egui::Stroke::new(2.0, color),
            egui::StrokeKind::Inside,
        );
    };
    match tier {
        Tier::Tagged => {
            ui.painter()
                .rect_filled(cell, 4.0, egui::Color32::from_black_alpha(140));
        }
        Tier::Untagged => outline(widgets::UNTAGGED),
        Tier::Thin => outline(widgets::THIN),
    }
}
/// A tile whose file could not be decoded (zip, pdf, xcf, a broken image): the format and the file name, instead of nothing.
fn paint_undecodable(ui: &egui::Ui, cell: egui::Rect, row: &FileRow) {
    let weak = ui.visuals().weak_text_color();
    ui.painter().text(
        cell.center() - egui::vec2(0.0, 8.0),
        egui::Align2::CENTER_CENTER,
        row.format.to_uppercase(),
        egui::FontId::proportional(22.0),
        weak,
    );
    let name = std::path::Path::new(&row.path)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let name: String = if name.chars().count() > 30 {
        name.chars().take(28).collect::<String>() + "…"
    } else {
        name
    };
    ui.painter().text(
        cell.center() + egui::vec2(0.0, 14.0),
        egui::Align2::CENTER_CENTER,
        name,
        egui::FontId::proportional(10.0),
        weak,
    );
}

fn paint_selection(ui: &egui::Ui, cell: egui::Rect) {
    let color = widgets::SELECTED;
    ui.painter().rect_stroke(
        cell.shrink(1.0),
        4.0,
        egui::Stroke::new(3.0, color),
        egui::StrokeKind::Inside,
    );
    let top = cell.right_top() + egui::vec2(-28.0, 6.0);
    ui.painter().rect_filled(
        egui::Rect::from_min_size(top, egui::vec2(22.0, 22.0)),
        3.0,
        color,
    );
    ui.painter().line_segment(
        [top + egui::vec2(4.0, 11.0), top + egui::vec2(9.0, 16.0)],
        egui::Stroke::new(2.5, egui::Color32::BLACK),
    );
    ui.painter().line_segment(
        [top + egui::vec2(9.0, 16.0), top + egui::vec2(18.0, 5.0)],
        egui::Stroke::new(2.5, egui::Color32::BLACK),
    );
}

pub fn run(c: &Cfg, q: &str) -> Result<(), String> {
    let db = Db::open_cfg(&c)?;
    let rows = db.all()?;
    if let Err(e) = grab::migrate_cache(c, &rows) {
        eprintln!("thumbnail cache migration: {e}");
    }
    let search_complete = crate::search_complete::SearchComplete::new(c, &rows);
    let (msg_tx, msg_rx) = mpsc::channel();
    let ocr_engine = crate::ocr::current_label(c);
    let ocr_done = ocr_done_set(&c.db, &ocr_engine).unwrap_or_default();
    let mut app = App::new(
        c,
        rows,
        q,
        msg_tx,
        msg_rx,
        ocr_done,
        ocr_engine,
        search_complete,
    );
    app.refilter();
    let title = format!(
        "memetag · {} frames, ≤{} fps",
        c.strip_frames, c.preview_fps
    );
    let (size, zoom) = window_prefs(&c.db.with_file_name("window.toml"));
    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(size)
            .with_title(title),
        ..Default::default()
    };
    eframe::run_native(
        "memetag",
        opts,
        Box::new(move |cc| {
            if zoom != 1.0 {
                cc.egui_ctx.set_zoom_factor(zoom);
            }
            install_cjk_fallback(&cc.egui_ctx);
            app.start_workers(cc.egui_ctx.clone());
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| format!("gui: {e}"))
}

/// The last window size (points at zoom 1) and zoom factor, from `window.toml` beside the index; the defaults
/// when there is none or it does not parse. Sizes are clamped to something a monitor can show.
fn window_prefs(path: &Path) -> ([f32; 2], f32) {
    let default = ([1100.0, 760.0], 1.0);
    let Ok(text) = std::fs::read_to_string(path) else {
        return default;
    };
    let Ok(t) = text.parse::<toml::Table>() else {
        return default;
    };
    let num = |k: &str| {
        t.get(k)
            .and_then(|v| v.as_float().or(v.as_integer().map(|i| i as f64)))
            .map(|f| f as f32)
    };
    match (num("width"), num("height"), num("zoom")) {
        (Some(w), Some(h), z)
            if (200.0..=16000.0).contains(&w) && (150.0..=16000.0).contains(&h) =>
        {
            (
                [w, h],
                z.filter(|z| (0.25..=4.0).contains(z)).unwrap_or(1.0),
            )
        }
        _ => default,
    }
}

/// CJK fallback font (egui's bundled fonts do not cover every OCR language).
/// One face of Noto Sans CJK covers Japanese, Chinese and Korean; it is read from the system at startup (19 MB,
/// never embedded) and appended to both font families, so every label, tooltip and text box falls back to it.
pub(crate) fn install_cjk_fallback(ctx: &egui::Context) {
    let fixed = [
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Medium.ttc",
        "/usr/share/fonts/wenquanyi/wqy-zenhei/wqy-zenhei.ttc",
    ];
    let fc = || {
        std::process::Command::new("fc-match")
            .args(["-f", "%{file}", ":lang=ja"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim().to_string())
            })
    };
    let Some(path) = fixed
        .iter()
        .map(std::path::PathBuf::from)
        .chain(fc())
        .find(|p| p.is_file())
    else {
        eprintln!(
            "no CJK font found (paru -S noto-fonts-cjk): Japanese OCR text will show as boxes"
        );
        return;
    };
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("CJK font {}: {e}", path.display());
            return;
        }
    };
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert(
        "cjk".into(),
        std::sync::Arc::new(egui::FontData::from_owned(bytes)),
    ); // face 0 of the collection: Noto Sans CJK JP
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts.families.entry(family).or_default().push("cjk".into());
    }
    ctx.set_fonts(fonts);
}

#[cfg(test)]
mod vocab_refresh_tests {
    use super::*;

    #[test]
    fn rules_only_pull_refreshes_search_aliases_and_existing_rows() {
        let dir = std::env::temp_dir().join(format!("memetag-grid-vocab-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = crate::cfg();
        c.root = dir.clone();
        c.sources = None;
        c.active_source = None;
        c.db = dir.join("index.sqlite");
        c.vocab = crate::vocab::Vocab::load(&dir.join("vocab.toml"));
        let path = dir.join("cat.png");
        image::RgbImage::from_pixel(4, 4, image::Rgb([100, 50, 20]))
            .save(&path)
            .unwrap();
        let db = Db::open_cfg(&c).unwrap();
        db.upsert_file(&c.root, &path, &c.vocab).unwrap();
        // A real row, with a stored tag; avoid any live journal or server.
        db.conn
            .execute(
                "INSERT INTO tags(path,tag,source) VALUES('cat.png','cat','xmp')",
                [],
            )
            .unwrap();
        let rows = db.all().unwrap();
        let completion = crate::search_complete::SearchComplete::new(&c, &rows);
        let (tx, rx) = mpsc::channel();
        let mut app = App::new(
            &c,
            rows,
            "animal",
            tx,
            rx,
            HashSet::new(),
            String::new(),
            completion,
        );
        app.refilter();
        assert!(app.hits.is_empty());
        app.selected.insert(0);
        let mut updated = c.vocab.clone();
        updated.add_rule("cat", "animal");
        updated.aliases.insert("kitty".into(), "cat".into());
        updated.save().unwrap();
        db.reimply(&updated).unwrap(); // worker already applied the changes; a second delta is empty
        app.apply_pull(crate::pull::Outcome {
            vocab_changed: true,
            ..Default::default()
        });
        assert_eq!(app.cfg.vocab.canon("kitty"), "cat");
        assert!(app.rows[0].tags.contains("animal"));
        assert_eq!(app.hits, vec![0]);
        assert!(app.selected.contains(&0));
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
mod scroll_memory {
    //! The grid's scroll offset must survive a trip through the editor view.
    use egui::*;
    fn frame(ctx: &Context, f: impl FnOnce(&mut Ui) -> f32) -> f32 {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(400.0, 300.0))),
            ..Default::default()
        };
        let mut out = 0.0;
        let mut f = Some(f);
        // Headless: nobody uploads the font atlas, so acknowledge the deltas or epaint asserts on drop.
        ctx.run_ui(input, |ui| {
            if let Some(f) = f.take() {
                out = f(ui);
            }
        })
        .textures_delta
        .clear();
        out
    }
    fn grid(ui: &mut Ui, salt: bool, force: Option<f32>) -> f32 {
        CentralPanel::default()
            .show(ui, |ui| {
                let mut sa = ScrollArea::vertical();
                if salt {
                    sa = sa.id_salt("grid");
                }
                if let Some(y) = force {
                    sa = sa.vertical_scroll_offset(y);
                }
                sa.show(ui, |ui| {
                    for i in 0..200 {
                        ui.label(format!("tile {i}"));
                    }
                })
                .state
                .offset
                .y
            })
            .inner
    }
    fn editor(ui: &mut Ui, salt: bool) {
        CentralPanel::default().show(ui, |ui| {
            let mut sa = ScrollArea::vertical();
            if salt {
                sa = sa.id_salt(("editor", "some/path.png"));
            }
            sa.show(ui, |ui| {
                ui.label("short editor page");
            });
        });
    }
    fn round_trip(salt: bool) -> (f32, f32) {
        let ctx = Context::default();
        frame(&ctx, |ui| grid(ui, salt, Some(1500.0)));
        let before = frame(&ctx, |ui| grid(ui, salt, None));
        for _ in 0..3 {
            frame(&ctx, |ui| {
                editor(ui, salt);
                0.0
            });
        }
        let after = frame(&ctx, |ui| grid(ui, salt, None));
        (before, after)
    }
    #[test]
    fn unsalted_scroll_areas_share_state_and_reset() {
        let (before, after) = round_trip(false);
        assert!(
            before > 1000.0,
            "grid should be scrolled down, got {before}"
        );
        assert!(
            after < 1.0,
            "the bug: an unnamed editor scroll area drags the grid back to the top, got {after}"
        );
    }
    #[test]
    fn salted_grid_scroll_survives_the_editor() {
        let (before, after) = round_trip(true);
        assert!(
            before > 1000.0,
            "grid should be scrolled down, got {before}"
        );
        assert!(
            (after - before).abs() < 1.0,
            "grid offset must come back unchanged: before {before}, after {after}"
        );
    }
}
