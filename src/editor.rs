//! Single-file edits: preserve timestamps/pixels and keep human OCR independent
//! of the machine-text table, including writes from an already-running old worker.
use crate::{
    containers,
    index::{Db, FileRow},
    tagger, xmp, Cfg,
};
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Clone)]
pub struct Draft {
    pub path: String,
    /// the file's mtime in unix seconds — its download time, kept across tagging; shown in the editor header
    pub mtime: f64,
    pub tags: BTreeSet<String>,
    pub inherited: Vec<(String, String)>,
    pub text: String,
    pub original_text: String,
    pub machine: String,
    pub reviewed: bool,
    pub originally_reviewed: bool,
    pub original_tags: BTreeSet<String>,
    pub revision: Vec<u8>,
}

pub fn load(c: &Cfg, rel: &str) -> Result<Draft, String> {
    let path = c.file_path(rel)?;
    let bytes = crate::batch::read_for_edit(c, &path)?;
    let mtime = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let packet = containers::get_xmp(&bytes)?;
    let parsed = packet
        .as_deref()
        .map(xmp::read)
        .transpose()?
        .unwrap_or_default();
    let db = Db::open_cfg(&c)?;
    let machine = db
        .conn
        .query_row("SELECT body FROM text WHERE path=?1 LIMIT 1", [rel], |r| {
            r.get::<_, String>(0)
        })
        .optional()
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let reviewed = parsed.fields.get("textSource").map(String::as_str) == Some("manual");
    let text = if reviewed {
        parsed.fields.get("text").cloned().unwrap_or_default()
    } else if !machine.is_empty() {
        machine.clone()
    } else {
        parsed.fields.get("text").cloned().unwrap_or_default()
    };
    let tags: BTreeSet<_> = parsed.tags.iter().map(|t| c.vocab.canon(t)).collect();
    let mut st = db
        .conn
        .prepare("SELECT tag,source FROM tags WHERE path=?1 AND source!='xmp' ORDER BY source,tag")
        .map_err(|e| e.to_string())?;
    let inherited = st
        .query_map([rel], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(Draft {
        path: rel.into(),
        mtime,
        original_tags: tags.clone(),
        tags,
        inherited,
        original_text: text.clone(),
        text,
        machine,
        reviewed,
        originally_reviewed: reviewed,
        revision: Sha256::digest(&bytes).to_vec(),
    })
}

pub fn save(c: &Cfg, draft: &Draft) -> Result<FileRow, String> {
    let path = c.file_path(&draft.path)?;
    let bytes = crate::batch::read_for_edit(c, &path)?;
    if Sha256::digest(&bytes).as_slice() != draft.revision {
        return Err("The file changed since editing began. Cancel and reopen it to reload.".into());
    }
    let tags: BTreeSet<_> = draft
        .tags
        .iter()
        .map(|t| c.vocab.canon(t))
        .filter(|t| !t.is_empty())
        .collect();
    let ops: Vec<String> = draft
        .original_tags
        .difference(&tags)
        .map(|t| format!("-{t}"))
        .chain(
            tags.difference(&draft.original_tags)
                .map(|t| format!("+{t}")),
        )
        .collect();
    let db = Db::open_cfg(&c)?;
    let mut fields = vec![];
    if draft.reviewed {
        fields.push(("text", draft.text.clone()));
        fields.push(("textSource", "manual".into()));
    } else if draft.originally_reviewed {
        let latest: String = db
            .conn
            .query_row(
                "SELECT body FROM text WHERE path=?1 LIMIT 1",
                [&draft.path],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        fields.push(("text", latest));
        fields.push(("textSource", "auto".into()));
    }
    let after = tagger::edit_bytes(c, &path, bytes, &ops, &fields)?;
    // A tag-only save leaves the index's machine text alone, as the mass edit does: the file may carry an older
    // engine's embedded text, and importing that here would put it over the current engine's reading while
    // text_meta still says the current engine is done (review, 2026-09-22).
    let indexed = if fields.is_empty() {
        std::fs::metadata(&path)
            .map_err(|e| e.to_string())
            .and_then(|md| db.import_tag_scan(c.scan_bytes(&path, &after, &md)?))
    } else {
        db.upsert_cfg(c, &path, &after).map(|_| ())
    };
    indexed.map_err(|e| {
        format!(
            "File saved, but index update failed: {e}. Reopen to retry. Changes are in the file."
        )
    })?;
    // only tags this save added count as a use: re-saving an image must not inflate its old tags' ranking
    for tag in tags.difference(&draft.original_tags) {
        db.conn.execute("INSERT INTO tag_history(tag,uses) VALUES(?1,1) ON CONFLICT(tag) DO UPDATE SET uses=uses+1", [tag]).map_err(|e| e.to_string())?;
    }
    db.row(&draft.path)?
        .ok_or("Saved file missing from index".into())
}

use eframe::egui;
use std::sync::mpsc;
type Loaded = Result<(Draft, Vec<(String, u64, bool)>), String>;
const NEXT_HELP: &str = "Save, then open the next file in these results with fewer than 3 hand-written tags (the Tagging assistant's untagged and thin tiers), continuing from this one. Ctrl+Enter.";

pub enum Action {
    None,
    Cancel,
    OpenOriginal,
    /// The file was written; `true` asks the grid to open the next file needing tags in the current results.
    Saved(FileRow, bool),
    /// Leave this file as it is and open the next one needing tags.
    Next,
}
pub struct EditorUi {
    draft: Option<Draft>,
    load_rx: mpsc::Receiver<Loaded>,
    save_rx: Option<mpsc::Receiver<Result<FileRow, String>>>,
    suggestions: Vec<(String, u64, bool)>,
    input: crate::widgets::TagInput,
    error: Option<String>,
    discard: bool,
    /// Set by "Save and next": the Saved action then asks for the next file.
    next: bool,
    /// Set by "Skip": leaving (at once, or after the discard prompt) yields Next instead of Cancel.
    skip: bool,
    primary: crate::xsel::Primary,
}

impl EditorUi {
    pub fn new(c: Cfg, path: String, ctx: egui::Context) -> Self {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let result = load(&c, &path).map(|d| (d, crate::autocomplete::load(&c)));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
        Self {
            draft: None,
            load_rx: rx,
            save_rx: None,
            suggestions: vec![],
            input: Default::default(),
            error: None,
            discard: false,
            next: false,
            skip: false,
            primary: Default::default(),
        }
    }
    /// Leaving without saving: Next when Skip asked for it, Cancel otherwise.
    fn leave(&self) -> Action {
        if self.skip {
            Action::Next
        } else {
            Action::Cancel
        }
    }
    /// Compared against the loaded state, so typing and deleting again is not a change (no phantom discard prompt).
    fn dirty(&self) -> bool {
        let Some(d) = &self.draft else { return false };
        !self.input.text.trim().is_empty()
            || d.tags != d.original_tags
            || d.reviewed != d.originally_reviewed
            || (d.reviewed && d.text != d.original_text)
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        c: &Cfg,
        thumb: Option<(egui::TextureId, egui::Vec2, u32)>,
    ) -> Action {
        let ctx = ui.ctx().clone();
        if self.draft.is_none() && self.error.is_none() {
            match self.load_rx.try_recv() {
                Ok(Ok((draft, suggestions))) => {
                    self.draft = Some(draft);
                    self.suggestions = suggestions;
                }
                Ok(Err(e)) => self.error = Some(e),
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.error = Some("Editor loading stopped".into())
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(50))
                }
            }
        }
        if let Some(rx) = &self.save_rx {
            match rx.try_recv() {
                Ok(Ok(row)) => return Action::Saved(row, self.next),
                Ok(Err(e)) => {
                    self.error = Some(e);
                    self.save_rx = None;
                    self.next = false; // a later plain Save must not jump on (review, 2026-09-22)
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.error = Some("Saving stopped; reopen to check the file".into());
                    self.save_rx = None;
                    self.next = false;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    ctx.request_repaint_after(std::time::Duration::from_millis(50))
                }
            }
        }
        let busy = self.save_rx.is_some();
        let mut action = Action::None;
        let mut save_clicked = false;
        let mut cancel = false;
        let mut save_next = false;
        let mut skip = false;
        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Edit tags and text");
                let loaded = !busy && self.draft.is_some();
                if ui.add_enabled(loaded, egui::Button::new("Save")).clicked() {
                    save_clicked = true;
                }
                if ui
                    .add_enabled(loaded, egui::Button::new("Save and next"))
                    .on_hover_text(NEXT_HELP)
                    .clicked()
                {
                    save_next = true;
                }
                if ui
                    .add_enabled(!busy, egui::Button::new("Skip"))
                    .on_hover_text("Leave this file as it is and open the next one needing tags")
                    .clicked()
                {
                    skip = true;
                }
                if ui.add_enabled(!busy, egui::Button::new("Cancel")).clicked() {
                    cancel = true;
                    self.skip = false;
                }
                if ui.button("Open original").clicked() {
                    action = Action::OpenOriginal;
                }
                if busy {
                    ui.spinner();
                    ui.label("Saving…");
                }
            });
            if let Some(e) = &self.error {
                ui.colored_label(crate::widgets::ERROR, e);
            }
            if self.discard {
                // Boxed and bold: as a plain line it went unnoticed, and Escape seemed dead (2026-09-22)
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.strong("Discard unsaved edits?");
                        if ui.button("Discard").clicked() {
                            action = self.leave();
                        }
                        if ui.button("Keep editing").clicked() {
                            self.discard = false;
                            self.skip = false;
                        }
                        ui.small("Escape keeps editing");
                    });
                });
            }
            let Some(draft) = self.draft.as_mut() else { if self.error.is_none() { ui.spinner(); ui.label("Loading…"); } return; };
            ui.label(&draft.path);
            ui.label(
                egui::RichText::new(format!("Downloaded {}", crate::index::fmt_time(draft.mtime)))
                    .weak(),
            )
            .on_hover_text("The file's modified time — when it was saved here — kept across tagging");
            // Salted per image so each edit opens at the top and never touches the grid's scroll position.
            egui::ScrollArea::vertical().id_salt(("editor", draft.path.as_str())).show(ui, |ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    if let Some((id, size, frames)) = thumb {
                        let frame = egui::vec2(size.x / frames as f32, size.y);
                        let scale = (200.0 / frame.x).min(160.0 / frame.y).min(1.0);
                        let image = egui::Image::from_texture(egui::load::SizedTexture::new(
                            id,
                            frame * scale,
                        ))
                        .uv(egui::Rect::from_min_max(
                            egui::Pos2::ZERO,
                            egui::pos2(1.0 / frames as f32, 1.0),
                        ))
                        .sense(egui::Sense::click());
                        // The same action as the Open original button.
                        if ui
                            .add(image)
                            .on_hover_cursor(egui::CursorIcon::PointingHand)
                            .on_hover_text("Open original in feh/mpv")
                            .clicked()
                        {
                            action = Action::OpenOriginal;
                        }
                    }
                    ui.strong("Tags");
                    crate::widgets::chips(ui, "", &mut draft.tags, |_| "Remove tag".into());
                    if let Some(tag) = self.input.show(ui, &self.suggestions, "edit-tag-input", "Add", &draft.tags) { draft.tags.insert(c.vocab.canon(&tag)); }
                    if !draft.inherited.is_empty() {
                        ui.collapsing("Tags from folders and implications", |ui| {
                            for (tag, source) in &draft.inherited { ui.label(format!("{tag} ({source})")); }
                        });
                    }
                    ui.separator(); ui.strong("OCR text");
                    let mut out = egui::TextEdit::multiline(&mut draft.text).id_salt("edit-ocr-text").desired_rows(7).desired_width(f32::INFINITY).show(ui);
                    let pasted = self.primary.sync(ui, &mut out, &mut draft.text, true);
                    if out.response.changed() || pasted { draft.reviewed = true; }
                    if ui.checkbox(&mut draft.reviewed, "Human-reviewed — keep this text, even if empty").changed() && !draft.reviewed { draft.text = draft.machine.clone(); }
                    ui.collapsing("Machine transcription", |ui| { ui.label(if draft.machine.is_empty() { "No machine text available" } else { &draft.machine }); });
                    ui.small("Save keeps the file's modification date. Ctrl+S saves; Ctrl+Enter saves and opens the next file needing tags; Escape cancels, asking first when there are unsaved edits.");
                });
            });
        });
        if !busy && crate::widgets::escape(&ctx) {
            // With the prompt showing, Escape puts it away: a held key must never discard, and a prompt that went
            // unnoticed is found again by the key that raised it
            cancel = !self.discard;
            self.discard = false;
            self.skip = false;
        }
        if skip {
            cancel = true;
            self.skip = true;
        }
        if cancel {
            if self.dirty() {
                self.discard = true;
            } else {
                return self.leave();
            }
        }
        // real presses only: a held Ctrl+Enter used to save-and-next through one file per key repeat
        save_clicked |= !busy && crate::widgets::chord(&ctx, egui::Key::S);
        save_next |= !busy && crate::widgets::chord(&ctx, egui::Key::Enter);
        if save_next && self.draft.is_some() {
            save_clicked = true;
            self.next = true;
        }
        if save_clicked && !busy {
            if let Some(draft) = &mut self.draft {
                if !self.input.text.trim().is_empty() {
                    draft.tags.insert(c.vocab.canon(&self.input.text));
                    self.input.text.clear();
                }
                let draft = draft.clone();
                let c = c.clone();
                let wake = ctx.clone();
                let (tx, rx) = mpsc::channel();
                self.save_rx = Some(rx);
                self.error = None;
                std::thread::spawn(move || {
                    let _ = tx.send(save(&c, &draft));
                    wake.request_repaint();
                });
            }
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{self, FileTimes};
    use std::os::unix::fs::MetadataExt;
    use std::time::{Duration, UNIX_EPOCH};
    #[test]
    fn edits_preserve_file_and_override_an_old_ocr_worker() {
        let root = std::env::temp_dir().join(format!("memetag-editor-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let c = Cfg {
            sources: None,
            active_source: None,
            root: root.clone(),
            db: root.join("index.sqlite"),
            thumbs: root.join("thumbs"),
            vocab: crate::vocab::Vocab::default(),
            preview_fps: 4.0,
            strip_frames: 8,
            index_threads: 1,
            texture_budget_mb: 64,
            embed_model: None,
            ocr_model: None,
            ocr_prompt: String::new(),
            translate_model: None,
            translate_prompt: String::new(),
            speech_command: String::new(),
            ollama_url: "http://127.0.0.1:1".into(),
        };
        let db = Db::open_cfg(&c).unwrap();
        for rel in [
            "test.png",
            "test.jpg",
            "test.gif",
            "test.webp",
            "animated.png",
            "animated.webp",
        ] {
            let path = root.join(rel);
            if rel.starts_with("animated") {
                fs::copy(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures")
                        .join(rel),
                    &path,
                )
                .unwrap();
            } else {
                image::RgbImage::from_pixel(16, 16, image::Rgb([90, 40, 10]))
                    .save(&path)
                    .unwrap();
            }
            let original_content = containers::strip_xmp(&fs::read(&path).unwrap()).unwrap();
            let time = UNIX_EPOCH + Duration::new(1_500_000_000, 123_456_789);
            fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(FileTimes::new().set_modified(time))
                .unwrap();
            db.upsert_file(&root, &path, &c.vocab).unwrap();
            db.conn
                .execute(
                    "INSERT INTO text VALUES(?1,'original machine caption')",
                    [&rel],
                )
                .unwrap();
            let before = fs::metadata(&path).unwrap();
            let pixels = crate::writer::pixel_hash(&fs::read(&path).unwrap());
            let mut draft = load(&c, &rel).unwrap();
            draft.tags.insert("remember this".into());
            draft.reviewed = true;
            draft.text = "Human café caption".into();
            let row = save(&c, &draft).unwrap();
            assert_eq!(row.text, "Human café caption");
            assert!(row.tags.contains("remember this"));
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), time);
            assert_eq!(
                containers::strip_xmp(&fs::read(&path).unwrap()).unwrap(),
                original_content
            );
            assert_eq!(fs::metadata(&path).unwrap().ino(), before.ino());
            assert_eq!(
                fs::metadata(&path).unwrap().created().ok(),
                before.created().ok()
            );
            assert_eq!(crate::writer::pixel_hash(&fs::read(&path).unwrap()), pixels);
            assert_eq!(load(&c, &rel).unwrap().machine, "original machine caption");
            // Simulate the running pre-editor OCR executable, which knows
            // nothing about manual_text and freely replaces the machine table.
            db.conn
                .execute("DELETE FROM text WHERE path=?1", [&rel])
                .unwrap();
            db.conn
                .execute("INSERT INTO text VALUES(?1,'late machine result')", [&rel])
                .unwrap();
            assert_eq!(
                db.all()
                    .unwrap()
                    .into_iter()
                    .find(|r| r.path == rel)
                    .unwrap()
                    .text,
                "Human café caption"
            );
            assert!(tagger::set_field(&c, &path, "text", "unwanted OCR").is_err());
            assert!(tagger::set_cached_text(&c, &path, "stale snapshot").is_err());
            let mut blank = load(&c, &rel).unwrap();
            blank.text.clear();
            assert_eq!(save(&c, &blank).unwrap().text, "");
            // Rebuild the protection from the embedded metadata, not the cache.
            db.conn
                .execute("DELETE FROM manual_text WHERE path=?1", [&rel])
                .unwrap();
            db.upsert_file(&root, &path, &c.vocab).unwrap();
            assert_eq!(
                db.all()
                    .unwrap()
                    .into_iter()
                    .find(|r| r.path == rel)
                    .unwrap()
                    .text,
                ""
            );
            let mut automatic = load(&c, &rel).unwrap();
            automatic.reviewed = false;
            assert_eq!(save(&c, &automatic).unwrap().text, "late machine result");
            let mut stale = load(&c, &rel).unwrap();
            stale.tags.insert("must not overwrite".into());
            tagger::apply(&c, &path, &["+another editor".into()], &[]).unwrap();
            assert!(save(&c, &stale).err().unwrap().contains("changed since"));
            assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), time);
        }
        let suggestions = crate::autocomplete::matches(&crate::autocomplete::load(&c), "remember");
        assert!(suggestions.contains(&"remember this".to_string()));
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }
}
