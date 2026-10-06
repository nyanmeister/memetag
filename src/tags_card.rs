//! The Tags card: every tag written by hand, with counts, and a way to fix a mistaken one. Rename changes it in
//! every file (renaming to a tag that already exists merges the two); Delete takes it out of every file. Both hand
//! the work to the batch panel with the plan filled in, so it is reviewed and runs through the same journaled path
//! as any other mass edit. The card itself writes nothing to a file; it only drops the old name from the
//! suggestion history.
use crate::{
    autocomplete,
    index::Db,
    widgets::{self, TagInput},
    Cfg,
};
use eframe::egui;
use std::collections::BTreeSet;

pub enum Action {
    None,
    Close,
    /// Open the batch panel on these files with this plan.
    Batch {
        paths: Vec<String>,
        add: BTreeSet<String>,
        remove: BTreeSet<String>,
        /// (old, new) for a rename: once every file has it, the vocabulary follows (`Vocab::rename`).
        rename: Option<(String, String)>,
    },
}

const HELP: &str = "Every tag you wrote into a file, with how many files carry it. Rename changes the tag in all \
of them; renaming to a tag that already exists merges the two. Delete takes it out of every file. Either opens \
the usual batch plan, and nothing is written until you apply it there. Once a rename has reached every file, the \
Implications rules and aliases that mention the old name are rewritten to the new one. Folder and implied tags are \
not listed; they come from the folder name and the Implications rules. Ctrl+S opens the plan for the rename or \
delete you have open.";

pub struct TagsUi {
    /// Hand-written tags and how many files carry each, by name.
    counts: Vec<(String, usize)>,
    filter: String,
    suggestions: Vec<(String, u64, bool)>,
    /// The tag whose rename form is open.
    renaming: Option<String>,
    to: TagInput,
    /// The tag whose delete confirmation is open.
    deleting: Option<String>,
    error: Option<String>,
    closing: bool,
}

