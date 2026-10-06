//! The Propose card on the select bar: the selection defines a tag it shares, the card ranks every file without that
//! tag by likeness (`propose::rank`, image vectors, OCR text, or both) and shows the top of the list as tiles.
//! Left-click cycles a tile: accept (green), reject (red), untouched. Accept hands the green ones to the batch panel
//! as a reviewed "+tag" plan, exactly as Edit tags would, and the red ones are remembered against the tag so the next
//! ranking pulls away from them. Middle-click copies the file as the grid does; Edit on a hovered tile opens the
//! editor. The rejected files of the tag can be shown and un-rejected. Nothing is written by this card itself but
//! those rejections and the mode it was last run with.
use crate::index::{Db, FileRow};
use crate::propose::{self, Mode, Proposal};
use crate::widgets;
use crate::Cfg;
use eframe::egui;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::mpsc;

pub enum Action {
    None,
    Close,
    Open(usize),
    /// The tile's Edit button: the editor for that file, as on a grid tile.
    Edit(usize),
    /// Middle-click: copy the file to the clipboard and stay, as on a grid tile.
    Copy(usize),
    /// The green tiles go to the batch panel with `+tag`; the red ones are recorded first.
    Apply {
        tag: String,
        accept: Vec<String>,
        reject: Vec<String>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mark {
    Accept,
    Reject,
}

const PAGE: usize = 50;
const TILE: f32 = 150.0;
/// How many proposals a ranking brings back; the card pages through them.
const KEEP: usize = 500;

struct Loaded {
    /// Tags written by hand on the selected files, most shared first.
    choices: Vec<(String, usize)>,
    /// The mode last used per tag.
    modes: HashMap<String, Mode>,
}

/// What a ranking brought back: the proposals, how many candidates were scored, and the tag's rejections at the time.
struct Ranked {
    proposals: Vec<Proposal>,
    scored: usize,
    rejected: Vec<String>,
}

type Texture = (egui::TextureId, egui::Vec2, u32);

pub struct ProposeUi {
    cfg: Cfg,
    positives: Vec<String>,
    loaded: Option<Loaded>,
    load_rx: mpsc::Receiver<Result<Loaded, String>>,
    tag: String,
    mode: Mode,
    rank_rx: Option<mpsc::Receiver<Result<Ranked, String>>>,
    ranked: Option<Ranked>,
    /// Row index per proposal, rebuilt by `remap`, which also drops a proposal whose file went or now carries the tag.
    rows: Vec<usize>,
    /// Row index per rejected file (a file that went is left out), rebuilt by `remap`.
    rejected_rows: Vec<usize>,
    show_rejected: bool,
    /// Something was un-rejected since the last ranking: the list on show does not include it yet.
    stale: bool,
    marks: HashMap<String, Mark>,
    page: usize,
    err: Option<String>,
    closing: bool,
}

impl ProposeUi {
    pub fn new(c: Cfg, positives: Vec<String>, ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        let (cfg, paths) = (c.clone(), positives.clone());
        std::thread::spawn(move || {
            let result = (|| -> Result<Loaded, String> {
                let db = Db::open_cfg(&cfg)?;
                let selected: HashSet<&str> = paths.iter().map(String::as_str).collect();
                let mut counts: BTreeMap<String, usize> = BTreeMap::new();
                let mut st = db
                    .conn
                    .prepare("SELECT path, tag FROM tags WHERE source='xmp'")
                    .map_err(|e| e.to_string())?;
                for row in st
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                    .map_err(|e| e.to_string())?
                {
                    let (path, tag) = row.map_err(|e| e.to_string())?;
                    if selected.contains(path.as_str()) {
                        *counts.entry(tag).or_insert(0) += 1;
                    }
                }
                let mut choices: Vec<(String, usize)> = counts.into_iter().collect();
                choices.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                let mut st = db
                    .conn
                    .prepare("SELECT tag, mode FROM proposal_modes")
                    .map_err(|e| e.to_string())?;
                let modes = st
                    .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                    .map_err(|e| e.to_string())?
                    .filter_map(Result::ok)
                    .filter_map(|(t, m)| Mode::parse(&m).map(|m| (t, m)))
                    .collect();
                Ok(Loaded { choices, modes })
            })();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        Self {
            cfg: c,
            positives,
            loaded: None,
            load_rx: rx,
            tag: String::new(),
            mode: Mode::Image,
            rank_rx: None,
            ranked: None,
            rows: vec![],
            rejected_rows: vec![],
            show_rejected: false,
            stale: false,
            marks: HashMap::new(),
            page: 0,
            err: None,
            closing: false,
        }
    }
    pub fn request_close(&mut self) {
        self.closing = true;
    }
    fn busy(&self) -> bool {
        self.rank_rx.is_some()
    }
    /// Row indices the card draws this frame (the current page, and the rejected list when it is open), so the grid
    /// can fetch just those textures.
    pub fn visible(&self) -> Vec<usize> {
        let mut v: Vec<usize> = self
            .rows
            .iter()
            .skip(self.page * PAGE)
            .take(PAGE)
            .copied()
            .collect();
        if self.show_rejected {
            v.extend(&self.rejected_rows);
        }
        v
    }
    pub fn remap(&mut self, rows: &[FileRow]) {
        let index: HashMap<&str, usize> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.path.as_str(), i))
            .collect();
        let tag = self.tag.clone();
        match &mut self.ranked {
            Some(r) => {
                r.proposals.retain(|p| {
                    index
                        .get(p.path.as_str())
                        .is_some_and(|&i| !rows[i].tags.contains(&tag))
                });
                self.rows = r.proposals.iter().map(|p| index[p.path.as_str()]).collect();
                r.rejected.retain(|p| index.contains_key(p.as_str()));
                self.rejected_rows = r.rejected.iter().map(|p| index[p.as_str()]).collect();
            }
            None => {
                self.rows.clear();
                self.rejected_rows.clear();
            }
        }
    }
    fn pick_tag(&mut self, tag: String) {
        if tag == self.tag {
            return;
        }
        self.tag = tag;
        if let Some(l) = &self.loaded {
            if let Some(m) = l.modes.get(&self.tag) {
                self.mode = *m;
            }
        }
        self.ranked = None;
        self.rows.clear();
        self.rejected_rows.clear();
        self.show_rejected = false;
        self.stale = false;
        self.marks.clear();
        self.page = 0;
        self.err = None;
    }
    /// Rank on a thread: the rows are the grid's (no reload), the vectors and rejections come from the index.
    fn start(&mut self, rows: &[FileRow], ctx: &egui::Context) {
        if self.busy() || self.tag.is_empty() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        self.rank_rx = Some(rx);
        self.err = None;
        let (cfg, tag, mode, positives, rows, ctx) = (
            self.cfg.clone(),
            self.tag.clone(),
            self.mode,
            self.positives.clone(),
            rows.to_vec(),
            ctx.clone(),
        );
        std::thread::spawn(move || {
            let result = (|| -> Result<Ranked, String> {
                let db = Db::open_cfg(&cfg)?;
                let vecs = propose::cached(&db)?;
                if mode != Mode::Text && vecs.is_empty() {
                    return Err("No image vectors yet: run `memetag embeddings` once (a few minutes), or pick the text mode.".into());
                }
                let text = (mode != Mode::Image).then(|| propose::TextSpace::build(&rows));
                let rejected = propose::rejects(&db, &tag)?;
                let positives: HashSet<String> = positives.into_iter().collect();
                let ranked = propose::rank(
                    &rows,
                    &vecs,
                    text.as_ref(),
                    &positives,
                    &rejected,
                    |r| {
                        !r.tags.contains(&tag)
                            && !positives.contains(&r.path)
                            && !rejected.contains(&r.path)
                    },
                    mode,
                )?;
                propose::remember_mode(&db, &tag, mode)?;
                let scored = ranked.len();
                let mut rejected: Vec<String> = rejected.into_iter().collect();
                rejected.sort();
                Ok(Ranked {
                    proposals: ranked.into_iter().take(KEEP).collect(),
                    scored,
                    rejected,
                })
            })();
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }
    /// Take these files off the tag's rejection list, in the index and in the card.
    fn unreject(&mut self, paths: Vec<String>, rows: &[FileRow]) {
        if paths.is_empty() {
            return;
        }
        match Db::open_cfg(&self.cfg).and_then(|db| propose::unreject(&db, &self.tag, &paths)) {
            Ok(()) => {
                if let Some(r) = &mut self.ranked {
                    r.rejected.retain(|p| !paths.contains(p));
                }
                self.remap(rows);
                self.stale = true;
            }
            Err(e) => self.err = Some(format!("Un-reject failed: {e}")),
        }
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        rows: &[FileRow],
        // texture, the size of one frame, and how many frames the texture holds side by side (a storyboard strip)
        textures: &HashMap<usize, Texture>,
    ) -> Action {
        if self.closing {
            return Action::Close;
        }
        if self.loaded.is_none() && self.err.is_none() {
            match self.load_rx.try_recv() {
                Ok(Ok(l)) => {
                    if let Some((t, _)) = l.choices.first() {
                        let t = t.clone();
                        self.loaded = Some(l);
                        self.pick_tag(t);
                    } else {
                        self.loaded = Some(l);
                    }
                }
                Ok(Err(e)) => self.err = Some(e),
                Err(mpsc::TryRecvError::Disconnected) => self.err = Some("Loading stopped".into()),
                _ => ctx.request_repaint_after(std::time::Duration::from_millis(50)),
            }
        }
        if let Some(rx) = &self.rank_rx {
            match rx.try_recv() {
                Ok(Ok(r)) => {
                    self.ranked = Some(r);
                    self.remap(rows);
                    self.marks.clear();
                    self.page = 0;
                    self.stale = false;
                    self.rank_rx = None;
                }
                Ok(Err(e)) => {
                    self.err = Some(e);
                    self.rank_rx = None;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.err = Some("Ranking stopped".into());
                    self.rank_rx = None;
                }
                _ => ctx.request_repaint_after(std::time::Duration::from_millis(50)),
            }
        }
        let max = ctx.content_rect().size() - egui::vec2(40.0, 60.0);
        let mut open = true;
        let mut action = Action::None;
        let mut unreject: Vec<String> = vec![];
        egui::Window::new("Propose").open(&mut open).collapsible(false).resizable(true).vscroll(true)
            .default_size(egui::vec2(1000.0, 700.0).min(max)).max_size(max).show(ctx, |ui| {
            let Some(loaded) = &self.loaded else {
                if let Some(e) = &self.err { ui.colored_label(widgets::ERROR, e); } else { ui.label("Reading the selection's tags…"); ui.spinner(); }
                return;
            };
            if loaded.choices.is_empty() {
                ui.label(format!("None of the {} selected files carries a hand-written tag. Tag them first; the tag they share is what gets proposed.", self.positives.len()));
                return;
            }
            let choices = loaded.choices.clone();
            let mut picked: Option<String> = None;
            ui.horizontal(|ui| {
                ui.label(format!("{} selected files define", self.positives.len()));
                egui::ComboBox::from_id_salt("propose-tag").selected_text(&self.tag).show_ui(ui, |ui| {
                    for (t, n) in &choices {
                        if ui.selectable_label(*t == self.tag, format!("{t}  ({n} of {})", self.positives.len())).clicked() { picked = Some(t.clone()); }
                    }
                });
                ui.label("by");
                for m in Mode::ALL {
                    if ui.selectable_label(self.mode == m, m.as_str()).on_hover_text(match m {
                        Mode::Image => "How the picture looks (CLIP vector of the thumbnail): characters, art style, templates",
                        Mode::Text => "What the OCR text says (TF-IDF): tags that live in the caption",
                        Mode::Both => "Both likenesses summed",
                    }).clicked() { self.mode = m; }
                }
                if ui.add_enabled(!self.busy() && !self.tag.is_empty(), egui::Button::new("Rank")).clicked() { self.start(rows, ctx); }
                if self.busy() { ui.spinner(); ui.label("ranking…"); }
            });
            if let Some(t) = picked { self.pick_tag(t); }
            if let Some(e) = &self.err { ui.colored_label(widgets::ERROR, e); }
            let Some(ranked) = &self.ranked else {
                ui.small("Rank scores every file without the tag by likeness to the selection and shows the closest. The mode is remembered per tag.");
                return;
            };
            let accepted = self.marks.values().filter(|m| **m == Mark::Accept).count();
            let rejected_now = self.marks.values().filter(|m| **m == Mark::Reject).count();
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("{} files without \"{}\" scored, the {} closest shown.", ranked.scored, self.tag, ranked.proposals.len()));
                if !ranked.rejected.is_empty() {
                    let n = ranked.rejected.len();
                    if ui.selectable_label(self.show_rejected, format!("{n} rejected earlier pulling away")).on_hover_text("Show them; a click there takes a file off the list").clicked() { self.show_rejected = !self.show_rejected; }
                }
                if self.stale { ui.colored_label(widgets::OCR_PENDING, "Rank again to offer the un-rejected files."); }
            });
            ui.small("Left-click a tile: accept (green), then reject (red), then neither. Middle-click copies the file. Right-click opens the original. Accept hands the green ones to the batch panel for review; the red ones are remembered against this tag.");
            ui.horizontal(|ui| {
                if ui.add_enabled(accepted + rejected_now > 0, egui::Button::new(format!("Accept {accepted}, reject {rejected_now}"))).clicked() {
                    let mut accept = vec![]; let mut reject = vec![];
                    for (p, m) in &self.marks { match m { Mark::Accept => accept.push(p.clone()), Mark::Reject => reject.push(p.clone()) } }
                    accept.sort(); reject.sort();
                    action = Action::Apply { tag: self.tag.clone(), accept, reject };
                }
                if ui.button("Reject the rest of this page").on_hover_text("Every unmarked tile on this page turns red: you looked, they are not it").clicked() {
                    for p in ranked.proposals.iter().skip(self.page * PAGE).take(PAGE) {
                        self.marks.entry(p.path.clone()).or_insert(Mark::Reject);
                    }
                }
                if ui.add_enabled(!self.marks.is_empty(), egui::Button::new("Clear marks")).clicked() { self.marks.clear(); }
                let pages = ranked.proposals.len().div_ceil(PAGE).max(1);
                self.page = self.page.min(pages - 1);
                if pages > 1 {
                    ui.separator();
                    if ui.add_enabled(self.page > 0, egui::Button::new("◀")).clicked() { self.page -= 1; }
                    ui.label(format!("{}–{} of {}", self.page * PAGE + 1, ((self.page + 1) * PAGE).min(ranked.proposals.len()), ranked.proposals.len()));
                    if ui.add_enabled(self.page + 1 < pages, egui::Button::new("▶")).clicked() { self.page += 1; }
                }
            });
            if self.show_rejected && !ranked.rejected.is_empty() {
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("Rejected for \"{}\": {} files", self.tag, ranked.rejected.len())).strong());
                    if ui.small_button("Un-reject all").clicked() { unreject = ranked.rejected.clone(); }
                });
                ui.small("Click a tile to un-reject it; middle-click copies, right-click opens the original.");
                ui.horizontal_wrapped(|ui| {
                    for (p, &i) in ranked.rejected.iter().zip(&self.rejected_rows) {
                        let row = &rows[i];
                        let resp = tile(ui, row, textures.get(&i), &format!("rejected · {}", tags_label(row)), Some(egui::Stroke::new(3.0, widgets::ERROR)));
                        let resp = resp.on_hover_text(format!("{}\nClick to un-reject", hover_text(row)));
                        if resp.clicked() { unreject.push(p.clone()); }
                        if resp.clicked_by(egui::PointerButton::Middle) { action = Action::Copy(i); }
                        if resp.secondary_clicked() { action = Action::Open(i); }
                    }
                });
            }
            ui.separator();
            if ranked.proposals.is_empty() { ui.label("Nothing to propose: every file that could be scored already carries the tag or was rejected."); return; }
            let mut toggled: Option<String> = None;
            ui.horizontal_wrapped(|ui| {
                for (p, &i) in ranked.proposals.iter().zip(&self.rows).skip(self.page * PAGE).take(PAGE) {
                    let row = &rows[i];
                    let stroke = match self.marks.get(&p.path) {
                        Some(Mark::Accept) => Some(egui::Stroke::new(3.0, widgets::OK)),
                        Some(Mark::Reject) => Some(egui::Stroke::new(3.0, widgets::ERROR)),
                        None => None,
                    };
                    let resp = tile(ui, row, textures.get(&i), &format!("{:.2} · {}", p.score, tags_label(row)), stroke);
                    // the hover Edit button, as on a grid tile; its click must not also cycle the mark
                    let pic = egui::Rect::from_min_size(resp.rect.min, egui::vec2(TILE, TILE));
                    let edit_rect = egui::Rect::from_min_size(pic.min + egui::vec2(4.0, 4.0), egui::vec2(44.0, 24.0));
                    let edit_hit = ui.rect_contains_pointer(pic) && ui.place(edit_rect, egui::Button::new("Edit")).on_hover_text("Edit this file's tags and text; it leaves the list once it carries the tag").clicked();
                    let resp = resp.on_hover_text(format!("{}\nscore {:.3}", hover_text(row), p.score));
                    if edit_hit { action = Action::Edit(i); }
                    else if resp.clicked() { toggled = Some(p.path.clone()); }
                    if resp.clicked_by(egui::PointerButton::Middle) { action = Action::Copy(i); }
                    if resp.secondary_clicked() { action = Action::Open(i); }
                }
            });
            if let Some(p) = toggled {
                match self.marks.get(&p) {
                    None => { self.marks.insert(p, Mark::Accept); }
                    Some(Mark::Accept) => { self.marks.insert(p, Mark::Reject); }
                    Some(Mark::Reject) => { self.marks.remove(&p); }
                }
            }
        });
        if !open {
            return Action::Close;
        }
        self.unreject(unreject, rows);
        action
    }
}

