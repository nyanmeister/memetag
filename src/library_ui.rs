use crate::{
    index::Db,
    library::Scope,
    sources::{Draft, Remote, Source},
    Cfg,
};
use eframe::egui;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

pub enum Action {
    None,
    Close,
    Saved,
}
pub struct LibraryUi {
    draft: Draft,
    selected: usize,
    known: BTreeMap<String, BTreeSet<PathBuf>>,
    counts: BTreeMap<String, usize>,
    folder: String,
    filter: String,
    new_name: String,
    new_path: String,
    new_network: bool,
    new_remote: Remote,
    new_server_root: String,
    locate: String,
    error: Option<String>,
}
impl LibraryUi {
    pub fn new(c: &Cfg) -> Result<Self, String> {
        let db = Db::open_cfg(c)?;
        let draft = Draft::load(&c.root)?;
        let new_remote = draft
            .library
            .sources
            .iter()
            .find_map(|s| s.remote.clone())
            .unwrap_or_else(|| Remote::defaults(String::new(), PathBuf::new()));
        let mut known: BTreeMap<String, BTreeSet<PathBuf>> = BTreeMap::new();
        let mut counts = BTreeMap::new();
        for row in db.all_cached()? {
            if let Ok((source, rel)) = draft.library.identify(&row.path) {
                *counts.entry(source.id.clone()).or_insert(0) += 1;
                if let Some(dir) = rel
                    .components()
                    .next()
                    .filter(|_| rel.components().count() > 1)
                {
                    known
                        .entry(source.id.clone())
                        .or_default()
                        .insert(PathBuf::from(dir.as_os_str()));
                }
            }
        }
        for source in &draft.library.sources {
            if source.remote.is_none() && source.available() {
                if let Ok(entries) = std::fs::read_dir(&source.path) {
                    for entry in entries.flatten() {
                        if entry.file_type().is_ok_and(|t| t.is_dir()) {
                            known
                                .entry(source.id.clone())
                                .or_default()
                                .insert(PathBuf::from(entry.file_name()));
                        }
                    }
                }
            }
            for folder in source.scope.include.iter().chain(&source.scope.exclude) {
                if folder != std::path::Path::new(".") {
                    known
                        .entry(source.id.clone())
                        .or_default()
                        .insert(folder.clone());
                }
            }
        }
        Ok(Self {
            draft,
            selected: 0,
            known,
            counts,
            folder: String::new(),
            filter: String::new(),
            new_name: String::new(),
            new_path: String::new(),
            new_network: false,
            new_remote,
            new_server_root: String::new(),
            locate: String::new(),
            error: None,
        })
    }
    fn status(source: &Source) -> &'static str {
        if source.remote.is_some() {
            "Server source — refresh checks availability"
        } else if source.available() {
            "Available locally"
        } else {
            "Offline or replaced folder — cache retained; use Locate"
        }
    }
    fn body(&mut self, ui: &mut egui::Ui) -> Action {
        ui.heading("Library sources and folders");
        ui.label("Sources share one searchable library. Disabling folders keeps their cached OCR and vectors.");
        ui.horizontal_wrapped(|ui| {
            for (index, source) in self.draft.library.sources.iter().enumerate() {
                if ui
                    .selectable_label(self.selected == index, &source.name)
                    .clicked()
                {
                    self.selected = index;
                    self.locate.clear();
                    self.folder.clear();
                    self.filter.clear();
                }
            }
        });
        ui.separator();
        let source = &mut self.draft.library.sources[self.selected];
        ui.horizontal(|ui| {
            ui.checkbox(&mut source.enabled, "Include this source");
            ui.label("Name:");
            ui.text_edit_singleline(&mut source.name);
        });
        ui.label(format!("Folder: {}", source.path.display()));
        ui.label(format!(
            "{} · {} cached files",
            Self::status(source),
            self.counts.get(&source.id).copied().unwrap_or(0)
        ));
        if let Some(remote) = &mut source.remote {
            ui.collapsing("Server settings", |ui| {
                ui.horizontal(|ui| { ui.label("SSH host:"); ui.text_edit_singleline(&mut remote.host); });
                let mut root = remote.root.to_string_lossy().into_owned();
                ui.horizontal(|ui| { ui.label("Folder on server:"); if ui.text_edit_singleline(&mut root).changed() { remote.root = root.into(); } });
                ui.collapsing("Helper commands", |ui| Self::commands(ui, remote));
                ui.label("Changes point this source at the specified server folder. Save, then refresh to update its cached files.");
            });
        }
        let id = source.id.clone();
        let mut locate_clicked = false;
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.locate)
                    .hint_text("New full path to this source folder"),
            );
            locate_clicked = ui.button("Locate folder").clicked();
        });
        if locate_clicked {
            match self.draft.locate(&id, &PathBuf::from(self.locate.trim())) {
                Ok(()) => self.error = None,
                Err(e) => self.error = Some(e),
            }
        }
        let source = &mut self.draft.library.sources[self.selected];
        ui.horizontal(|ui| {
            if ui.button("Include all folders").clicked() {
                source.scope = Scope::default();
            }
            if ui.button("Include no folders").clicked() {
                source.scope = Scope {
                    include: vec![],
                    exclude: vec![],
                };
            }
        });
        ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("Filter subfolders"));
        let folders = self.known.entry(id.clone()).or_default();
        egui::ScrollArea::vertical()
            .max_height(180.0)
            .id_salt(("source-folders", &id))
            .show(ui, |ui| {
                for folder in folders.iter() {
                    if !folder
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&self.filter.to_lowercase())
                    {
                        continue;
                    }
                    let mut included = source.scope.contains(&folder.join("__file__"));
                    if ui
                        .checkbox(&mut included, folder.display().to_string())
                        .changed()
                    {
                        source.scope.set_included(folder.clone(), included);
                    }
                }
            });
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.folder).hint_text("Relative subfolder path"),
            );
            if ui.button("Add subfolder").clicked() {
                let folder = PathBuf::from(self.folder.trim());
                let mut scope = source.scope.clone();
                scope.set_included(folder.clone(), true);
                match scope.validate() {
                    Ok(()) => {
                        source.scope = scope;
                        folders.insert(folder);
                        self.folder.clear();
                        self.error = None;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
        });
        ui.collapsing("Selection rules", |ui| {
            ui.label(format!("Permanent source ID: {}", source.id));
            for p in &source.scope.include { ui.label(format!("Include {}", p.display())); }
            for p in &source.scope.exclude { ui.label(format!("Exclude {}", p.display())); }
            ui.label("More specific rules take precedence; exclusions win ties. Including all includes files directly in the source folder.");
        });
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Add an outside folder (files stay where they are)");
            ui.checkbox(&mut self.new_network, "Network source");
        });
        if self.new_network {
            ui.horizontal_wrapped(|ui| {
                ui.label("Reuse server:");
                for source in &self.draft.library.sources {
                    if let Some(remote) = &source.remote {
                        if ui
                            .button(&source.name)
                            .on_hover_text(&remote.host)
                            .clicked()
                        {
                            self.new_remote = remote.clone();
                        }
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.label("SSH host:");
                ui.text_edit_singleline(&mut self.new_remote.host);
            });
            ui.horizontal(|ui| {
                ui.label("Folder on server:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.new_server_root)
                        .hint_text("Blank uses the full folder path below"),
                );
            });
            ui.collapsing("Helper commands", |ui| {
                Self::commands(ui, &mut self.new_remote)
            });
            ui.label("The full path below is the folder as mounted on this computer. Scans and writes run on the SSH server, which needs memetag installed.");
        }
        ui.horizontal(|ui| {
            ui.add(
                egui::TextEdit::singleline(&mut self.new_name)
                    .hint_text("Source name")
                    .desired_width(120.0),
            );
            ui.add(egui::TextEdit::singleline(&mut self.new_path).hint_text("Full folder path"));
            if ui.button("Add folder").clicked() {
                let path = PathBuf::from(self.new_path.trim());
                let result = if self.new_network {
                    let mut remote = self.new_remote.clone();
                    remote.root = if self.new_server_root.trim().is_empty() {
                        path.clone()
                    } else {
                        self.new_server_root.trim().into()
                    };
                    self.draft.add_network(&self.new_name, &path, remote)
                } else {
                    self.draft.add(&self.new_name, &path)
                };
                match result {
                    Ok(_) => {
                        self.selected = self.draft.library.sources.len() - 1;
                        self.new_name.clear();
                        self.new_path.clear();
                        self.new_server_root.clear();
                        self.new_network = false;
                        self.error = None;
                    }
                    Err(e) => self.error = Some(e),
                }
            }
        });
        if let Some(error) = &self.error {
            ui.colored_label(crate::widgets::ERROR, error);
        }
        let mut action = Action::None;
        ui.horizontal(|ui| {
            if ui.button("Save sources").clicked() {
                match self.draft.save() {
                    Ok(()) => action = Action::Saved,
                    Err(e) => self.error = Some(e),
                }
            }
            if ui.button("Cancel").clicked() {
                action = Action::Close;
            }
        });
        ui.label("Save changes visibility immediately. Refresh/pull discovers files in newly added folders.");
        action
    }
    fn commands(ui: &mut egui::Ui, remote: &mut Remote) {
        ui.label("Scan/list command:");
        ui.text_edit_singleline(&mut remote.pull_command);
        ui.label("Write command:");
        ui.text_edit_singleline(&mut remote.batch_command);
    }
    pub fn show(&mut self, ctx: &egui::Context) -> Action {
        let mut open = true;
        let mut action = Action::None;
        egui::Window::new("Library folders")
            .id(egui::Id::new("library-menu"))
            .open(&mut open)
            .default_width(650.0)
            .vscroll(true)
            .show(ctx, |ui| {
                action = self.body(ui);
            });
        if open {
            action
        } else {
            Action::Close
        }
    }
}
struct Standalone {
    menu: LibraryUi,
}
impl eframe::App for Standalone {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ui, |ui| {
            let action = egui::ScrollArea::vertical()
                .id_salt("standalone-library-menu")
                .show(ui, |ui| self.menu.body(ui))
                .inner;
            if matches!(action, Action::Saved | Action::Close) {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        });
    }
}
pub fn run(c: &Cfg) -> Result<(), String> {
    let menu = LibraryUi::new(c)?;
    eframe::run_native(
        "memetag folders",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default().with_inner_size([700.0, 760.0]),
            ..Default::default()
        },
        Box::new(move |cc| {
            crate::grid::install_cjk_fallback(&cc.egui_ctx);
            Ok(Box::new(Standalone { menu }))
        }),
    )
    .map_err(|e| e.to_string())
}
