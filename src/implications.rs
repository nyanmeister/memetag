//! The Implications card on the select bar. A rule says "every file tagged A also gets B"; rules live in vocab.toml.
//! Since 0.3.6 the same card writes **suggestions** ("a file tagged A may also have B": offered in the editor,
//! accepted or denied per file, nothing derived) — one kind switch rather than a third card (asked 2026-10-31).
//! Add and remove act at once: the file is rewritten, the index updated (`Db::reimply`, no reindex, no media
//! touched) and the grid pushes the rules to the server. There is no draft to save or discard (asked
//! 2026-10-07: "when I hit Add rule, I want that to be the last step").
use crate::{
    autocomplete,
    index::FileRow,
    vocab::Vocab,
    widgets::{self, TagInput},
    Cfg,
};
use eframe::egui;
use std::collections::{BTreeSet, HashMap, HashSet};

pub enum Action {
    None,
    Close,
    Applied {
        /// boxed: a Vocab is three maps, and the enum is passed around every frame
        vocab: Box<Vocab>,
        changed: Vec<(String, BTreeSet<String>)>,
    },
}

const HELP: &[(&str, &str)] = &[
    ("What a rule does", "Every file that has tag A also gets tag B. B is added for you and shows as \"(implied)\" in the editor. A stays on the file."),
    ("Example", "\"twitter post\" ⇒ \"twitter\". Searching twitter then finds every twitter post; searching \"twitter post\" still finds only those."),
    ("Shapes", "One tag can imply several (folder:trump ⇒ person:donald trump, topic:politics). Several tags can imply the same one (reimu and marisa ⇒ series:touhou). Rules chain: A ⇒ B and B ⇒ C gives files with A the tag C too. What does not exist is a rule that needs two tags at once: a rule always fires off one tag."),
    ("When to add one", "Only when B is true for every file that has A, without exception, ever. If you can think of one exception, do not add the rule. A tweet screenshot is not always from twitter, so twitter post ⇒ source:twitter would be wrong."),
    ("Suggestions", "The other kind of rule: \"a file tagged A may also have B\". Nothing is derived; the editor lists B under Suggested whenever a file has A (directly, by folder or by implication) and you answer per file: accept adds B, deny writes not:B so \"has none\" is kept as data, and an unanswered one is offered again next time. For things usually but not always true: catgirl → tail, succubus → horns, wings."),
    ("What Add does", "Writes the rule to vocab.toml, updates the index right away and syncs the rules to the other machines. No reindex, and no file is touched. The × on a rule removes it the same way and takes its implied tags away again. An implied tag cannot be removed from one file; remove the rule instead."),
];

/// Which kind of rule the New rule fields write.
/// One specific tag's rules as a row: `bs` are its targets, each with whether it was added since the card opened.
struct Group {
    a: String,
    fresh_a: bool,
    bs: Vec<(String, bool)>,
}
/// Sorted (a, b) pairs into rows by a; `opened` says which pairs were there when the card opened.
fn group(rules: Vec<(String, String)>, opened: &HashSet<(String, String)>) -> Vec<Group> {
    let mut out: Vec<Group> = vec![];
    for (a, b) in rules {
        let fresh = !opened.contains(&(a.clone(), b.clone()));
        match out.last_mut() {
            Some(g) if g.a == a => {
                g.fresh_a &= fresh;
                g.bs.push((b, fresh));
            }
            _ => out.push(Group {
                a,
                fresh_a: fresh,
                bs: vec![(b, fresh)],
            }),
        }
    }
    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Implies,
    Suggests,
}

pub struct ImplicationsUi {
    /// The rules as the card shows them: what was on disk when it opened, then what each Add or remove wrote.
    vocab: Vocab,
    /// The rules when the card opened; the ones added since are coloured, so this session's work stands out.
    opened: HashSet<(String, String)>,
    opened_suggestions: HashSet<(String, String)>,
    /// The rules grouped by their specific tag, as the list draws them: made when the vocabulary changes, not per
    /// frame (`vocab.rules()` sorts every rule, and a `Vec::contains` per chip made the old draw quadratic in the
    /// rule count: 715 rules cost most of a core per frame on 2026-11-05).
    groups: Vec<Group>,
    sgroups: Vec<Group>,
    /// Each rule group's height as last drawn, for `widgets::rows_in_view`.
    heights: Vec<f32>,
    kind: Kind,
    from: TagInput,
    to: TagInput,
    from_tags: BTreeSet<String>,
    to_tags: BTreeSet<String>,
    suggestions: Vec<(String, u64, bool)>,
    /// The suggestions being read from the index when the card opens.
    load: widgets::Pending<Vec<(String, u64, bool)>>,
    counts: HashMap<String, usize>,
    error: Option<String>,
    note: Option<String>,
}

