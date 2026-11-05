//! The Tags card: every tag written by hand, with counts, and a way to fix a mistaken one. Rename changes it in
//! every file (renaming to a tag that already exists merges the two); Delete takes it out of every file. Both hand
//! the work to the batch panel with the plan filled in, so it is reviewed and runs through the same journaled path
//! as any other mass edit. The card itself writes nothing to a file; it only drops the old name from the
//! suggestion history.
//!
//! Categories live here too: a tinted chip per tag opens the menu that sets its kind, and the Categories section
//! above the list is where a category itself is made, tinted, renamed or taken away (asked 2026-11-05: "I don't
//! know how I can change the color of a category I've added"). Those go straight to vocab.toml through the grid.
//!
//! Only the rows in view are drawn (`widgets::rows_in_view`): the full list is thousands of rows, and drawing
//! them all every frame cost most of a core while the card was open (measured 2026-11-05, 3,551 tags).
use crate::{
    autocomplete,
    index::Db,
    vocab::{self, Vocab},
    widgets::{self, TagInput},
    Cfg,
};
use eframe::egui;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

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
    /// Set (Some) or clear (None) these tags' categories in vocab.toml; the grid saves and syncs.
    Categories(Vec<(String, Option<String>)>),
    /// A category's tint; `None` puts a built-in category back on its own tint.
    Color(String, Option<[u8; 3]>),
    /// A category of the owner's own: it exists through its tint until a tag joins it.
    AddCategory(String),
    /// (old, new): every tag in it moves, and the tint with them.
    RenameCategory(String, String),
    /// Its tags have no category afterwards.
    RemoveCategory(String),
}

const HELP: &str = "Every tag you wrote into a file, with how many files carry it. Rename changes the tag in all \
of them (renaming to a tag that already exists merges the two); Delete takes it out of every file. Either opens the \
usual batch plan, and nothing is written until you apply it there. Once a rename has reached every file, the \
Implications rules and aliases that mention the old name are rewritten to the new one. Folder and implied tags are \
not listed; they come from the folder name and the Implications rules. Ctrl+S opens the plan for the rename or \
delete you have open. The tinted chip on each row is the tag's category (what kind of tag it is: character, \
series, artist, …); click it to change it, which writes vocab.toml at once and syncs it. With a Find narrowing the \
list, \"Set all shown\" gives every listed tag one category in one go; Find also matches category names, and \
\"other\" lists the tags that have none yet. Categories themselves are made, tinted, renamed and removed in the \
Categories section: the swatch opens a colour picker, and a new name typed there or in a chip's menu becomes a \
category with a tint of its own.";

/// A row's height before any has been drawn; the first drawn row corrects it.
const ROW_GUESS: f32 = 22.0;
/// A picked colour is written this long after the last change, so a drag through the picker is one save.
const COLOR_SETTLE: Duration = Duration::from_millis(400);

pub struct TagsUi {
    /// (tag, files carrying it), sorted by tag: every hand-written tag in the index.
    counts: Vec<(String, usize)>,
    filter: String,
    /// The vocabulary the card reads categories and tints from; `follow` keeps it current after a save.
    vocab: Vocab,
    /// `vocab.category_order()`, computed when the vocabulary changes rather than per row per frame.
    order: Vec<String>,
    /// What is typed in a category menu's "new category…" field.
    new_category: String,
    /// What is typed in the Categories section's own "new category…" field.
    add_name: String,
    focus_filter: u8,
    suggestions: Vec<(String, u64, bool)>,
    renaming: Option<String>,
    to: TagInput,
    deleting: Option<String>,
    error: Option<String>,
    closing: bool,
    /// The rows the Find leaves: indices into `counts`, and the filter text they were made for.
    shown: Vec<usize>,
    shown_for: Option<String>,
    /// Each shown row's height as last drawn, for `rows_in_view`.
    heights: Vec<f32>,
    /// (category, hand-written tags in it) in row order, "other" last.
    by_category: Vec<(String, usize)>,
    /// A colour being picked: written once it has settled (`COLOR_SETTLE`) and the button is up.
    pending_color: Option<(String, [u8; 3], Instant)>,
    renaming_category: Option<String>,
    category_to: String,
    removing_category: Option<String>,
}

