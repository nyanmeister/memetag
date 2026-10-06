//! The resolve card for a vocab conflict: this machine and the server both changed the alias/implication rules since
//! the last sync, so the automatic sync stopped rather than overwrite one with the other (vocabsync's conflict guard).
//! The card shows what differs and offers three choices: keep mine, take the server's, or merge
//! both. Merge is the union of every rule; the one thing a union cannot settle on its own — an alias both sides map to
//! *different* targets — is a per-alias pick here, defaulting to the local target so nothing is decided behind the user.
//!
//! Every button hands the work back to the grid, which runs the ssh + reimply on a thread and folds the result in; the
//! card stays open and disabled ("Working…") until that lands, so a slow server cannot freeze the window.
use crate::vocab::Vocab;
use crate::vocabsync::{self, AliasClash, Conflict};
use eframe::egui;
use std::collections::BTreeSet;

pub enum Action {
    None,
    Close,
    Resolve(Resolution),
}

/// Which way to reconcile. `Merge` carries the picked target for each clashing alias key.
#[derive(Clone)]
pub enum Resolution {
    Mine,
    Theirs,
    Merge(Vec<(String, String)>),
}

/// What a resolution produced, for the grid to fold in. `fold` is the new rules and the index rows `reimply` changed
/// when the local file was replaced (take-theirs, merge); `None` when only the server moved (keep-mine).
pub struct Resolved {
    pub fold: Option<(Vocab, Vec<(String, BTreeSet<String>)>)>,
    pub message: String,
}

pub struct VocabConflictUi {
    conflict: Conflict,
    clashes: Vec<AliasClash>,
    /// per clash: true = keep the local target, false = take the server's; defaults to keep-mine
    keep_local: Vec<bool>,
    working: bool,
}

impl VocabConflictUi {
    pub fn new(conflict: Conflict) -> Self {
        let (_, clashes) = vocabsync::merge(&conflict.local, &conflict.canon);
        let keep_local = vec![true; clashes.len()];
        Self {
            conflict,
            clashes,
            keep_local,
            working: false,
        }
    }
    /// A newer conflict arrived (another pull found a different divergence): take it over, unless a resolution is
    /// already in flight — that thread will finish and close the card.
    pub fn update(&mut self, conflict: Conflict) {
        if !self.working {
            *self = Self::new(conflict);
        }
    }
    /// The alias target chosen for each clash, from the radio state: local when kept, the server's when not.
    fn merge_picks(&self) -> Vec<(String, String)> {
        self.clashes
            .iter()
            .enumerate()
            .map(|(i, cl)| {
                let target = if self.keep_local[i] {
                    cl.local.clone()
                } else {
                    cl.canon.clone()
                };
                (cl.key.clone(), target)
            })
            .collect()
    }

    pub fn show(&mut self, ctx: &egui::Context) -> Action {
        let max = ctx.content_rect().size() - egui::vec2(40.0, 60.0);
        let mut open = true;
        let mut action = Action::None;
        egui::Window::new("Rules differ from the server")
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .vscroll(true)
            .default_size(egui::vec2(620.0, 520.0).min(max))
            .max_size(max)
            .show(ctx, |ui| {
                ui.label(
                    "Your alias/implication rules and the server's both changed since the last sync. Nothing has been \
                     written on either side. Choose how to reconcile them.",
                );
                ui.add_space(6.0);
                let d = &self.conflict.diff;
                if !d.only_local.is_empty() {
                    ui.label(egui::RichText::new("Only here").strong());
                    for l in &d.only_local {
                        ui.label(format!("    {l}"));
                    }
                }
                if !d.only_canon.is_empty() {
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("Only on the server").strong());
                    for l in &d.only_canon {
                        ui.label(format!("    {l}"));
                    }
                }
                if !self.clashes.is_empty() {
                    ui.add_space(8.0);
                    ui.separator();
                    ui.label(
                        egui::RichText::new(
                            "If you merge, these aliases are defined differently on each side — pick one of each:",
                        )
                        .strong(),
                    );
                    for (i, cl) in self.clashes.iter().enumerate() {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(format!("{} =", cl.key));
                            ui.selectable_value(
                                &mut self.keep_local[i],
                                true,
                                format!("{} (here)", cl.local),
                            );
                            ui.selectable_value(
                                &mut self.keep_local[i],
                                false,
                                format!("{} (server)", cl.canon),
                            );
                        });
                    }
                }
                ui.add_space(8.0);
                ui.separator();
                ui.add_enabled_ui(!self.working, |ui| {
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .button("Keep mine")
                            .on_hover_text("Push your rules; the server's differing rules are dropped")
                            .clicked()
                        {
                            action = Action::Resolve(Resolution::Mine);
                        }
                        if ui
                            .button("Take server's")
                            .on_hover_text("Replace your rules with the server's; your differing rules are dropped")
                            .clicked()
                        {
                            action = Action::Resolve(Resolution::Theirs);
                        }
                        let label = if self.clashes.is_empty() {
                            "Merge both"
                        } else {
                            "Merge with these picks"
                        };
                        if ui
                            .button(label)
                            .on_hover_text("Keep every rule from both sides")
                            .clicked()
                        {
                            action = Action::Resolve(Resolution::Merge(self.merge_picks()));
                        }
                    });
                    if self.working {
                        ui.add_space(4.0);
                        ui.label("Working…");
                    }
                });
            });
        if let Action::Resolve(_) = &action {
            self.working = true; // keep the card up, disabled, until the grid's thread reports back
        }
        // the close box only takes when nothing is in flight, so a resolution is never abandoned mid-write
        if !open && !self.working && matches!(action, Action::None) {
            action = Action::Close;
        }
        action
    }
}

