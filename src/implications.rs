//! The Implications card on the select bar. A rule says "every file tagged A also gets B"; rules live in vocab.toml and
//! Save applies them to the index at once (`Db::reimply`), so no reindex is needed and no file is modified.
use crate::{
    autocomplete,
    index::{Db, FileRow},
    vocab::Vocab,
    widgets::{self, TagInput},
    Cfg,
};
use eframe::egui;
use std::collections::{BTreeSet, HashMap};

pub enum Action {
    None,
    Close,
    Applied {
        vocab: Vocab,
        changed: Vec<(String, BTreeSet<String>)>,
    },
}

const HELP: &[(&str, &str)] = &[
    ("What a rule does", "Every file that has tag A also gets tag B. B is added for you and shows as \"(implied)\" in the editor. A stays on the file."),
    ("Example", "\"twitter post\" ⇒ \"twitter\". Searching twitter then finds every twitter post; searching \"twitter post\" still finds only those."),
    ("Shapes", "One tag can imply several (folder:trump ⇒ person:donald trump, topic:politics). Several tags can imply the same one (reimu and marisa ⇒ series:touhou). Rules chain: A ⇒ B and B ⇒ C gives files with A the tag C too. What does not exist is a rule that needs two tags at once: a rule always fires off one tag."),
    ("When to add one", "Only when B is true for every file that has A, without exception, ever. If you can think of one exception, do not add the rule. A tweet screenshot is not always from twitter, so twitter post ⇒ source:twitter would be wrong."),
    ("What Save does", "Ctrl+S, or the button. Rewrites vocab.toml and updates the index right away. No reindex, and no file is touched. Removing a rule takes its implied tags away again. An implied tag cannot be removed from one file; remove the rule instead."),
];

pub struct ImplicationsUi {
    draft: Vocab,
    saved: Vec<(String, String)>,
    from: TagInput,
    to: TagInput,
    from_tags: BTreeSet<String>,
    to_tags: BTreeSet<String>,
    suggestions: Vec<(String, u64, bool)>,
    counts: HashMap<String, usize>,
    error: Option<String>,
    note: Option<String>,
    closing: bool,
    discard: bool,
}

impl ImplicationsUi {
    /// Opens on the vocabulary as it is on disk, so rules edited by hand since the window started show up too.
    pub fn new(c: &Cfg, rows: &[FileRow]) -> Self {
        let draft = match &c.vocab.path {
            Some(p) => Vocab::load(p),
            None => c.vocab.clone(),
        };
        let saved = draft.rules();
        let mut this = Self {
            draft,
            saved,
            from: TagInput::default(),
            to: TagInput::default(),
            from_tags: BTreeSet::new(),
            to_tags: BTreeSet::new(),
            suggestions: autocomplete::load(c),
            counts: HashMap::new(),
            error: None,
            note: None,
            closing: false,
            discard: false,
        };
        this.refresh(rows);
        this
    }
    /// How many files carry each tag a rule or the new-rule fields mention; one pass over the rows.
    pub fn refresh(&mut self, rows: &[FileRow]) {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for (a, b) in self.draft.rules() {
            counts.insert(a, 0);
            counts.insert(b, 0);
        }
        for t in self.from_tags.iter().chain(self.to_tags.iter()) {
            counts.insert(t.clone(), 0);
        }
        for r in rows {
            for t in &r.tags {
                if let Some(n) = counts.get_mut(t) {
                    *n += 1;
                }
            }
        }
        self.counts = counts;
    }
    fn count_one(&mut self, tag: &str, rows: &[FileRow]) {
        let n = rows.iter().filter(|r| r.tags.contains(tag)).count();
        self.counts.insert(tag.to_string(), n);
    }
    pub fn request_close(&mut self) {
        self.closing = true;
    }
    /// The vocabulary changed under the card (a rename from the Tags card rewrote vocab.toml): a card with no
    /// unsaved work takes the new rules over, so its next Save cannot write the old ones back; one with unsaved
    /// work carries the rename into its draft instead (review, 2026-09-22).
    pub fn follow(&mut self, vocab: &Vocab, rename: Option<(&str, &str)>) {
        if !self.dirty() {
            self.draft = vocab.clone();
        } else if let Some((from, to)) = rename {
            self.draft.rename(from, to);
        }
        self.saved = vocab.rules();
    }
    fn dirty(&self) -> bool {
        self.draft.rules() != self.saved
    }
    fn files(&self, tag: &str) -> String {
        match self.counts.get(tag) {
            Some(1) => "1 file".into(),
            Some(n) => format!("{n} files"),
            None => String::new(),
        }
    }