impl TagsUi {
    pub fn new(c: &Cfg) -> Self {
        let mut this = Self {
            counts: vec![],
            filter: String::new(),
            vocab: c.vocab.clone(),
            order: c.vocab.category_order(),
            new_category: String::new(),
            add_name: String::new(),
            focus_filter: 4,
            suggestions: vec![],
            renaming: None,
            to: TagInput::default(),
            deleting: None,
            error: None,
            closing: false,
            shown: vec![],
            shown_for: None,
            heights: vec![],
            by_category: vec![],
            pending_color: None,
            renaming_category: None,
            category_to: String::new(),
            removing_category: None,
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
        self.shown_for = None;
    }
    /// The vocabulary changed under the card (a category it set came back, a pull): read categories from it.
    pub fn follow(&mut self, vocab: &Vocab) {
        self.vocab = vocab.clone();
        self.order = vocab.category_order();
        self.shown_for = None;
        let gone = |c: &Option<String>| c.as_ref().is_some_and(|c| !self.order.contains(c));
        if gone(&self.renaming_category) {
            self.renaming_category = None;
        }
        if gone(&self.removing_category) {
            self.removing_category = None;
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
    fn tags(n: usize) -> String {
        if n == 1 {
            "1 tag".into()
        } else {
            format!("{n} tags")
        }
    }
    /// A tag whose category comes from another tag: a denial (not:x) and a relation (chibi<x>) take x's.
    fn derived(t: &str) -> bool {
        t.starts_with(vocab::NOT) || vocab::relation(t).is_some()
    }
    fn category_name(&self, t: &str) -> String {
        self.vocab
            .category_of(t)
            .unwrap_or_else(|| vocab::OTHER.to_string())
    }
    /// The rows for the current Find, and the per-category counts, when either input changed. Find matches the
    /// tag, or its category's name ("other" for none), so "series" and "other" narrow by kind.
    fn refilter(&mut self) {
        if self.shown_for.as_deref() == Some(self.filter.as_str()) {
            return;
        }
        let needle = self.filter.trim().to_lowercase();
        self.shown = (0..self.counts.len())
            .filter(|&i| {
                let t = &self.counts[i].0;
                needle.is_empty()
                    || t.to_lowercase().contains(&needle)
                    || self.category_name(t) == needle
            })
            .collect();
        self.heights = vec![0.0; self.shown.len()];
        let mut by: Vec<(String, usize)> = self
            .order
            .iter()
            .cloned()
            .chain(std::iter::once(vocab::OTHER.to_string()))
            .map(|c| (c, 0))
            .collect();
        for (t, _) in &self.counts {
            let c = self.category_name(t);
            if let Some(row) = by.iter_mut().find(|(n, _)| *n == c) {
                row.1 += 1;
            }
        }
        self.by_category = by;
        self.shown_for = Some(self.filter.clone());
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
    /// A category chip as a menu: tinted with the category, the menu lists every category, "no category", and a
    /// field for a new name (asked 2026-10-31: "where do I make new categories?"): Enter there creates the category
    /// by giving this tag to it; from then on it is in every menu and the editor's rows.
    fn category_menu(
        &mut self,
        ui: &mut egui::Ui,
        id: &str,
        now: Option<&str>,
        label: &str,
        order: &[String],
    ) -> Option<Option<String>> {
        let mut pick: Option<Option<String>> = None;
        let b = widgets::tinted_button(
            ui,
            label,
            widgets::category_fill(&self.vocab, now.unwrap_or(vocab::OTHER)),
        );
        ui.push_id(id, |ui| {
            // stays open on a click inside (the new-name field), as the picks close it themselves
            let config = egui::containers::menu::MenuConfig::new()
                .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside);
            egui::containers::menu::MenuButton::from_button(b)
                .config(config)
                .ui(ui, |ui| {
                    for n in order {
                        if ui
                            .selectable_label(now == Some(n.as_str()), n.as_str())
                            .clicked()
                        {
                            pick = Some(Some(n.clone()));
                            ui.close();
                        }
                    }
                    if ui.selectable_label(now.is_none(), "no category").clicked() {
                        pick = Some(None);
                        ui.close();
                    }
                    ui.separator();
                    let mut make = false;
                    ui.horizontal(|ui| {
                        let field = ui.add(
                            egui::TextEdit::singleline(&mut self.new_category)
                                .hint_text("new category…")
                                .desired_width(120.0),
                        );
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter));
                        make |= enter && (field.has_focus() || field.lost_focus());
                        make |= ui
                            .add_enabled(
                                !self.new_category.trim().is_empty(),
                                egui::Button::new("Add"),
                            )
                            .clicked();
                    });
                    if make {
                        let name = self.new_category.trim().to_lowercase();
                        if !name.is_empty() && name != vocab::OTHER {
                            pick = Some(Some(name));
                            self.new_category.clear();
                            ui.close();
                        }
                    }
                });
        });
        pick
    }

    /// The Categories section: one row per category with its swatch (a colour picker), how many tags it holds,
    /// Rename… and Remove…; "other" counts the rest; a field at the end makes a new one.
    fn categories_section(&mut self, ui: &mut egui::Ui, action: &mut Action) {
        ui.small("The tint a category's tags wear, everywhere they show. Click a swatch to change it; a new name becomes a category with a tint of its own.");
        let rows = self.by_category.clone();
        let plain = ui.visuals().widgets.inactive.bg_fill;
        for (name, n) in rows {
            let other = name == vocab::OTHER;
            ui.horizontal(|ui| {
                if other {
                    // no tint to pick: "other" is the absence of a category
                    ui.add_space(ui.spacing().interact_size.y + ui.spacing().item_spacing.x);
                } else {
                    let mut rgb = self.vocab.color_of(&name).unwrap_or([plain.r(), plain.g(), plain.b()]);
                    let r = ui
                        .color_edit_button_srgb(&mut rgb)
                        .on_hover_text("The tint this category's tags wear; a change is saved once it settles");
                    if r.changed() {
                        self.vocab.set_color(&name, Some(rgb));
                        self.pending_color = Some((name.clone(), rgb, Instant::now()));
                    }
                }
                let chip = widgets::tinted_button(ui, &name, widgets::category_fill(&self.vocab, &name));
                ui.add(chip.sense(egui::Sense::hover()));
                ui.label(egui::RichText::new(Self::tags(n)).weak());
                if other {
                    ui.label(egui::RichText::new("tags with no category").weak());
                    return;
                }
                if vocab::default_color(&name).is_some()
                    && self.vocab.colors.contains_key(&name)
                    && ui
                        .small_button("own tint")
                        .on_hover_text("Back to the tint this category came with")
                        .clicked()
                {
                    *action = Action::Color(name.clone(), None);
                }
                if ui
                    .selectable_label(self.renaming_category.as_deref() == Some(name.as_str()), "Rename…")
                    .on_hover_text("Every tag in it moves to the new name, and the tint with them; onto an existing category the two merge")
                    .clicked()
                {
                    self.removing_category = None;
                    self.category_to.clear();
                    self.renaming_category = if self.renaming_category.as_deref() == Some(name.as_str()) {
                        None
                    } else {
                        Some(name.clone())
                    };
                }
                if ui
                    .selectable_label(self.removing_category.as_deref() == Some(name.as_str()), "Remove…")
                    .on_hover_text("Its tags stay on their files and go under \"other\"")
                    .clicked()
                {
                    self.renaming_category = None;
                    self.removing_category = if self.removing_category.as_deref() == Some(name.as_str()) {
                        None
                    } else {
                        Some(name.clone())
                    };
                }
            });
            if self.renaming_category.as_deref() == Some(name.as_str()) {
                ui.indent(("rename-category", &name), |ui| {
                    ui.horizontal(|ui| {
                        ui.label("to");
                        let field = ui.add(
                            egui::TextEdit::singleline(&mut self.category_to)
                                .hint_text("new name")
                                .desired_width(160.0),
                        );
                        let to = self.category_to.trim().to_lowercase();
                        let ok = !to.is_empty() && to != name && to != vocab::OTHER;
                        let enter = ui.input(|i| i.key_pressed(egui::Key::Enter))
                            && (field.has_focus() || field.lost_focus());
                        if ui.add_enabled(ok, egui::Button::new("Rename")).clicked()
                            || (enter && ok)
                        {
                            *action = Action::RenameCategory(name.clone(), to.clone());
                            self.renaming_category = None;
                        }
                        if self.order.contains(&to) && to != name {
                            ui.small(format!("\"{to}\" exists; this merges \"{name}\" into it."));
                        }
                    });
                });
            }
            if self.removing_category.as_deref() == Some(name.as_str()) {
                ui.indent(("remove-category", &name), |ui| {
                    ui.horizontal(|ui| {
                        ui.label(format!(
                            "Take the category off {}? The tags stay on their files.",
                            Self::tags(n)
                        ));
                        if ui.button("Remove").clicked() {
                            *action = Action::RemoveCategory(name.clone());
                            self.removing_category = None;
                        }
                        if ui.button("Keep it").clicked() {
                            self.removing_category = None;
                        }
                    });
                });
            }
        }
        ui.horizontal(|ui| {
            let field = ui.add(
                egui::TextEdit::singleline(&mut self.add_name)
                    .hint_text("new category…")
                    .desired_width(160.0),
            );
            let name = self.add_name.trim().to_lowercase();
            let ok = !name.is_empty() && name != vocab::OTHER && !self.order.contains(&name);
            let enter = ui.input(|i| i.key_pressed(egui::Key::Enter))
                && (field.has_focus() || field.lost_focus());
            if ui.add_enabled(ok, egui::Button::new("Add")).clicked() || (enter && ok) {
                *action = Action::AddCategory(name);
                self.add_name.clear();
            } else if !name.is_empty() && !ok {
                ui.small("exists already");
            }
        });
    }

    /// One tag's row: name, count, category chip, Rename…, Delete…, and the open form under it.
    #[allow(clippy::too_many_arguments)]
    fn tag_row(
        &mut self,
        ui: &mut egui::Ui,
        c: &Cfg,
        tag: &str,
        n: usize,
        order: &[String],
        rename: &mut Option<(String, String)>,
        delete: &mut Option<String>,
        categories: &mut Vec<(String, Option<String>)>,
    ) {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(tag).strong());
            ui.label(Self::files(n));
            let now = self.vocab.category_of(tag);
            let label = now.as_deref().unwrap_or(vocab::OTHER);
            if Self::derived(tag) {
                let b = widgets::tinted_button(ui, label, widgets::category_fill(&self.vocab, label));
                ui.add_enabled(false, b).on_disabled_hover_text("Follows the tag it is made from");
            } else if let Some(pick) = self.category_menu(ui, tag, now.as_deref(), label, order) {
                categories.push((tag.to_string(), pick));
            }
            if ui
                .selectable_label(self.renaming.as_deref() == Some(tag), "Rename…")
                .on_hover_text("Change this tag in every file, then in the Implications rules; to an existing tag's name merges the two")
                .clicked()
            {
                self.deleting = None;
                self.to.text.clear();
                self.renaming = if self.renaming.as_deref() == Some(tag) {
                    None
                } else {
                    self.to.focus(); // the form opens with the keyboard in its field
                    Some(tag.to_string())
                };
            }
            if ui
                .selectable_label(self.deleting.as_deref() == Some(tag), "Delete…")
                .on_hover_text("Take this tag out of every file")
                .clicked()
            {
                self.renaming = None;
                self.deleting = if self.deleting.as_deref() == Some(tag) {
                    None
                } else {
                    Some(tag.to_string())
                };
            }
        });
        if self.renaming.as_deref() == Some(tag) {
            ui.indent(("rename", tag), |ui| {
                ui.horizontal(|ui| {
                    ui.label("to");
                    if let Some(t) = self.to.show(ui, &self.suggestions, "tags-rename-to", "Rename", &BTreeSet::new()) {
                        let t = c.vocab.canon(&t);
                        if t.is_empty() || t == tag {
                            self.to.text.clear();
                        } else {
                            *rename = Some((tag.to_string(), t));
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
        if self.deleting.as_deref() == Some(tag) {
            ui.indent(("delete", tag), |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!("Take \"{tag}\" out of {}?", Self::files(n)));
                    if ui.button("Open the plan").clicked() {
                        *delete = Some(tag.to_string());
                    }
                    if ui.button("Keep it").clicked() {
                        self.deleting = None;
                    }
                });
            });
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, c: &Cfg) -> Action {
        let max = ctx.content_rect().size() - egui::vec2(40.0, 60.0);
        let mut open = true;
        let mut action = Action::None;
        let mut rename: Option<(String, String)> = None;
        let mut delete: Option<String> = None;
        let mut categories: Vec<(String, Option<String>)> = vec![];
        self.refilter();
        let order = self.order.clone();
        egui::Window::new("Tags")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_size(egui::vec2(760.0, 820.0).min(max))
            .max_size(max)
            .show(ctx, |ui| {
                egui::CollapsingHeader::new("How this works")
                    .default_open(true)
                    .show(ui, |ui| {
                        ui.label(HELP);
                    });
                ui.separator();
                egui::CollapsingHeader::new("Categories")
                    .default_open(true)
                    .show(ui, |ui| self.categories_section(ui, &mut action));
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Find");
                    if self.focus_filter > 0 && !widgets::clicking(ui) {
                        self.focus_filter -= 1;
                        let id = ui.make_persistent_id("tags-filter");
                        if ui.memory(|m| m.had_focus_last_frame(id) && m.has_focus(id)) {
                            self.focus_filter = 0;
                        } else {
                            ui.memory_mut(|m| m.request_focus(id));
                        }
                    }
                    ui.add(
                        egui::TextEdit::singleline(&mut self.filter)
                            // an explicit id: the field's own `id_salt` registers under another id than
                            // make_persistent_id gives, and the focus request then dies on egui's dead-man's
                            // switch (traced 2026-10-07)
                            .id(ui.make_persistent_id("tags-filter"))
                            .desired_width(240.0)
                            .hint_text("part of a tag"),
                    );
                    ui.label(format!("{} tags written by hand", self.counts.len()));
                });
                if let Some(e) = &self.error {
                    ui.colored_label(widgets::ERROR, e);
                }
                ui.separator();
                self.refilter(); // the Find may have changed this frame
                let needle = self.filter.trim().to_lowercase();
                // the bulk action leaves derived names out, so a category is never written against one
                if !needle.is_empty() && !self.shown.is_empty() {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} shown.", self.shown.len()));
                        if let Some(pick) = self.category_menu(
                            ui,
                            "tags-bulk-category",
                            None,
                            "Set all shown to…",
                            &order,
                        ) {
                            categories = self
                                .shown
                                .iter()
                                .map(|&i| self.counts[i].0.clone())
                                .filter(|t| !Self::derived(t))
                                .map(|t| (t, pick.clone()))
                                .collect();
                        }
                    });
                }
                if self.shown.is_empty() {
                    ui.label(if self.counts.is_empty() {
                        "No tags written by hand yet."
                    } else {
                        "Nothing matches."
                    });
                }
                let shown = std::mem::take(&mut self.shown);
                let mut heights = std::mem::take(&mut self.heights);
                egui::ScrollArea::vertical()
                    .id_salt("tags-list")
                    .auto_shrink([false, false])
                    .show_viewport(ui, |ui, viewport| {
                        widgets::rows_in_view(ui, viewport, &mut heights, ROW_GUESS, |ui, i| {
                            let (tag, n) = self.counts[shown[i]].clone();
                            self.tag_row(
                                ui,
                                c,
                                &tag,
                                n,
                                &order,
                                &mut rename,
                                &mut delete,
                                &mut categories,
                            );
                        });
                    });
                self.shown = shown;
                self.heights = heights;
            });
        // Ctrl+S commits whichever of the two is open (wishlist, 2026-09-22), the same as Enter in the field or the button
        if rename.is_none() && delete.is_none() && crate::widgets::chord(ctx, egui::Key::S) {
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
        // a picked colour goes out once it has settled and the button is up; the card keeps the tint live meanwhile
        if let Some((name, rgb, at)) = self.pending_color.clone() {
            if matches!(action, Action::None)
                && at.elapsed() >= COLOR_SETTLE
                && !ctx.input(|i| i.pointer.any_down())
            {
                action = Action::Color(name, Some(rgb));
                self.pending_color = None;
            } else {
                ctx.request_repaint_after(COLOR_SETTLE);
            }
        }
        if !open {
            self.closing = true;
        }
        // a close waits a frame for a colour still settling, so it is not lost
        if self.closing && matches!(action, Action::None) && self.pending_color.is_none() {
            action = Action::Close;
        }
        if matches!(action, Action::Close) {
            self.closing = false;
        }
        if !categories.is_empty() && matches!(action, Action::None) {
            action = Action::Categories(categories);
        }
        action
    }
}

fn load(c: &Cfg) -> Result<(Vec<(String, usize)>, Vec<(String, u64, bool)>), String> {
    let db = Db::open_cfg(c)?;
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
    let db = Db::open_cfg(c)?;
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
    if let Ok(db) = Db::open_cfg(c) {
        let _ = db
            .conn
            .execute("DELETE FROM tag_history WHERE tag=?1", [tag]);
    }
}
