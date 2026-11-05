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
/// current one. Only while `field` has focus and no modifier is
/// held, so Shift+Tab and Ctrl+arrows keep their meaning.
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

/// The dark tint a category's chips wear: the vocabulary's `[colors]` entry or the built-in one
/// (`Vocab::color_of`); a category with neither, or none at all, keeps the plain button fill (`None`).
pub fn category_fill(vocab: &crate::vocab::Vocab, category: &str) -> Option<egui::Color32> {
    vocab
        .color_of(category)
        .map(|c| egui::Color32::from_rgb(c[0], c[1], c[2]))
}

/// WCAG relative luminance of a colour, 0 (black) to 1 (white): sRGB channels linearised, then the
/// luminance weights.
pub fn luminance(c: egui::Color32) -> f32 {
    let lin = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}
/// The text colours (strong, weak) that read on a fill (asked 2026-11-05: "if the category color is too bright
/// for white text … make the text black instead, which lets us use white as a color"). Black beats white once
/// the fill's luminance passes 0.179, where the two WCAG contrast ratios, (L+0.05)/0.05 and 1.05/(L+0.05), are
/// equal; `None` (no tint) keeps the theme's own text colours.
pub fn text_on(ui: &egui::Ui, fill: Option<egui::Color32>) -> (egui::Color32, egui::Color32) {
    match fill {
        Some(f) if luminance(f) > 0.179 => (egui::Color32::BLACK, egui::Color32::from_gray(70)),
        Some(_) => (egui::Color32::from_gray(235), egui::Color32::from_gray(185)),
        None => (
            ui.visuals().widgets.inactive.text_color(),
            ui.visuals().weak_text_color(),
        ),
    }
}
/// A button in a category's tint, its text black or light as the tint needs; no tint, a plain button.
pub fn tinted_button(
    ui: &egui::Ui,
    label: &str,
    fill: Option<egui::Color32>,
) -> egui::Button<'static> {
    let (strong, _) = text_on(ui, fill);
    let mut b = egui::Button::new(egui::RichText::new(label).color(strong));
    if let Some(f) = fill {
        b = b.fill(f);
    }
    b
}

/// Draw only the rows of a long list that are in view, inside `ScrollArea::show_viewport`. Every row costs its
/// widgets each frame in egui, and a card listing the whole vocabulary (3,551 tags on 2026-11-05) spent most of a
/// core redrawing rows nobody could see. `heights[i]` is row i's height as last drawn (0 = never drawn yet, taken
/// as the mean of the drawn ones, or `guess` before any is); rows above and below the view become one blank
/// space each, so the scroll bar still spans the whole list. Rows draw under their own index as id, so a widget's
/// state (an open menu, a focused field) stays with its row whatever is scrolled past.
pub fn rows_in_view(
    ui: &mut egui::Ui,
    viewport: egui::Rect,
    heights: &mut [f32],
    guess: f32,
    mut row: impl FnMut(&mut egui::Ui, usize),
) {
    let spacing = ui.spacing().item_spacing.y;
    let (sum, n) = heights
        .iter()
        .filter(|h| **h > 0.0)
        .fold((0.0, 0usize), |(s, n), h| (s + h, n + 1));
    let typical = if n > 0 { sum / n as f32 } else { guess };
    let mut y = 0.0;
    let mut skipped = 0.0;
    for (i, height) in heights.iter_mut().enumerate() {
        let h = if *height > 0.0 { *height } else { typical };
        if y + h < viewport.min.y || y > viewport.max.y {
            skipped += h + spacing;
        } else {
            if skipped > 0.0 {
                ui.add_space(skipped - spacing);
                skipped = 0.0;
            }
            let drawn = ui.push_id(i, |ui| row(ui, i)).response.rect.height();
            if drawn > 0.0 {
                *height = drawn;
            }
        }
        y += h + spacing;
    }
    if skipped > 0.0 {
        ui.add_space(skipped - spacing);
    }
}

