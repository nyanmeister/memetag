//! Shared GUI pieces and the conventions every card follows. Read this before adding a card or a control.
//!
//! - **Two kinds of card.** Reference and rule cards (Search syntax, OCR, Implications) are floating `egui::Window`s
//!   over the grid, closed by their × or Escape. Edits (the editor, the batch plan) replace the whole panel, since they
//!   hold state the grid must not touch until they close, and they return an `Action` the grid folds in.
//! - **Escape, front to back.** One press closes the topmost thing only: an open suggestion popup, then the frontmost
//!   card, then the window. Cards with unsaved work turn the close into a discard prompt instead. `App::escape` owns
//!   the order; a card never reads Escape itself except to clear its own text field or to put away its own discard
//!   prompt. Every reader goes through `escape`, which drops key repeats: a held key closes one thing, not the window
//!   behind it.
//! - **Outcomes and errors.** Something that finished (a copy, a save, a pull) is a grid toast, three seconds, `OK` green.
//!   Something that failed stays in red in the card that caused it, until the next attempt clears it.
//! - **Tags are typed once, through `TagInput`,** and shown as `chips`. Up/Down choose a suggestion, Tab takes it,
//!   Enter adds the typed text, a comma ends the tag as it does in the search bar (the text after it stays in the
//!   field), Escape clears the field. A click on a suggestion takes it too.
//! - **Colours mean one thing each,** below. Anything else uses the theme's own visuals.
use crate::autocomplete;
use eframe::egui;
use std::collections::BTreeSet;

/// A finished outcome (toast, saved note, a rule not yet saved).
pub const OK: egui::Color32 = egui::Color32::LIGHT_GREEN;
/// A failure, in the card that caused it.
pub const ERROR: egui::Color32 = egui::Color32::LIGHT_RED;
/// A selected tile.
pub const SELECTED: egui::Color32 = egui::Color32::from_rgb(80, 190, 255);
/// A tile the OCR engine has not read yet (amber pill).
pub const OCR_PENDING: egui::Color32 = egui::Color32::from_rgb(255, 190, 60);
/// A tile with no hand-written tags, while the tagging assistant is on (orange outline).
pub const UNTAGGED: egui::Color32 = egui::Color32::from_rgb(255, 140, 0);
/// Tagging assistant: a tile with hand-written tags, but fewer than the assistant's threshold.
pub const THIN: egui::Color32 = egui::Color32::from_rgb(235, 210, 60);

/// Keyboard contract for a list of suggestions under a text field: Up/Down move through `len` of them, Tab takes the
/// current one. Only while `field` has focus and no modifier is held, so Shift+Tab and Ctrl+arrows keep their meaning.
pub fn choose(ui: &egui::Ui, field: egui::Id, len: usize, selected: &mut usize) -> Option<usize> {
    if len == 0 || !ui.memory(|m| m.has_focus(field)) {
        return None;
    }
    *selected = (*selected).min(len - 1);
    let mut chosen = None;
    ui.input_mut(|i| {
        if i.modifiers != egui::Modifiers::NONE {
            return;
        }
        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown) {
            *selected = (*selected + 1) % len;
        }
        if i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp) {
            *selected = (*selected + len - 1) % len;
        }
        if i.consume_key(egui::Modifiers::NONE, egui::Key::Tab) {
            chosen = Some(*selected);
        }
    });
    chosen
}

/// Tags as buttons that take themselves off the set; `label` leads the row when not empty; `hover` is each chip's tooltip.
pub fn chips(
    ui: &mut egui::Ui,
    label: &str,
    tags: &mut BTreeSet<String>,
    hover: impl Fn(&str) -> String,
) {
    let mut drop = None;
    ui.horizontal_wrapped(|ui| {
        if !label.is_empty() {
            ui.label(label);
        }
        for t in tags.iter() {
            let r = ui.button(format!("{t} ×"));
            let h = hover(t);
            let r = if h.is_empty() { r } else { r.on_hover_text(h) };
            if r.clicked() {
                drop = Some(t.clone());
            }
        }
    });
    if let Some(t) = drop {
        tags.remove(&t);
    }
}