fn tags_label(row: &FileRow) -> String {
    if row.xmp_tag_count == 0 {
        "no tags".to_string()
    } else {
        format!("{} tags", row.xmp_tag_count)
    }
}
fn hover_text(row: &FileRow) -> String {
    format!(
        "{}\n{} × {} · {} · {}\n{}",
        row.path,
        row.width,
        row.height,
        row.format,
        tags_label(row),
        if row.tags.is_empty() {
            "No tags yet".to_string()
        } else {
            row.tags
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" · ")
        }
    )
}
/// One tile: the picture (the first frame of a strip), an outline when asked or hovered, two small lines under it.
fn tile(
    ui: &mut egui::Ui,
    row: &FileRow,
    texture: Option<&Texture>,
    top_line: &str,
    stroke: Option<egui::Stroke>,
) -> egui::Response {
    let (cell, resp) = ui.allocate_exact_size(egui::vec2(TILE, TILE + 34.0), egui::Sense::click());
    let pic = egui::Rect::from_min_size(cell.min, egui::vec2(TILE, TILE));
    ui.painter()
        .rect_filled(pic, 4.0, ui.visuals().widgets.inactive.bg_fill);
    if let Some((tid, size, frames)) = texture {
        let scale = (TILE / size.x).min(TILE / size.y).min(4.0);
        let draw = egui::Rect::from_center_size(pic.center(), *size * scale);
        let uv = egui::Rect::from_min_max(
            egui::pos2(0.0, 0.0),
            egui::pos2(1.0 / (*frames).max(1) as f32, 1.0),
        );
        egui::Image::from_texture(egui::load::SizedTexture::new(*tid, draw.size()))
            .uv(uv)
            .paint_at(ui, draw);
    }
    let stroke = stroke.or_else(|| {
        resp.hovered()
            .then(|| ui.visuals().widgets.hovered.fg_stroke)
    });
    if let Some(s) = stroke {
        ui.painter()
            .rect_stroke(pic, 4.0, s, egui::StrokeKind::Inside);
    }
    let folder = row
        .path
        .rsplit_once('/')
        .map(|(d, _)| d)
        .unwrap_or("(root)");
    let font = egui::FontId::proportional(11.0);
    let color = ui.visuals().weak_text_color();
    ui.painter().text(
        pic.left_bottom() + egui::vec2(2.0, 4.0),
        egui::Align2::LEFT_TOP,
        top_line,
        font.clone(),
        color,
    );
    ui.painter().text(
        pic.left_bottom() + egui::vec2(2.0, 18.0),
        egui::Align2::LEFT_TOP,
        folder.chars().take(24).collect::<String>(),
        font,
        color,
    );
    resp
}