/// One category's row of the editor: `show` are the tags of the file in this row (chips with ×, tinted with
/// `fill`), `dim` the inherited ones shown after them without a ×.
pub struct ChipRow<'a> {
    pub label: &'a str,
    pub show: &'a [String],
    pub dim: &'a [String],
    pub fill: Option<egui::Color32>,
    /// The tint of any tag by its own category: a relational chip's modifier half wears its modifier's.
    pub tint_of: &'a dyn Fn(&str) -> Option<egui::Color32>,
}
/// What a chip was clicked with.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChipClick {
    Primary,
    Secondary,
}
/// Draw a `ChipRow` and say which chip was clicked, with which button; the caller decides what that means
/// (removal, or the relation gesture). `armed` is the chip drawn as selected, the modifier waiting for its target.
pub fn chip_row(
    ui: &mut egui::Ui,
    row: &ChipRow,
    armed: Option<&str>,
    hover: impl Fn(&str) -> String,
) -> Option<(String, ChipClick)> {
    let mut out = None;
    ui.horizontal_wrapped(|ui| {
        if !row.label.is_empty() {
            ui.add_sized(
                [76.0, 18.0],
                egui::Label::new(egui::RichText::new(row.label).weak()).halign(egui::Align::LEFT),
            )
            .on_hover_text("Click a tag to remove it. Right-click one to relate it to another tag on this file; right-click the target next; left-click or Escape cancels. Categories are set in the Tags card.");
        }
        for t in row.show {
            let r = match crate::vocab::relation(t) {
                // a relation wears both categories: the modifier's tint, then the target's (mockup 2026-11-05, B)
                Some((m, target)) => {
                    let plain = ui.visuals().widgets.inactive.bg_fill;
                    let left = (row.tint_of)(m).unwrap_or(plain);
                    let right = row.fill.unwrap_or(plain);
                    split_chip(ui, m, &format!("‹{target}› ×"), left, right, armed == Some(t.as_str()))
                }
                None => {
                    let mut b = egui::Button::new(chip_text(ui, t, row.fill)).selected(armed == Some(t.as_str()));
                    if let Some(f) = row.fill {
                        b = b.fill(f);
                    }
                    ui.add(b)
                }
            };
            let h = hover(t);
            let r = if h.is_empty() { r } else { r.on_hover_text(h) };
            if r.clicked() {
                out = Some((t.clone(), ChipClick::Primary));
            } else if r.secondary_clicked() {
                out = Some((t.clone(), ChipClick::Secondary));
            }
        }
        for t in row.dim {
            ui.add_enabled(false, egui::Button::new(t.as_str()));
        }
    });
    out
}

/// A relational chip, `modifier ‹target› ×` as one pill split at the seam between the words: the modifier's half
/// in its category's tint, the target's in its own, each with the text colour its fill needs (mockup 2026-11-05,
/// option B: "each word on its own tint"; asked with it: the pill fits the text and the words sit as close as a
/// plain chip's). Sized and padded like a `Button`, so it lines up with the chips beside it.
fn split_chip(
    ui: &mut egui::Ui,
    modifier: &str,
    rest: &str,
    left: egui::Color32,
    right: egui::Color32,
    selected: bool,
) -> egui::Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let (left_text, _) = text_on(ui, Some(left));
    let (right_text, _) = text_on(ui, Some(right));
    let lg = ui
        .painter()
        .layout_no_wrap(modifier.to_string(), font.clone(), left_text);
    let rg = ui
        .painter()
        .layout_no_wrap(rest.to_string(), font, right_text);
    let pad = ui.spacing().button_padding;
    let seam = 2.5; // half a space each side of the seam: the words as close as "albino ‹beach›" in one chip
    let lw = pad.x + lg.size().x + seam;
    let rw = seam + rg.size().x + pad.x;
    let h = (lg.size().y.max(rg.size().y) + 2.0 * pad.y).max(ui.spacing().interact_size.y);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(lw + rw, h), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let visuals = ui.style().interact_selectable(&resp, selected);
        let cr = visuals.corner_radius;
        let p = ui.painter();
        let seam_x = rect.min.x + lw;
        let left_rect = egui::Rect::from_min_max(rect.min, egui::pos2(seam_x, rect.max.y));
        let right_rect = egui::Rect::from_min_max(egui::pos2(seam_x, rect.min.y), rect.max);
        p.with_clip_rect(left_rect.intersect(p.clip_rect()))
            .rect_filled(rect, cr, left);
        p.with_clip_rect(right_rect.intersect(p.clip_rect()))
            .rect_filled(rect, cr, right);
        if selected || resp.hovered() {
            p.rect_stroke(rect, cr, visuals.bg_stroke, egui::StrokeKind::Inside);
        }
        let y = |g: &egui::Galley| rect.center().y - g.size().y / 2.0;
        p.galley(
            egui::pos2(rect.min.x + pad.x, y(&lg)),
            lg.clone(),
            left_text,
        );
        p.galley(egui::pos2(seam_x + seam, y(&rg)), rg.clone(), right_text);
    }
    resp
}

