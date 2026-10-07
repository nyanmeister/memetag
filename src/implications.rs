//! The Implications card on the select bar. A rule says "every file tagged A also gets B"; rules live in vocab.toml.
//! Add and remove act at once: the file is rewritten, the index updated (`Db::reimply`, no reindex, no media
//! touched) and the grid pushes the rules to the server. There is no draft to save or discard (asked
//! 2026-10-07: "when I hit Add rule, I want that to be the last step").
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
    ("What Add does", "Writes the rule to vocab.toml, updates the index right away and syncs the rules to the other machines. No reindex, and no file is touched. The × on a rule removes it the same way and takes its implied tags away again. An implied tag cannot be removed from one file; remove the rule instead."),
];

pub struct ImplicationsUi {
    /// The rules as the card shows them: what was on disk when it opened, then what each Add or remove wrote.
    vocab: Vocab,
    /// The rules when the card opened; the ones added since are coloured, so this session's work stands out.
    opened: Vec<(String, String)>,
    from: TagInput,
    to: TagInput,
    from_tags: BTreeSet<String>,
    to_tags: BTreeSet<String>,
    suggestions: Vec<(String, u64, bool)>,
    counts: HashMap<String, usize>,
    error: Option<String>,
    note: Option<String>,
    closing: bool,
}