/// A one-line tag field with the local autocomplete under it; Enter, Tab, a comma, a click on a suggestion or the
/// button hand the tag back (not canonicalised: the caller applies its vocabulary). A comma ends the tag the way it
/// does in a search, so a tag can never contain one (the search delimiter); what follows the comma stays in the
/// field. Escape with text in the field clears it, so a half-typed tag never leaks into the card's own Escape.
#[derive(Default)]
pub struct TagInput {
    pub text: String,
    selected: usize,
    primary: crate::xsel::Primary,
}
/// Autocomplete hits with the ones already in the field moved to the end and flagged `true`. A tag you have keeps its
/// place in the list but sits below the ones you don't, dimmed. Stable within each
/// group, so the autocomplete ranking survives the split.
fn order(matches: Vec<String>, present: &BTreeSet<String>) -> Vec<(String, bool)> {
    let (absent, here): (Vec<String>, Vec<String>) =
        matches.into_iter().partition(|t| !present.contains(t));
    absent
        .into_iter()
        .map(|t| (t, false))
        .chain(here.into_iter().map(|t| (t, true)))
        .collect()
}

impl TagInput {
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        suggestions: &[(String, u64, bool)],
        id: &str,
        button: &str,
        present: &BTreeSet<String>,
    ) -> Option<String> {
        let field = ui.make_persistent_id(id);
        let before = order(autocomplete::matches(suggestions, &self.text), present);
        let mut chosen =
            choose(ui, field, before.len(), &mut self.selected).map(|k| before[k].0.clone());
        // egui drops focus at the start of the frame Escape arrives in, so "had it last frame" is the test that sees it
        if ui.is_enabled()
            && !self.text.is_empty()
            && ui.memory(|m| m.has_focus(field) || m.had_focus_last_frame(field))
            && escape(ui.ctx())
        {
            self.text.clear();
            self.selected = 0;
            ui.memory_mut(|m| m.request_focus(field));
        }
        // Text after a comma, kept in the field once the part before it is handed back.
        let mut rest: Option<String> = None;
        ui.horizontal(|ui| {
            let mut out = egui::TextEdit::singleline(&mut self.text)
                .id(field)
                .lock_focus(!before.is_empty())
                .hint_text("Type a tag…")
                .desired_width(350.0)
                .show(ui);
            let pasted = self.primary.sync(ui, &mut out, &mut self.text, false);
            let response = out.response;
            if response.changed() || pasted {
                self.selected = 0;
            }
            if let Some(k) = self.text.find(',') {
                let head = self.text[..k].trim().to_string();
                let tail = self.text[k + 1..].trim_start().to_string();
                if head.is_empty() {
                    self.text = tail;
                } else {
                    chosen = Some(head);
                    rest = Some(tail);
                }
                self.selected = 0;
            }
            let pressed = ui.button(button).clicked();
            // a plain Enter only: Ctrl+Enter belongs to the card (save, apply) and must not commit a half-typed tag
            // on its way there (review, 2026-09-22)
            let entered = (response.has_focus() || response.lost_focus())
                && ui.input(|i| i.modifiers.is_none() && i.key_pressed(egui::Key::Enter));
            if chosen.is_none() && (pressed || entered) && !self.text.trim().is_empty() {
                chosen = Some(self.text.clone());
            }
        });
        let hits = order(autocomplete::matches(suggestions, &self.text), present);
        if !hits.is_empty() {
            ui.small(
                "Up/Down choose · Tab accepts · Enter or a comma adds what you typed · Esc clears",
            );
        }
        for (i, (tag, here)) in hits.iter().enumerate() {
            // a tag already in the field is dimmed and marked, so it reads as "you have this" rather than a fresh offer
            let label = if *here {
                egui::RichText::new(format!("{tag}  ✓")).color(ui.visuals().weak_text_color())
            } else {
                egui::RichText::new(tag.as_str())
            };
            let mut resp = ui.selectable_label(i == self.selected, label);
            if *here {
                resp = resp.on_hover_text("already in this field");
            }
            if resp.clicked() {
                chosen = Some(tag.clone());
            }
        }
        if chosen.is_some() {
            self.text = rest.unwrap_or_default();
            self.selected = 0;
            ui.memory_mut(|m| m.request_focus(field));
        }
        chosen
    }
}

/// The one reader of Escape. Consumes every plain Escape press of this frame and says whether any was a real press
/// rather than a key repeat. Autorepeat outruns the cards (especially with fast repeat settings), so a
/// held Escape used to close the editor with the first repeat and the whole window with the second (2026-09-22).
pub fn escape(ctx: &egui::Context) -> bool {
    ctx.input_mut(|i| {
        let mut pressed = false;
        i.events.retain(|e| match e {
            egui::Event::Key {
                key: egui::Key::Escape,
                pressed: true,
                repeat,
                modifiers,
                ..
            } if modifiers.is_none() => {
                pressed |= !repeat;
                false
            }
            _ => true,
        });
        pressed
    })
}

