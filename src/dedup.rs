//! The Duplicates card on the select bar: the current search results grouped by look-alikes (`similar::groups`, complete
//! linkage at the default distance), each group a row of tiles side by side. It is a viewing and selecting tool:
//! left-click toggles a tile's selection, "Select group" takes the whole row, and the select bar's Edit tags then works
//! on them; right-click opens the original; "Show in grid" turns the group into a `similar:` search. memetag deletes
//! nothing, so what to do with a duplicate stays a decision made elsewhere with the paths in hand.
//!
//! The grouping runs on a thread (the grid sends the hashes of its hits, the card gets the groups back as paths) and the
//! card maps paths to row indices itself, again after every pull, so a row that moved or went keeps nothing stale.
use crate::index::FileRow;
use crate::widgets;
use eframe::egui;
use std::collections::{HashMap, HashSet};

pub enum Action {
    None,
    Close,
    Toggle(usize),
    Select(Vec<usize>),
    Open(usize),
    Search(String),
}

const PAGE: usize = 30;

pub struct DedupUi {
    /// Groups as paths (biggest first), and the same as row indices, rebuilt by `remap`.
    groups: Vec<Vec<String>>,
    rows: Vec<Vec<usize>>,
    /// How many results were grouped, or None while the thread still runs.
    scope: Option<usize>,
    page: usize,
}

impl DedupUi {
    pub fn new() -> Self {
        Self {
            groups: vec![],
            rows: vec![],
            scope: None,
            page: 0,
        }
    }
    pub fn waiting(&self) -> bool {
        self.scope.is_none()
    }
    /// The thread's answer: groups among `scope` results.
    pub fn set(&mut self, groups: Vec<Vec<String>>, scope: usize, rows: &[FileRow]) {
        self.groups = groups;
        self.scope = Some(scope);
        self.page = 0;
        self.remap(rows);
    }
    pub fn remap(&mut self, rows: &[FileRow]) {
        let index = crate::index::by_path(rows);
        self.rows = self
            .groups
            .iter()
            .map(|g| {
                g.iter()
                    .filter_map(|p| index.get(p.as_str()).copied())
                    .collect()
            })
            .collect();
    }
    /// Row indices the card draws this frame (the current page), so the grid can fetch just those textures.
    pub fn visible(&self) -> Vec<usize> {
        self.rows
            .iter()
            .skip(self.page * PAGE)
            .take(PAGE)
            .flatten()
            .copied()
            .collect()
    }

    pub fn show(
        &mut self,
        ctx: &egui::Context,
        rows: &[FileRow],
        selected: &HashSet<usize>,
        // texture, the size of one frame, and how many frames the texture holds side by side (a storyboard strip)
        textures: &HashMap<usize, widgets::Texture>,
    ) -> Action {
        let mut open = true;
        let mut action = Action::None;
        widgets::card(ctx, "Duplicates", [1000.0, 700.0], true, &mut open, |ui| {
            let Some(scope) = self.scope else {
                ui.label("Finding look-alikes among the search results…");
                ui.spinner();
                return;
            };
            let grouped: usize = self.groups.iter().map(Vec::len).sum();
            ui.label(format!("{} groups holding {grouped} of the {scope} files in this search; every file in a group looks like every other in it (within {} of 256 hash bits). Narrow the search to narrow this.",
                             self.groups.len(), crate::similar::DEFAULT_DISTANCE));
            ui.small("Left-click a tile to select it, right-click to open the original; Edit tags on the select bar then works on the selection. Nothing here deletes.");
            let groups = self.groups.len();
            if groups > PAGE {
                ui.horizontal(|ui| widgets::pager(ui, &mut self.page, groups, PAGE, "groups "));
            }
            ui.separator();
            if self.groups.is_empty() {
                ui.label("No two files in this search look alike.");
                return;
            }
            for (k, members) in self
                .rows
                .iter()
                .enumerate()
                .skip(self.page * PAGE)
                .take(PAGE)
            {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("Group {} · {} files", k + 1, members.len()))
                            .strong(),
                    );
                    if ui.small_button("Select group").clicked() {
                        action = Action::Select(members.clone());
                    }
                    if let Some(&first) = members.first() {
                        if ui
                            .small_button("Show in grid")
                            .on_hover_text("Search similar: for the first file of the group")
                            .clicked()
                        {
                            action = Action::Search(rows[first].path.clone());
                        }
                    }
                });
                ui.horizontal_wrapped(|ui| {
                    for &i in members {
                        let row = &rows[i];
                        let line = format!(
                            "{}×{} {} · {}",
                            row.width,
                            row.height,
                            row.format,
                            widgets::tags_label(row)
                        );
                        let stroke = selected
                            .contains(&i)
                            .then(|| egui::Stroke::new(3.0, widgets::SELECTED));
                        let resp = widgets::tile(ui, row, textures.get(&i), &line, stroke)
                            .on_hover_text(widgets::hover_text(row));
                        if resp.clicked() {
                            action = Action::Toggle(i);
                        }
                        if resp.secondary_clicked() {
                            action = Action::Open(i);
                        }
                    }
                });
                ui.add_space(6.0);
            }
        });
        if !open {
            return Action::Close;
        }
        action
    }
}