impl ImplicationsUi {
    /// Opens on the vocabulary as it is on disk, so rules edited by hand since the window started show up too.
    pub fn new(c: &Cfg, rows: &[FileRow], ctx: &egui::Context) -> Self {
        let vocab = c.vocab_on_disk();
        let opened = vocab.rules().into_iter().collect();
        let opened_suggestions = vocab.suggestion_rules().into_iter().collect();
        let mut from = TagInput::default();
        from.focus(); // the card opens ready to type the first tag
        let mut this = Self {
            vocab,
            opened,
            opened_suggestions,
            groups: vec![],
            sgroups: vec![],
            heights: vec![],
            kind: Kind::Implies,
            from,
            to: TagInput::default(),
            from_tags: BTreeSet::new(),
            to_tags: BTreeSet::new(),
            suggestions: vec![],
            load: {
                let c = c.clone();
                widgets::Pending::spawn(ctx, move || Ok(autocomplete::load(&c)))
            },
            counts: HashMap::new(),
            error: None,
            note: None,
        };
        this.refresh(rows);
        this.regroup();
        this
    }
    /// The two lists as drawn, from the vocabulary as it is now.
    fn regroup(&mut self) {
        self.groups = group(self.vocab.rules(), &self.opened);
        self.sgroups = group(self.vocab.suggestion_rules(), &self.opened_suggestions);
        self.heights = vec![0.0; self.groups.len()];
    }
    /// How many files carry each tag a rule or the new-rule fields mention; one pass over the rows.
    pub fn refresh(&mut self, rows: &[FileRow]) {
        let mut counts: HashMap<String, usize> = HashMap::new();
        for (a, b) in self
            .vocab
            .rules()
            .into_iter()
            .chain(self.vocab.suggestion_rules())
        {
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
    /// The vocabulary changed under the card (its own Add came back through the grid, a rename from the Tags card,
    /// a pull from the server): show those rules.
    pub fn follow(&mut self, vocab: &Vocab) {
        self.vocab = vocab.clone();
        self.regroup();
    }
    fn files(&self, tag: &str) -> String {
        self.counts
            .get(tag)
            .map_or(String::new(), |n| widgets::count(*n, "file"))
    }
    /// One group's row: the specific tag, its file count, the arrow, and a chip per rule that removes it. Rules
    /// added since the card opened are coloured. Returns the (a, b) of a clicked chip.
    fn rule_row(&self, ui: &mut egui::Ui, i: usize, suggestion: bool) -> Option<(String, String)> {
        let g = if suggestion {
            &self.sgroups[i]
        } else {
            &self.groups[i]
        };
        let mut clicked = None;
        ui.horizontal_wrapped(|ui| {
            let a = g.a.as_str();
            ui.label(if g.fresh_a {
                egui::RichText::new(a).color(widgets::OK)
            } else {
                egui::RichText::new(a)
            });
            ui.label(egui::RichText::new(self.files(a)).weak())
                .on_hover_text(format!("files that carry \"{a}\" now"));
            if suggestion {
                ui.label("?")
                    .on_hover_text("suggests: offered in the editor, answered per file");
            } else {
                ui.label("⇒");
            }
            for (b, fresh) in &g.bs {
                let text = if *fresh {
                    egui::RichText::new(format!("{b} ×")).color(widgets::OK)
                } else {
                    egui::RichText::new(format!("{b} ×"))
                };
                let what = if suggestion { "suggestion" } else { "rule" };
                if ui
                    .button(text)
                    .on_hover_text(format!(
                        "\"{b}\" is on {} now. Click to remove this {what} now.",
                        self.files(b)
                    ))
                    .clicked()
                {
                    clicked = Some((g.a.clone(), b.clone()));
                }
            }
        });
        clicked
    }

    pub fn show(&mut self, ctx: &egui::Context, c: &Cfg, rows: &[FileRow]) -> Action {
        if let Some(Ok(s)) = self.load.poll(ctx, "suggestions") {
            self.suggestions = s;
        }
        let mut open = true;
        let mut action = Action::None;
        let mut remove: Option<(String, String)> = None;
        let mut remove_suggestion: Option<(String, String)> = None;
        let mut add = false;
        widgets::card(
            ctx,
            "Implications and suggestions",
            [760.0, 820.0],
            false,
            &mut open,
            |ui| {
                egui::CollapsingHeader::new("How this works")
                    .default_open(true)
                    .show(ui, |ui| {
                        for (head, text) in HELP {
                            ui.label(egui::RichText::new(*head).strong());
                            ui.label(*text);
                            ui.add_space(4.0);
                        }
                    });
                ui.separator();
                ui.horizontal(|ui| {
                ui.label(egui::RichText::new("New rule").strong());
                ui.radio_value(&mut self.kind, Kind::Implies, "implies").on_hover_text("Always true: B is added to every file that has A, now and on every later file");
                ui.radio_value(&mut self.kind, Kind::Suggests, "suggests").on_hover_text("Often true: the editor offers B on files that have A; you accept or deny it per file");
            });
                let suggests = self.kind == Kind::Suggests;
                // Both sides take any number of chips; Add makes one rule per combination, since a rule is always one arrow.
                let chips = |ui: &mut egui::Ui,
                             label: &str,
                             tags: &mut BTreeSet<String>,
                             input: &mut TagInput,
                             id: &str,
                             counts: &HashMap<String, usize>| {
                    widgets::chips(ui, label, tags, |t| {
                        let n = counts.get(t).copied().unwrap_or(0);
                        format!(
                            "{n} {} carry \"{t}\" today. Click to take it off this rule.",
                            if n == 1 { "file" } else { "files" }
                        )
                    });
                    if let Some(t) = input.show(ui, &self.suggestions, id, "Use", tags) {
                        let t = c.vocab.canon(&t);
                        if !t.is_empty() {
                            tags.insert(t);
                        }
                    }
                };
                let (mut from_tags, mut to_tags) = (
                    std::mem::take(&mut self.from_tags),
                    std::mem::take(&mut self.to_tags),
                );
                chips(
                    ui,
                    "Every file tagged",
                    &mut from_tags,
                    &mut self.from,
                    "imply-from",
                    &self.counts,
                );
                chips(
                    ui,
                    if suggests {
                        "may also have"
                    } else {
                        "also gets"
                    },
                    &mut to_tags,
                    &mut self.to,
                    "imply-to",
                    &self.counts,
                );
                widgets::tab_chain(ui, &mut [&mut self.from, &mut self.to]);
                let fresh: Vec<String> = from_tags
                    .iter()
                    .chain(to_tags.iter())
                    .filter(|t| !self.counts.contains_key(*t))
                    .cloned()
                    .collect();
                self.from_tags = from_tags;
                self.to_tags = to_tags;
                for t in fresh {
                    self.count_one(&t, rows);
                }
                // Ctrl+S adds too, and first takes a tag still being typed in either field, so the chord needs no Enter before it
                if widgets::chord(ui.ctx(), egui::Key::S) {
                    for (input, tags) in [
                        (&mut self.from, &mut self.from_tags),
                        (&mut self.to, &mut self.to_tags),
                    ] {
                        let t = c.vocab.canon(input.text.trim());
                        if !t.is_empty() {
                            tags.insert(t);
                            input.text.clear();
                        }
                    }
                    add = !self.from_tags.is_empty() && !self.to_tags.is_empty();
                }
                ui.horizontal(|ui| {
                let n = self.from_tags.len() * self.to_tags.len();
                if ui.add_enabled(n > 0, egui::Button::new(if n > 1 { format!("Add {n} rules") } else { "Add rule".to_string() }))
                    .on_hover_text(if suggests { "Write the suggestion now: vocab.toml, then the server (Ctrl+S)" } else { "Write the rule and apply it now: vocab.toml, the index, then the server (Ctrl+S)" }).clicked() { add = true; }
                if let Some(n) = &self.note { ui.colored_label(widgets::OK, n); }
            });
                if let Some(e) = &self.error {
                    ui.colored_label(widgets::ERROR, e);
                } // own line, so a long refusal wraps instead of widening the card
                ui.add_space(8.0);
                ui.separator();
                // Grouped by the specific tag, one row each, its implied tags as chips; the list grows, so it comes last,
                // in its own scroll with only the rows in view drawn. The suggestions follow in the same scroll.
                let rules: usize = self.groups.iter().map(|g| g.bs.len()).sum();
                let srules: usize = self.sgroups.iter().map(|g| g.bs.len()).sum();
                ui.label(
                    egui::RichText::new(format!(
                        "Rules ({} tags imply something, {} rules)",
                        self.groups.len(),
                        rules
                    ))
                    .strong(),
                );
                if rules == 0 {
                    ui.label("None yet.");
                }
                let mut heights = std::mem::take(&mut self.heights);
                egui::ScrollArea::vertical()
                    .id_salt("rules")
                    .auto_shrink([false, false])
                    .show_viewport(ui, |ui, viewport| {
                        widgets::rows_in_view(ui, viewport, &mut heights, 22.0, |ui, i| {
                            if let Some((a, b)) = self.rule_row(ui, i, false) {
                                remove = Some((a, b));
                            }
                        });
                        ui.add_space(8.0);
                        ui.label(
                            egui::RichText::new(format!(
                                "Suggestions ({} tags suggest something, {} rules)",
                                self.sgroups.len(),
                                srules
                            ))
                            .strong(),
                        );
                        if srules == 0 {
                            ui.label("None yet.");
                        }
                        for i in 0..self.sgroups.len() {
                            if let Some((a, b)) = self.rule_row(ui, i, true) {
                                remove_suggestion = Some((a, b));
                            }
                        }
                    });
                self.heights = heights;
            },
        );
        if let Some((a, b)) = remove_suggestion {
            let mut next = c.vocab_on_disk();
            next.remove_suggestion(&a, &b);
            action = self.apply(c, next, "Removed");
        }
        if let Some((a, b)) = remove {
            let mut next = c.vocab_on_disk();
            next.remove_rule(&a, &b);
            action = self.apply(c, next, "Removed");
        }
        if add {
            let mut next = c.vocab_on_disk();
            let mut errors = vec![];
            let mut added = 0;
            for a in &self.from_tags {
                for b in &self.to_tags {
                    if self.kind == Kind::Suggests {
                        if a == b {
                            errors.push(format!("\"{a}\" cannot suggest itself"));
                        } else if b.starts_with(crate::vocab::NOT) {
                            errors.push(format!(
                                "\"{b}\" is a denial; suggest the tag itself and deny it per file"
                            ));
                        } else if next.implies(a, b) {
                            errors.push(format!("\"{a}\" already implies \"{b}\", so every such file has it; nothing to suggest"));
                        } else if next.suggests_directly(a, b) {
                            errors.push(format!("\"{a}\" already suggests \"{b}\""));
                        } else {
                            next.add_suggestion(a, b);
                            added += 1;
                        }
                        continue;
                    }
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
        if !open {
            action = Action::Close;
        }
        action
    }
    /// Write the rules and rebuild the derived rows. The file goes first: if the index update then fails, the next
    /// reindex or Add catches up, and nothing is lost. The card keeps showing the old rules until the write succeeds.
    fn apply(&mut self, c: &Cfg, mut next: Vocab, did: &str) -> Action {
        match crate::index::save_rules(c, &mut next) {
            Ok(changed) => {
                self.vocab = next.clone();
                self.regroup();
                self.note = Some(match changed.len() {
                    0 => format!("{did}."), // a suggestion: nothing derived, no file changes
                    1 => format!("{did}. 1 file changed."),
                    n => format!("{did}. {n} files changed."),
                });
                self.error = None;
                Action::Applied {
                    vocab: Box::new(next),
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