/// The one reader of a Ctrl+`key` chord (Ctrl+S, Ctrl+Enter): consumes this frame's presses of it and says whether
/// any was a real press rather than a key repeat. Read with `key_pressed`, one press reached every open card at
/// once, and a held Ctrl+Enter saved-and-nexted one file per repeat (review, 2026-09-22).
pub fn chord(ctx: &egui::Context, key: egui::Key) -> bool {
    ctx.input_mut(|i| {
        let mut pressed = false;
        i.events.retain(|e| match e {
            egui::Event::Key {
                key: k,
                pressed: true,
                repeat,
                modifiers,
                ..
            } if *k == key && modifiers.command && !modifiers.shift && !modifiers.alt => {
                pressed |= !repeat;
                false
            }
            _ => true,
        });
        pressed
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn present_tags_sink_to_the_bottom_flagged() {
        let present: BTreeSet<String> = ["b", "d"].iter().map(|s| s.to_string()).collect();
        let got = order(
            vec!["a".into(), "b".into(), "c".into(), "d".into()],
            &present,
        );
        assert_eq!(
            got,
            vec![
                ("a".into(), false),
                ("c".into(), false), // the ones you don't have keep their order, first
                ("b".into(), true),
                ("d".into(), true), // the ones you have follow, in their order, flagged
            ]
        );
        // empty present is a no-op: nothing flagged, order untouched
        let none = BTreeSet::new();
        assert_eq!(
            order(vec!["x".into(), "y".into()], &none),
            vec![("x".into(), false), ("y".into(), false)]
        );
    }

    fn key(pressed: bool, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key: egui::Key::Escape,
            physical_key: None,
            pressed,
            repeat: false, // egui works the flag out from its own key state
            modifiers,
        }
    }

    #[test]
    fn a_chord_is_consumed_once_and_repeats_do_not_count() {
        let cmd = egui::Modifiers::COMMAND;
        let s = |pressed: bool| egui::Event::Key {
            key: egui::Key::S,
            physical_key: None,
            pressed,
            repeat: false,
            modifiers: cmd,
        };
        let ctx = egui::Context::default();
        frame(&ctx, vec![s(true)]);
        assert!(chord(&ctx, egui::Key::S), "a press");
        assert!(
            !chord(&ctx, egui::Key::S),
            "consumed: a second card sees nothing"
        );
        ctx.end_pass().textures_delta.clear();
        frame(&ctx, vec![s(true)]);
        assert!(!chord(&ctx, egui::Key::S), "still held: a repeat");
        ctx.end_pass().textures_delta.clear();
        frame(&ctx, vec![s(false), s(true)]);
        assert!(chord(&ctx, egui::Key::S), "released and pressed again");
        assert!(!chord(&ctx, egui::Key::Enter), "another key is untouched");
        ctx.end_pass().textures_delta.clear();
    }

    fn frame(ctx: &egui::Context, events: Vec<egui::Event>) {
        ctx.begin_pass(egui::RawInput {
            events,
            ..Default::default()
        });
    }

    #[test]
    fn escape_counts_presses_not_repeats() {
        let none = egui::Modifiers::NONE;
        let ctx = egui::Context::default();
        frame(&ctx, vec![key(true, none), key(true, none)]);
        assert!(escape(&ctx), "a press counts, with a repeat beside it");
        assert!(!escape(&ctx), "both were consumed");
        ctx.end_pass().textures_delta.clear();
        frame(&ctx, vec![key(true, none)]);
        assert!(!escape(&ctx), "still held: a repeat is not a press");
        ctx.end_pass().textures_delta.clear();
        frame(&ctx, vec![key(false, none), key(true, none)]);
        assert!(escape(&ctx), "released and pressed again");
        ctx.end_pass().textures_delta.clear();
        frame(
            &ctx,
            vec![key(false, none), key(true, egui::Modifiers::SHIFT)],
        );
        assert!(!escape(&ctx), "a modified Escape is someone else's");
        assert_eq!(ctx.input(|i| i.events.len()), 2, "and is left in place");
        ctx.end_pass().textures_delta.clear();
    }
}