/// Run a resolution: the ssh write, then — when the local file was replaced — reload the rules and rebuild the derived
/// index rows. Called on the grid's worker thread, never the UI thread.
pub fn resolve(
    store: &dyn vocabsync::Canonical,
    env: &vocabsync::Env,
    db_path: &std::path::Path,
    resolution: Resolution,
) -> Result<Resolved, String> {
    match resolution {
        Resolution::Mine => {
            vocabsync::take_mine(store, env)?;
            Ok(Resolved {
                fold: None,
                message: "Kept your rules; the server now matches".into(),
            })
        }
        Resolution::Theirs => {
            vocabsync::take_theirs(store, env)?;
            let (vocab, changed) = reapply(env, db_path)?;
            Ok(Resolved {
                message: format!("Took the server's rules; {} files changed", changed.len()),
                fold: Some((vocab, changed)),
            })
        }
        Resolution::Merge(picks) => {
            let left = vocabsync::resolve_merge(store, env, &picks)?;
            let (vocab, changed) = reapply(env, db_path)?;
            let note = if left.is_empty() {
                String::new()
            } else {
                format!(" ({} alias clash(es) kept local)", left.len())
            };
            Ok(Resolved {
                message: format!(
                    "Merged both rule sets; {} files changed{note}",
                    changed.len()
                ),
                fold: Some((vocab, changed)),
            })
        }
    }
}

fn reapply(
    env: &vocabsync::Env,
    db_path: &std::path::Path,
) -> Result<(Vocab, Vec<(String, BTreeSet<String>)>), String> {
    let vocab = Vocab::load(&env.vocab_path);
    let changed = crate::index::Db::open(db_path)?.reimply(&vocab)?;
    Ok((vocab, changed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conflict() -> Conflict {
        let local: Vocab = toml::from_str(
            "[aliases]\n\"pg\" = \"peter griffin\"\n[implications]\n\"reimu\" = [\"touhou\"]\n",
        )
        .unwrap();
        let canon: Vocab = toml::from_str(
            "[aliases]\n\"pg\" = \"peter griffon\"\n[implications]\n\"marisa\" = [\"touhou\"]\n",
        )
        .unwrap();
        let diff = vocabsync::diff(&local, &canon);
        Conflict { local, canon, diff }
    }

    #[test]
    fn picks_follow_the_radios() {
        let mut card = VocabConflictUi::new(conflict());
        assert_eq!(card.clashes.len(), 1, "\"pg\" is the one clash");
        // default keeps the local target
        assert_eq!(
            card.merge_picks(),
            vec![("pg".to_string(), "peter griffin".to_string())]
        );
        // flip the radio to the server's target
        card.keep_local[0] = false;
        assert_eq!(
            card.merge_picks(),
            vec![("pg".to_string(), "peter griffon".to_string())]
        );
    }

    #[test]
    fn card_renders_headless_without_panicking() {
        // one egui frame with no backend: exercises the layout/widget calls, so an id clash or a bad closure shows up
        // in tests as well as in a running window. With clashes (the busier path) and after a resolve press.
        let ctx = egui::Context::default();
        let mut card = VocabConflictUi::new(conflict());
        ctx.begin_pass(egui::RawInput::default());
        let _ = card.show(&ctx);
        ctx.end_pass().textures_delta.clear();
        // simulate a resolve having been dispatched: the card must stay up and disabled, not close
        card.working = true;
        ctx.begin_pass(egui::RawInput::default());
        let _ = card.show(&ctx);
        ctx.end_pass().textures_delta.clear();
        assert!(card.working);
    }
}