    pub fn show(&mut self, ctx: &egui::Context, c: &Cfg, rows: &[FileRow]) -> Action {
        let max = ctx.content_rect().size() - egui::vec2(40.0, 60.0);
        let mut open = true;
        let mut action = Action::None;
        let mut remove: Option<(String, String)> = None;
        let mut add = false;
        let mut save = false;
        egui::Window::new("Implications").open(&mut open).collapsible(false).resizable(true).vscroll(true)
            .default_size(egui::vec2(760.0, 640.0).min(max)).max_size(max).show(ctx, |ui| {
            egui::CollapsingHeader::new("How this works").default_open(true).show(ui, |ui| {
                for (head, text) in HELP { ui.label(egui::RichText::new(*head).strong()); ui.label(*text); ui.add_space(4.0); }
            });
            ui.separator();
            ui.label(egui::RichText::new("New rule").strong());
            // Both sides take any number of chips; Add makes one rule per combination, since a rule is always one arrow.
            let chips = |ui: &mut egui::Ui, label: &str, tags: &mut BTreeSet<String>, input: &mut TagInput, id: &str, counts: &HashMap<String, usize>| {
                widgets::chips(ui, label, tags, |t| { let n = counts.get(t).copied().unwrap_or(0); format!("{n} {} carry \"{t}\" today. Click to take it off this rule.", if n == 1 { "file" } else { "files" }) });
                if let Some(t) = input.show(ui, &self.suggestions, id, "Use", tags) { let t = c.vocab.canon(&t); if !t.is_empty() { tags.insert(t); } }
            };
            let (mut from_tags, mut to_tags) = (std::mem::take(&mut self.from_tags), std::mem::take(&mut self.to_tags));
            chips(ui, "Every file tagged", &mut from_tags, &mut self.from, "imply-from", &self.counts);
            chips(ui, "also gets", &mut to_tags, &mut self.to, "imply-to", &self.counts);
            let fresh: Vec<String> = from_tags.iter().chain(to_tags.iter()).filter(|t| !self.counts.contains_key(*t)).cloned().collect();
            self.from_tags = from_tags; self.to_tags = to_tags;
            for t in fresh { self.count_one(&t, rows); }
            ui.horizontal(|ui| {
                let n = self.from_tags.len() * self.to_tags.len();
                if ui.add_enabled(n > 0, egui::Button::new(if n > 1 { format!("Add {n} rules") } else { "Add rule".to_string() })).clicked() { add = true; }
            });
            if let Some(e) = &self.error { ui.colored_label(widgets::ERROR, e); } // own line, so a long refusal wraps instead of widening the card
            ui.add_space(8.0); ui.separator();
            let rules = self.draft.rules();
            let added = rules.iter().filter(|r| !self.saved.contains(r)).count(); let removed = self.saved.iter().filter(|r| !rules.contains(r)).count();
            ui.horizontal(|ui| {
                if ui.add_enabled(self.dirty(), egui::Button::new("Save and apply")).on_hover_text("Write vocab.toml and update the index now (Ctrl+S)").clicked() { save = true; }
                if ui.add_enabled(self.dirty(), egui::Button::new("Discard changes")).clicked() { self.discard = true; }
                if self.dirty() { ui.label(format!("{added} added, {removed} removed since the last save")); }
                else if let Some(n) = &self.note { ui.colored_label(widgets::OK, n); }
            });
            // Ctrl+S saves here as in the editor and the mass edit (consistent save shortcut)
            save |= self.dirty() && widgets::chord(ui.ctx(), egui::Key::S);
            ui.add_space(8.0); ui.separator();
            // Grouped by the specific tag, one row each, its implied tags as chips; the list grows, so it comes last.
            let mut groups: Vec<(&String, Vec<&String>)> = vec![];
            for (a, b) in &rules { match groups.last_mut() { Some((g, v)) if *g == a => v.push(b), _ => groups.push((a, vec![b])) } }
            ui.label(egui::RichText::new(format!("Rules ({} tags imply something, {} rules)", groups.len(), rules.len())).strong());
            if rules.is_empty() { ui.label("None yet."); }
            for (a, bs) in &groups {
                ui.horizontal_wrapped(|ui| {
                    let fresh_a = bs.iter().all(|b| !self.saved.contains(&((*a).clone(), (*b).clone())));
                    ui.label(if fresh_a { egui::RichText::new(a.as_str()).color(widgets::OK) } else { egui::RichText::new(a.as_str()) });
                    ui.label(egui::RichText::new(self.files(a)).weak()).on_hover_text(format!("files that carry \"{a}\" now"));
                    ui.label("⇒");
                    for b in bs {
                        let fresh = !self.saved.contains(&((*a).clone(), (*b).clone()));
                        let text = if fresh { egui::RichText::new(format!("{b} ×")).color(widgets::OK) } else { egui::RichText::new(format!("{b} ×")) };
                        if ui.button(text).on_hover_text(format!("\"{b}\" is on {} now. Click to remove this rule (takes effect on Save).", self.files(b))).clicked() { remove = Some(((*a).clone(), (*b).clone())); }
                    }
                });
            }
        });
        if self.discard {
            // a modal, so the question is seen whatever the card is scrolled to
            egui::Modal::new(egui::Id::new("implications-discard")).show(ctx, |ui| {
                ui.label("Throw away the unsaved rules?");
                ui.horizontal(|ui| {
                    if ui.button("Throw away").clicked() {
                        self.draft = Vocab {
                            implications: HashMap::new(),
                            ..self.draft.clone()
                        };
                        for (a, b) in &self.saved {
                            self.draft.add_rule(a, b);
                        }
                        self.discard = false;
                        self.error = None;
                        if self.closing {
                            action = Action::Close;
                        }
                    }
                    if ui.button("Keep editing").clicked() {
                        self.discard = false;
                        self.closing = false;
                    }
                });
            });
        }
        if let Some((a, b)) = remove {
            self.draft.remove_rule(&a, &b);
            self.error = None;
        }
        if add {
            let mut errors = vec![];
            for a in std::mem::take(&mut self.from_tags) {
                for b in &self.to_tags {
                    if a == *b {
                        errors.push(format!("\"{a}\" cannot imply itself"));
                    } else if self.draft.implies(b, &a) {
                        errors.push(format!("\"{b}\" already implies \"{a}\", so this would make them mean the same thing; two spellings of one thing are an alias, and those still live in vocab.toml by hand"));
                    } else if self.draft.implies(&a, b) {
                        errors.push(format!(
                            "\"{a}\" already implies \"{b}\", directly or through another rule"
                        ));
                    } else {
                        self.draft.add_rule(&a, b);
                    }
                }
            }
            self.to_tags.clear();
            self.note = None;
            self.error = if errors.is_empty() {
                None
            } else {
                Some(format!("Not added: {}.", errors.join("; ")))
            };
        }
        if save {
            match self.apply(c) {
                Ok((vocab, changed)) => {
                    self.saved = vocab.rules();
                    self.note = Some(format!("Saved. {} files changed.", changed.len()));
                    self.error = None;
                    action = Action::Applied { vocab, changed };
                }
                Err(e) => self.error = Some(e),
            }
        }
        if !open {
            self.closing = true;
        }
        if self.closing && !matches!(action, Action::Close) {
            if self.dirty() {
                self.discard = true;
            } else {
                action = Action::Close;
            }
        }
        if matches!(action, Action::Close) {
            self.closing = false;
        }
        action
    }
    /// Write the rules and rebuild the derived rows. The file goes first: if the index update then fails, the next
    /// reindex or a second Save catches up, and nothing is lost.
    fn apply(&mut self, c: &Cfg) -> Result<(Vocab, Vec<(String, BTreeSet<String>)>), String> {
        self.draft.save()?;
        let db = Db::open_cfg(&c)?;
        let changed = db
            .reimply(&self.draft)
            .map_err(|e| format!("vocab.toml saved, but the index was not updated: {e}"))?;
        Ok((self.draft.clone(), changed))
    }
}
