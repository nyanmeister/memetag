//! X11 selection conventions for text fields (middle-click and highlight copy/paste).
//! Highlighting text in a field owns PRIMARY with it, so a middle-click in any other
//! program pastes it; a middle-click in a field pastes PRIMARY at the pointer. CLIPBOARD (Ctrl+C / Ctrl+V) is untouched.
use eframe::egui::{self, text_edit::TextEditOutput};

/// One per view: only the focused field can hold a selection, so a single tracker serves all its fields.
#[derive(Default)]
pub struct Primary {
    last: String,
}

impl Primary {
    /// Call after a TextEdit's `show`, with the field's text. Returns true when a middle-click pasted into it
    /// (the text changed, the caret sits after the pasted text, and `out.cursor_range` says so).
    pub fn sync(
        &mut self,
        ui: &egui::Ui,
        out: &mut TextEditOutput,
        text: &mut String,
        multiline: bool,
    ) -> bool {
        // highlight → PRIMARY, once the pointer is up (a drag in progress would spawn an owner per frame)
        match out.cursor_range.filter(|r| !r.is_empty()) {
            Some(r) => {
                let sel = r.slice_str(text).to_string();
                if sel != self.last
                    && !ui.input(|i| i.pointer.any_down())
                    && crate::clipboard::own_text("PRIMARY", &sel).is_ok()
                {
                    self.last = sel;
                }
            }
            None => self.last.clear(),
        }
        if !out.response.clicked_by(egui::PointerButton::Middle) {
            return false;
        }
        let pasted = match crate::clipboard::read_selection("PRIMARY") {
            Ok(s) if !s.is_empty() => s,
            _ => return false,
        };
        let pasted = if multiline {
            pasted
        } else {
            pasted.replace(['\n', '\r'], " ")
        };
        let pos = ui
            .input(|i| i.pointer.interact_pos())
            .unwrap_or(out.response.rect.left_center());
        let at = usize::from(out.galley.cursor_from_pos(pos - out.galley_pos).index)
            .min(text.chars().count());
        let byte = text
            .char_indices()
            .nth(at)
            .map(|(b, _)| b)
            .unwrap_or(text.len());
        text.insert_str(byte, &pasted);
        let caret =
            egui::text::CCursorRange::one(egui::text::CCursor::new(at + pasted.chars().count()));
        out.state.cursor.set_char_range(Some(caret));
        out.state.clone().store(ui.ctx(), out.response.id);
        out.response.request_focus();
        out.cursor_range = Some(caret);
        true
    }
}