impl ImplicationsUi {
    /// Opens on the vocabulary as it is on disk, so rules edited by hand since the window started show up too.
    pub fn new(c: &Cfg, rows: &[FileRow]) -> Self {
        let vocab = Self::fresh(c);
        let opened = vocab.rules();
        let mut from = TagInput::default();
        from.focus(); // the card opens ready to type the first tag
        let mut this = Self {
            vocab,
            opened,
            from,
            to: TagInput::default(),
            from_tags: BTreeSet::new(),
            to_tags: BTreeSet::new(),
            suggestions: autocomplete::load(c),
            counts: HashMap::new(),
            error: None,
            note: None,
            closing: false,
        };
        this.refresh(rows);
        this
    }
    /// vocab.toml as it is now. Every edit starts from this rather than from the card's copy, so a hand edit or
    /// a pull since the card opened is kept, never written over.
    fn fresh(c: &Cfg) -> Vocab {
        match &c.vocab.path {
            Some(p) => Vocab::load(p),
            None => c.vocab.clone(),
        }
    }
    /// How many files carry each tag a rule or the new-rule fields mention; one pass over the rows.
    pub fn refresh(&mut self, rows: &[FileRow]) {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for (a, b) in self.vocab.rules() {
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
    /// The vocabulary changed under the card (its own Add came back through the grid, a rename from the Tags card,
    /// a pull from the server): show those rules.
    pub fn follow(&mut self, vocab: &Vocab) {
        self.vocab = vocab.clone();
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
            widgets::tab_chain(ui, &mut [&mut self.from, &mut self.to]);
            let fresh: Vec<String> = from_tags.iter().chain(to_tags.iter()).filter(|t| !self.counts.contains_key(*t)).cloned().collect();
            self.from_tags = from_tags; self.to_tags = to_tags;
            for t in fresh { self.count_one(&t, rows); }
            // Ctrl+S adds too, and first takes a tag still being typed in either field, so the chord needs no Enter before it
            if widgets::chord(ui.ctx(), egui::Key::S) {
                for (input, tags) in [(&mut self.from, &mut self.from_tags), (&mut self.to, &mut self.to_tags)] {
                    let t = c.vocab.canon(input.text.trim());
                    if !t.is_empty() { tags.insert(t); input.text.clear(); }
                }
                add = !self.from_tags.is_empty() && !self.to_tags.is_empty();
            }
            ui.horizontal(|ui| {
                let n = self.from_tags.len() * self.to_tags.len();
                if ui.add_enabled(n > 0, egui::Button::new(if n > 1 { format!("Add {n} rules") } else { "Add rule".to_string() }))
                    .on_hover_text("Write the rule and apply it now: vocab.toml, the index, then the server (Ctrl+S)").clicked() { add = true; }
                if let Some(n) = &self.note { ui.colored_label(widgets::OK, n); }
            });
            if let Some(e) = &self.error { ui.colored_label(widgets::ERROR, e); } // own line, so a long refusal wraps instead of widening the card
            ui.add_space(8.0); ui.separator();
            // Grouped by the specific tag, one row each, its implied tags as chips; the list grows, so it comes last.
            let rules = self.vocab.rules();
            let mut groups: Vec<(&String, Vec<&String>)> = vec![];
            for (a, b) in &rules { match groups.last_mut() { Some((g, v)) if *g == a => v.push(b), _ => groups.push((a, vec![b])) } }
            ui.label(egui::RichText::new(format!("Rules ({} tags imply something, {} rules)", groups.len(), rules.len())).strong());
            if rules.is_empty() { ui.label("None yet."); }
            for (a, bs) in &groups {
                ui.horizontal_wrapped(|ui| {
                    let fresh_a = bs.iter().all(|b| !self.opened.contains(&((*a).clone(), (*b).clone())));
                    ui.label(if fresh_a { egui::RichText::new(a.as_str()).color(widgets::OK) } else { egui::RichText::new(a.as_str()) });
                    ui.label(egui::RichText::new(self.files(a)).weak()).on_hover_text(format!("files that carry \"{a}\" now"));
                    ui.label("⇒");
                    for b in bs {
                        let fresh = !self.opened.contains(&((*a).clone(), (*b).clone()));
                        let text = if fresh { egui::RichText::new(format!("{b} ×")).color(widgets::OK) } else { egui::RichText::new(format!("{b} ×")) };
                        if ui.button(text).on_hover_text(format!("\"{b}\" is on {} now. Click to remove this rule now.", self.files(b))).clicked() { remove = Some(((*a).clone(), (*b).clone())); }
                    }
                });
            }
        });
        if let Some((a, b)) = remove {
            let mut next = Self::fresh(c);
            next.remove_rule(&a, &b);
            action = self.apply(c, next, "Removed");
        }
        if add {
            let mut next = Self::fresh(c);
            let mut errors = vec![];
            let mut added = 0;
            for a in &self.from_tags {
                for b in &self.to_tags {
                    if a == b {
                        errors.push(format!("\"{a}\" cannot imply itself"));
                    } else if next.implies(b, a) {
                        errors.push(format!("\"{b}\" already implies \"{a}\", so this would make them mean the same thing; two spellings of one thing are an alias, and those still live in vocab.toml by hand"));
                    } else if next.implies(a, b) {
                        errors.push(format!(
                            "\"{a}\" already implies \"{b}\", directly or through another rule"
                        ));
                    } else {
                        next.add_rule(a, b);
                        added += 1;
                    }
                }
            }
            let refused = if errors.is_empty() {
                None
            } else {
                Some(format!("Not added: {}.", errors.join("; ")))
            };
            if added > 0 {
                action = self.apply(c, next, if added == 1 { "Added" } else { "Added all" });
                // the fields clear only once the rules are on disk; after a refusal they still hold them for another try
                if matches!(action, Action::Applied { .. }) {
                    self.from_tags.clear();
                    self.to_tags.clear();
                    self.error = refused;
                }
            } else {
                self.note = None;
                self.error = refused;
            }
        }
        if !open || self.closing {
            self.closing = false;
            action = Action::Close;
        }
        action
    }
    /// Write the rules and rebuild the derived rows. The file goes first: if the index update then fails, the next
    /// reindex or Add catches up, and nothing is lost. The card keeps showing the old rules until the write succeeds.
    fn apply(&mut self, c: &Cfg, mut next: Vocab, did: &str) -> Action {
        let written = next.save().and_then(|_| {
            Db::open_cfg(c).and_then(|db| {
                db.reimply(&next)
                    .map_err(|e| format!("vocab.toml saved, but the index was not updated: {e}"))
            })
        });
        match written {
            Ok(changed) => {
                self.vocab = next.clone();
                self.note = Some(match changed.len() {
                    1 => format!("{did}. 1 file changed."),
                    n => format!("{did}. {n} files changed."),
                });
                self.error = None;
                Action::Applied {
                    vocab: next,
                    changed,
                }
            }
            Err(e) => {
                self.error = Some(e);
                Action::None
            }
        }
    }
}