impl TagsUi {
    pub fn new(c: &Cfg) -> Self {
        let mut this = Self {
            counts: vec![],
            filter: String::new(),
            suggestions: vec![],
            renaming: None,
            to: TagInput::default(),
            deleting: None,
            error: None,
            closing: false,
        };
        this.reload(c);
        this
    }
    /// Re-read the counts from the index, after a batch changed files.
    pub fn reload(&mut self, c: &Cfg) {
        match load(c) {
            Ok((counts, suggestions)) => {
                self.counts = counts;
                self.suggestions = suggestions;
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
        let known = |t: &Option<String>| {
            t.as_ref()
                .is_some_and(|t| self.counts.iter().any(|(k, _)| k == t))
        };
        if !known(&self.renaming) {
            self.renaming = None;
        }
        if !known(&self.deleting) {
            self.deleting = None;
        }
    }
    pub fn request_close(&mut self) {
        self.closing = true;
    }
    fn files(n: usize) -> String {
        if n == 1 {
            "1 file".into()
        } else {
            format!("{n} files")
        }
    }
    /// The plan for one tag: every file carrying it by hand loses it and, for a rename, gains the new name.
    fn plan(&mut self, c: &Cfg, tag: &str, to: Option<String>) -> Action {
        match paths_with(c, tag) {
            Ok(paths) if paths.is_empty() => {
                self.error = Some(format!(
                    "No file carries \"{tag}\" any more; the list was stale"
                ));
                self.reload(c);
                Action::None
            }
            Ok(paths) => {
                forget(c, tag);
                self.renaming = None;
                self.deleting = None;
                self.to.text.clear();
                self.error = None;
                Action::Batch {
                    paths,
                    rename: to.clone().map(|t| (tag.to_string(), t)),
                    add: to.into_iter().collect(),
                    remove: BTreeSet::from([tag.to_string()]),
                }
            }
            Err(e) => {
                self.error = Some(e);
                Action::None
            }
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, c: &Cfg) -> Action {
        let max = ctx.content_rect().size() - egui::vec2(40.0, 60.0);
        let mut open = true;
        let mut action = Action::None;
        let mut rename: Option<(String, String)> = None;
        let mut delete: Option<String> = None;
        egui::Window::new("Tags")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .vscroll(true)
            .default_size(egui::vec2(760.0, 600.0).min(max))
            .max_size(max)
            .show(ctx, |ui| {
                egui::CollapsingHeader::new("How this works")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label(HELP);
                    });
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Find");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            .id_salt("tags-filter")
                            .desired_width(240.0)
                            .hint_text("part of a tag"),
                    );
                    ui.label(format!("{} tags written by hand", self.counts.len()));
                });
                if let Some(e) = &self.error {
                    ui.colored_label(widgets::ERROR, e);
                }
                ui.separator();
                let needle = self.filter.trim().to_lowercase();
                let shown: Vec<(String, usize)> = self
                    .counts
                    .iter()
                    .filter(|(t, _)| needle.is_empty() || t.to_lowercase().contains(&needle))
                    .cloned()
                    .collect();
                if shown.is_empty() {
                    ui.label(if self.counts.is_empty() {
                        "No tags written by hand yet."
                    } else {
                        "Nothing matches."
                    });
                }
                for (tag, n) in shown {
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&tag).strong());
                        ui.label(Self::files(n));
                        if ui
                            .selectable_label(self.renaming.as_deref() == Some(tag.as_str()), "Rename…")
                            .on_hover_text("Change this tag in every file, then in the Implications rules; to an existing tag's name merges the two")
                            .clicked()
                        {
                            self.deleting = None;
                            self.to.text.clear();
                            self.renaming = if self.renaming.as_deref() == Some(tag.as_str()) {
                                None
                            } else {
                                Some(tag.clone())
                            };
                        }
                        if ui
                            .selectable_label(self.deleting.as_deref() == Some(tag.as_str()), "Delete…")
                            .on_hover_text("Take this tag out of every file")
                            .clicked()
                        {
                            self.renaming = None;
                            self.deleting = if self.deleting.as_deref() == Some(tag.as_str()) {
                                None
                            } else {
                                Some(tag.clone())
                            };
                        }
                    });
                    if self.renaming.as_deref() == Some(tag.as_str()) {
                        ui.indent(("rename", &tag), |ui| {
                            ui.horizontal(|ui| {
                                ui.label("to");
                                if let Some(t) = self.to.show(ui, &self.suggestions, "tags-rename-to", "Rename", &std::collections::BTreeSet::new()) {
                                    let t = c.vocab.canon(&t);
                                    if t.is_empty() || t == tag {
                                        self.to.text.clear();
                                    } else {
                                        rename = Some((tag.clone(), t));
                                    }
                                }
                            });
                            let typed = c.vocab.canon(self.to.text.trim());
                            let existing = self.counts.iter().find(|(k, _)| *k == typed && *k != tag);
                            ui.small(match existing {
                                Some((k, m)) => format!(
                                    "\"{k}\" already exists on {}; this merges \"{tag}\" into it.",
                                    Self::files(*m)
                                ),
                                None => format!(
                                    "Enter, Tab, Ctrl+S or a suggestion opens the plan: {} lose \"{tag}\" and get the new name.",
                                    Self::files(n)
                                ),
                            });
                        });
                    }
                    if self.deleting.as_deref() == Some(tag.as_str()) {
                        ui.indent(("delete", &tag), |ui| {
                            ui.horizontal(|ui| {
                                ui.label(format!("Take \"{tag}\" out of {}?", Self::files(n)));
                                if ui.button("Open the plan").clicked() {
                                    delete = Some(tag.clone());
                                }
                                if ui.button("Keep it").clicked() {
                                    self.deleting = None;
                                }
                            });
                        });
                    }
                }
            });
        // Ctrl+S commits whichever of the two is open (wishlist, 2026-09-22), the same as Enter in the field or the button
        if rename.is_none() && delete.is_none() && crate::widgets::chord(&ctx, egui::Key::S) {
            if let Some(tag) = self.renaming.clone() {
                let t = c.vocab.canon(self.to.text.trim());
                if !t.is_empty() && t != tag {
                    rename = Some((tag, t));
                }
            } else if let Some(tag) = self.deleting.clone() {
                delete = Some(tag);
            }
        }
        if let Some((from, to)) = rename {
            action = self.plan(c, &from, Some(to));
        } else if let Some(tag) = delete {
            action = self.plan(c, &tag, None);
        }
        if !open {
            self.closing = true;
        }
        if self.closing && matches!(action, Action::None) {
            action = Action::Close;
        }
        if matches!(action, Action::Close) {
            self.closing = false;
        }
        action
    }
}

fn load(c: &Cfg) -> Result<(Vec<(String, usize)>, Vec<(String, u64, bool)>), String> {
    let db = Db::open_cfg(&c)?;
    let mut st = db
        .conn
        .prepare("SELECT tag, COUNT(DISTINCT path) FROM tags WHERE source='xmp' GROUP BY tag ORDER BY tag")
        .map_err(|e| e.to_string())?;
    let counts = st
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        })
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    Ok((counts, autocomplete::load(c)))
}

fn paths_with(c: &Cfg, tag: &str) -> Result<Vec<String>, String> {
    let db = Db::open_cfg(&c)?;
    let mut st = db
        .conn
        .prepare("SELECT DISTINCT path FROM tags WHERE source='xmp' AND tag=?1 ORDER BY path")
        .map_err(|e| e.to_string())?;
    let paths = st
        .query_map([tag], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .filter_map(Result::ok)
        .collect();
    Ok(paths)
}

/// The old name leaves the suggestion history; the files still carrying it keep it in the pool until the batch runs.
fn forget(c: &Cfg, tag: &str) {
    if let Ok(db) = Db::open_cfg(&c) {
        let _ = db
            .conn
            .execute("DELETE FROM tag_history WHERE tag=?1", [tag]);
    }
}
