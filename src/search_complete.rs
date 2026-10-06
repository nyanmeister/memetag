//! Local search-bar completion. Syntax reference/credit is in query::quote_tag.
use crate::{autocomplete, index::FileRow, query, widgets, Cfg};
use eframe::egui;

pub struct SearchComplete {
    remembered: Vec<(String, u64, bool)>,
    values: Vec<(String, u64, bool)>,
    selected: usize,
    caret: usize,
    signature: (String, usize),
    suppressed: Option<(String, usize)>,
    open: bool,
    was_open: bool,
    primary: crate::xsel::Primary,
}
impl SearchComplete {
    pub fn new(c: &Cfg, rows: &[FileRow]) -> Self {
        let remembered = autocomplete::load(c)
            .into_iter()
            .filter(|(_, _, own)| *own)
            .collect();
        let mut this = Self {
            remembered,
            values: vec![],
            selected: 0,
            caret: 0,
            signature: (String::new(), 0),
            suppressed: None,
            open: false,
            was_open: false,
            primary: Default::default(),
        };
        this.refresh(rows);
        this
    }
    pub fn refresh(&mut self, rows: &[FileRow]) {
        self.values = autocomplete::local_counts(rows, &self.remembered);
    }
    /// Escape while the suggestions show: keep them closed until the query changes. Returns whether there was anything
    /// to dismiss, so the grid knows the key is spent. Judged on the frame's entry state: egui drops the field's focus
    /// when Escape arrives, so by the time the grid asks, `show` has already closed the popup on its own.
    pub fn dismiss(&mut self, q: &str) -> bool {
        if !self.open && !self.was_open {
            return false;
        }
        self.suppressed = Some((q.into(), self.caret));
        self.open = false;
        true
    }
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        c: &Cfg,
        q: &mut String,
        first: &mut bool,
    ) -> (bool, bool) {
        let id = ui.make_persistent_id("search-query");
        self.was_open = self.open;
        let before = q.clone();
        let context = query::completion(q, self.caret);
        let hits = context
            .as_ref()
            .map(|(_, prefix)| autocomplete::matches(&self.values, prefix))
            .unwrap_or_default();
        let mut chosen = if self.open {
            widgets::choose(ui, id, hits.len(), &mut self.selected).map(|k| hits[k].clone())
        } else {
            None
        };
        let mut output = egui::TextEdit::singleline(q)
            .id(id)
            .lock_focus(self.open)
            .desired_width(f32::INFINITY)
            .hint_text("tags or t:caption · Tab completes tags · comma adds another tag")
            .show(ui);
        self.primary.sync(ui, &mut output, q, false); // highlight owns PRIMARY, middle-click pastes it
        let response = &output.response;
        if *first || ui.memory(|m| m.focused().is_none()) {
            response.request_focus();
            *first = false;
        }
        let range = output.cursor_range;
        self.caret = range
            .map(|r| usize::from(r.primary.index))
            .unwrap_or_else(|| q.chars().count());
        let signature = (q.clone(), self.caret);
        if self.signature != signature {
            self.selected = 0;
            self.suppressed = None;
            self.signature = signature.clone();
        }
        let current = query::completion(q, self.caret);
        let hits = current
            .as_ref()
            .map(|(_, prefix)| autocomplete::matches(&self.values, prefix))
            .unwrap_or_default();
        let focused = response.has_focus() || response.lost_focus();
        self.open = focused
            && range.is_some_and(|r| r.is_empty())
            && self.suppressed.as_ref() != Some(&signature)
            && !hits.is_empty();
        self.selected = self.selected.min(hits.len().saturating_sub(1));
        let copy_first = focused && ui.input(|i| i.key_pressed(egui::Key::Enter));
        egui::Popup::from_response(response)
            .open(self.open)
            .width(response.rect.width().min(500.0))
            .show(|ui| {
                ui.small("Used tags · Up/Down choose · Tab completes · Esc dismisses");
                for (i, tag) in hits.iter().enumerate() {
                    if ui.selectable_label(i == self.selected, tag).clicked() {
                        chosen = Some(tag.clone());
                    }
                }
            });
        if let (Some(tag), Some((span, _))) = (chosen, current) {
            let replacement = query::quote_tag(&tag);
            let caret = q[..span.start].chars().count() + replacement.chars().count();
            q.replace_range(span, &replacement);
            output
                .state
                .cursor
                .set_char_range(Some(egui::text::CCursorRange::one(
                    egui::text::CCursor::new(caret),
                )));
            output.state.store(ui.ctx(), id);
            response.request_focus();
            self.caret = caret;
            self.signature = (q.clone(), caret);
            self.suppressed = Some(self.signature.clone());
            self.open = false;
            let db_path = c.db.clone();
            std::thread::spawn(move || {
                if let Ok(db) = crate::index::Db::open(&db_path) {
                    let _ = db.conn.execute("INSERT INTO tag_history(tag,uses) VALUES(?1,1) ON CONFLICT(tag) DO UPDATE SET uses=uses+1", [tag]);
                }
            });
        } else if response.lost_focus() {
            self.suppressed = Some(signature);
            self.open = false;
        }
        (before != *q, copy_first)
    }
}
