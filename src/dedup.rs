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
const TILE: f32 = 150.0;

pub struct DedupUi {
    /// Groups as paths (biggest first), and the same as row indices, rebuilt by `remap`.
    groups: Vec<Vec<String>>,
    rows: Vec<Vec<usize>>,
    /// How many results were grouped, or None while the thread still runs.
    scope: Option<usize>,
    page: usize,
    closing: bool,
}

impl DedupUi {
    pub fn new() -> Self {
        Self {
            groups: vec![],
            rows: vec![],
            scope: None,
            page: 0,
            closing: false,
        }
    }
    pub fn request_close(&mut self) {
        self.closing = true;
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
        let index: HashMap<&str, usize> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| (r.path.as_str(), i))
            .collect();
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
        textures: &HashMap<usize, (egui::TextureId, egui::Vec2, u32)>,
    ) -> Action {
        if self.closing {
            return Action::Close;
        }
        let max = ctx.content_rect().size() - egui::vec2(40.0, 60.0);
        let mut open = true;
        let mut action = Action::None;
        egui::Window::new("Duplicates").open(&mut open).collapsible(false).resizable(true).vscroll(true)
            .default_size(egui::vec2(1000.0, 700.0).min(max)).max_size(max).show(ctx, |ui| {
            let Some(scope) = self.scope else { ui.label("Finding look-alikes among the search results…"); ui.spinner(); return; };
            let grouped: usize = self.groups.iter().map(Vec::len).sum();
            ui.label(format!("{} groups holding {grouped} of the {scope} files in this search; every file in a group looks like every other in it (within {} of 256 hash bits). Narrow the search to narrow this.",
                             self.groups.len(), crate::similar::DEFAULT_DISTANCE));
            ui.small("Left-click a tile to select it, right-click to open the original; Edit tags on the select bar then works on the selection. Nothing here deletes.");
            let pages = self.groups.len().div_ceil(PAGE).max(1);
            if pages > 1 {
                ui.horizontal(|ui| {
                    if ui.add_enabled(self.page > 0, egui::Button::new("◀")).clicked() { self.page -= 1; }
                    ui.label(format!("groups {}–{} of {}", self.page * PAGE + 1, ((self.page + 1) * PAGE).min(self.groups.len()), self.groups.len()));
                    if ui.add_enabled(self.page + 1 < pages, egui::Button::new("▶")).clicked() { self.page += 1; }
                });
            }
            ui.separator();
            if self.groups.is_empty() { ui.label("No two files in this search look alike."); return; }
            for (k, members) in self.rows.iter().enumerate().skip(self.page * PAGE).take(PAGE) {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("Group {} · {} files", k + 1, members.len())).strong());
                    if ui.small_button("Select group").clicked() { action = Action::Select(members.clone()); }
                    if let Some(&first) = members.first() { if ui.small_button("Show in grid").on_hover_text("Search similar: for the first file of the group").clicked() { action = Action::Search(rows[first].path.clone()); } }
                });
                ui.horizontal_wrapped(|ui| {
                    for &i in members {
                        let row = &rows[i];
                        let (cell, resp) = ui.allocate_exact_size(egui::vec2(TILE, TILE + 34.0), egui::Sense::click());
                        let pic = egui::Rect::from_min_size(cell.min, egui::vec2(TILE, TILE));
                        ui.painter().rect_filled(pic, 4.0, ui.visuals().widgets.inactive.bg_fill);
                        if let Some((tid, size, frames)) = textures.get(&i) {
                            let scale = (TILE / size.x).min(TILE / size.y).min(4.0);
                            let draw = egui::Rect::from_center_size(pic.center(), *size * scale);
                            // the first frame of a strip, not the whole strip squeezed into one frame's box
                            let uv = egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0 / (*frames).max(1) as f32, 1.0));
                            egui::Image::from_texture(egui::load::SizedTexture::new(*tid, draw.size())).uv(uv).paint_at(ui, draw);
                        }
                        if selected.contains(&i) { ui.painter().rect_stroke(pic, 4.0, egui::Stroke::new(3.0, widgets::SELECTED), egui::StrokeKind::Inside); }
                        else if resp.hovered() { ui.painter().rect_stroke(pic, 4.0, ui.visuals().widgets.hovered.fg_stroke, egui::StrokeKind::Inside); }
                        let tags = if row.xmp_tag_count == 0 { "no tags".to_string() } else { format!("{} tags", row.xmp_tag_count) };
                        let folder = row.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("(root)");
                        let font = egui::FontId::proportional(11.0); let color = ui.visuals().weak_text_color();
                        ui.painter().text(pic.left_bottom() + egui::vec2(2.0, 4.0), egui::Align2::LEFT_TOP, format!("{}×{} {} · {tags}", row.width, row.height, row.format), font.clone(), color);
                        ui.painter().text(pic.left_bottom() + egui::vec2(2.0, 18.0), egui::Align2::LEFT_TOP, folder.chars().take(24).collect::<String>(), font, color);
                        let resp = resp.on_hover_text(format!("{}\n{} × {} · {} · {}\n{}", row.path, row.width, row.height, row.format, tags,
                                                             if row.tags.is_empty() { "No tags yet".to_string() } else { row.tags.iter().map(String::as_str).collect::<Vec<_>>().join(" · ") }));
                        if resp.clicked() { action = Action::Toggle(i); }
                        if resp.secondary_clicked() { action = Action::Open(i); }
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