/// A plain chip's label, `tag ×`, in the text colour its fill needs (a relational tag goes through `split_chip`).
fn chip_text(ui: &egui::Ui, tag: &str, fill: Option<egui::Color32>) -> egui::text::LayoutJob {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let (strong, _) = text_on(ui, fill);
    let mut job = egui::text::LayoutJob::default();
    job.append(
        &format!("{tag} ×"),
        0.0,
        egui::TextFormat::simple(font, strong),
    );
    job
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
    /// Set by `focus`: frames left in which `show` asks for the keyboard. Frames carrying the click that opened
    /// the card do not count: egui's per-widget interaction in such a frame sees a click away from the focused
    /// field and surrenders the focus again (traced 2026-10-07 on the Tags card; the Implications card only
    /// escaped by timing). A new Window's first pass is also a sizing pass, so one request is never enough.
    focus_frames: u8,
    /// The field's widget id once shown, so `tab_chain` can hand it the keyboard.
    field: Option<egui::Id>,
    /// Tab (or Shift+Tab) pressed while this field was empty; read by `tab_chain`.
    tab: Option<Tab>,
}
/// A pointer press, click or release is in this frame's input: a focus request made now is undone by egui's own
/// "clicked elsewhere" rule, so `TagInput::focus` and the Tags card's Find field wait for the next frame.
pub fn clicking(ui: &egui::Ui) -> bool {
    ui.input(|i| i.pointer.any_pressed() || i.pointer.any_click() || i.pointer.any_released())
}
/// Which way a Tab press in a `TagInput` wants to go.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    Next,
    Previous,
}
/// Tab in an empty field moves the keyboard to the next one, Shift+Tab to the previous, wrapping at the ends
/// (asked 2026-10-07: "have the tab key go to the next text field"). Without this egui's Tab lands on the
/// field's own button first. Call after every field in the chain has been shown this frame.
pub fn tab_chain(ui: &egui::Ui, inputs: &mut [&mut TagInput]) {
    let n = inputs.len();
    for i in 0..n {
        let Some(dir) = inputs[i].tab() else { continue };
        let j = match dir {
            Tab::Next => (i + 1) % n,
            Tab::Previous => (i + n - 1) % n,
        };
        if let Some(id) = inputs[j].field {
            ui.memory_mut(|m| m.request_focus(id));
        }
        return;
    }
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
    /// Give the field the keyboard the next time it is shown, so a card opens ready to type into (asked
    /// 2026-10-07, for the editor: one click less per file).
    pub fn focus(&mut self) {
        self.focus_frames = 4;
    }
    /// A Tab pressed while this field was empty; taken once. A caller with a single field
    /// hands the keyboard on by itself, several fields go through `tab_chain`.
    pub fn tab(&mut self) -> Option<Tab> {
        self.tab.take()
    }
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        suggestions: &[(String, u64, bool)],
        id: &str,
        button: &str,
        present: &BTreeSet<String>,
    ) -> Option<String> {
        let field = ui.make_persistent_id(id);
        if self.focus_frames > 0 && !clicking(ui) {
            self.focus_frames -= 1;
            if ui.memory(|m| m.had_focus_last_frame(field) && m.has_focus(field)) {
                self.focus_frames = 0; // it took
            } else {
                ui.memory_mut(|m| m.request_focus(field));
            }
        }
        let before = order(autocomplete::matches(suggestions, &self.text), present);
        let empty = self.text.trim().is_empty();
        let mut chosen = if ui.is_enabled() && !empty {
            choose(ui, field, before.len(), &mut self.selected).map(|k| before[k].0.clone())
        } else {
            None
        };
        // Tab completes or commits text and stays here, for stacking several tags. Only a field that was empty
        // before this key press hands focus on; clearing it after a completion must not also move focus.
        // Consume navigation before TextEdit sees it; lock_focus keeps egui's own focus walk out of it.
        self.tab = None;
        self.field = Some(field);
        if chosen.is_none() && ui.is_enabled() && ui.memory(|m| m.has_focus(field)) {
            ui.input_mut(|i| {
                if i.consume_key(egui::Modifiers::SHIFT, egui::Key::Tab) {
                    if empty {
                        self.tab = Some(Tab::Previous);
                    }
                } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Tab) {
                    if empty {
                        self.tab = Some(Tab::Next);
                    } else {
                        chosen = Some(self.text.clone());
                    }
                }
            });
        }
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
                .lock_focus(true) // Tab is ours (a suggestion, or the next field), never egui's focus walk
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
                "Up/Down choose · Tab takes it · Enter or a comma adds what you typed · Empty field: Tab moves on · Esc clears",
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

    fn tag_frame(
        ctx: &egui::Context,
        inputs: &mut [TagInput; 2],
        mut events: Vec<egui::Event>,
    ) -> [Option<String>; 2] {
        let modifiers = events
            .iter()
            .find_map(|event| match event {
                egui::Event::Key { modifiers, .. } => Some(*modifiers),
                _ => None,
            })
            .unwrap_or_default();
        events.insert(0, egui::Event::ModifiersChanged(modifiers));
        frame(ctx, events);
        let mut tags = [None, None];
        egui::Area::new(egui::Id::new("tag-test")).show(ctx, |ui| {
            for (i, input) in inputs.iter_mut().enumerate() {
                tags[i] = input.show(
                    ui,
                    &[("cat".into(), 1, false)],
                    &format!("tag-{i}"),
                    "Use",
                    &BTreeSet::new(),
                );
            }
            let [first, second] = inputs;
            tab_chain(ui, &mut [first, second]);
        });
        ctx.end_pass().textures_delta.clear();
        tags
    }

    fn tag_key(key: egui::Key, modifiers: egui::Modifiers) -> egui::Event {
        egui::Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn tag_completion_and_literal_commits_keep_focus() {
        for (text, key, expected) in [
            ("ca", egui::Key::Tab, "cat"),
            ("new-tag", egui::Key::Tab, "new-tag"),
            ("ca", egui::Key::Enter, "ca"),
        ] {
            let ctx = egui::Context::default();
            let mut inputs = [TagInput::default(), TagInput::default()];
            inputs[0].focus();
            for _ in 0..5 {
                tag_frame(&ctx, &mut inputs, vec![]);
            }
            let field = inputs[0].field.unwrap();
            inputs[0].text = text.into();
            let tags = tag_frame(&ctx, &mut inputs, vec![tag_key(key, egui::Modifiers::NONE)]);
            assert_eq!(tags, [Some(expected.into()), None]);
            assert!(inputs[0].text.is_empty());
            assert!(ctx.memory(|m| m.has_focus(field)), "{text}: {key:?}");
            tag_frame(&ctx, &mut inputs, vec![]);
            assert!(ctx.memory(|m| m.has_focus(field)), "focus persists");
        }
    }

    #[test]
    fn tag_tab_navigation_requires_an_empty_field() {
        for (text, modifiers, moves) in [
            ("", egui::Modifiers::NONE, true),
            ("", egui::Modifiers::SHIFT, true),
            ("  ", egui::Modifiers::NONE, true),
            ("ca", egui::Modifiers::SHIFT, false),
            ("new-tag", egui::Modifiers::SHIFT, false),
        ] {
            let ctx = egui::Context::default();
            let mut inputs = [TagInput::default(), TagInput::default()];
            inputs[0].focus();
            for _ in 0..5 {
                tag_frame(&ctx, &mut inputs, vec![]);
            }
            inputs[0].text = text.into();
            let tags = tag_frame(&ctx, &mut inputs, vec![tag_key(egui::Key::Tab, modifiers)]);
            assert_eq!(tags, [None, None]);
            assert_eq!(inputs[0].text, text);
            let field = inputs[usize::from(moves)].field.unwrap();
            assert!(ctx.memory(|m| m.has_focus(field)), "{text}: {modifiers:?}");
        }
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
    #[test]
    fn text_goes_black_once_the_fill_is_bright() {
        use egui::Color32 as C;
        // Expected by hand (sRGB linearised, WCAG weights): white 1.0, black 0.0; the character tint #2f5a31
        // is 0.081; the pink picked on 2026-11-05, #cb8d8d, is 0.337; grey 117 is 0.178 and grey 118 is 0.181,
        // either side of the 0.179 line where black and white text have the same contrast ratio
        assert!((luminance(C::WHITE) - 1.0).abs() < 1e-4);
        assert!(luminance(C::BLACK).abs() < 1e-6);
        assert!((luminance(C::from_rgb(47, 90, 49)) - 0.081).abs() < 0.002);
        assert!((luminance(C::from_rgb(203, 141, 141)) - 0.337).abs() < 0.002);
        let ctx = egui::Context::default();
        ctx.begin_pass(Default::default());
        let ui = &mut egui::Ui::new(
            ctx.clone(),
            egui::Id::new("tint-test"),
            egui::UiBuilder::new(),
        );
        {
            let black = |fill| text_on(ui, Some(fill)).0 == C::BLACK;
            assert!(black(C::WHITE));
            assert!(black(C::from_rgb(203, 141, 141)));
            assert!(black(C::from_gray(118)));
            assert!(!black(C::from_gray(117)));
            assert!(!black(C::from_rgb(47, 90, 49)));
            assert!(!black(C::BLACK));
            assert_eq!(
                text_on(ui, None).0,
                ui.visuals().widgets.inactive.text_color(),
                "no tint: the theme's own"
            );
        }
        ctx.end_pass().textures_delta.clear();
    }
}
