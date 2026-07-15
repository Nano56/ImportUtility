use eframe::egui;
use egui_extras::DatePickerButton;
use rfd::FileDialog;
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

use crate::importer::{self, ConflictResolution, ProgressMessage};

#[derive(Clone)]
struct SourceProgress {
    path: String,
    status: String,
    progress: f32,
    speed_bytes_per_sec: f64,
    eta_seconds: Option<u64>,
    total_size_bytes: u64,
    copied_size_bytes: u64,
    done: bool,
}

fn format_size(bytes: u64) -> String {
    if bytes >= 1_048_576 * 1024 {
        format!("{:.2} GB", bytes as f64 / (1_048_576.0 * 1024.0))
    } else if bytes >= 1_048_576 {
        format!("{:.1} MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

#[derive(serde::Deserialize, serde::Serialize)]
struct AppState {
    destination: Option<PathBuf>,
    project_label: String,
    conflict_res: ConflictResolution,
    extensions_input: String,
    filter_start_date: bool,
    filter_end_date: bool,
}

fn format_speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec >= 1_048_576.0 {
        format!("{:.1} MB/s", bytes_per_sec / 1_048_576.0)
    } else if bytes_per_sec >= 1024.0 {
        format!("{:.1} KB/s", bytes_per_sec / 1024.0)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}

fn format_eta(seconds: Option<u64>) -> String {
    if let Some(secs) = seconds {
        let m = secs / 60;
        let s = secs % 60;
        if m > 0 {
            format!("ETA: {}m {}s", m, s)
        } else {
            format!("ETA: {}s", s)
        }
    } else {
        "ETA: Calculating...".to_string()
    }
}

pub struct ImportUtilityApp {
    sources: Vec<(PathBuf, String)>,
    destination: Option<PathBuf>,
    project_label: String,
    conflict_res: ConflictResolution,
    
    extensions_input: String,
    
    new_source_input: String,
    dest_input: String,

    filter_start_date: bool,
    start_date: chrono::NaiveDate,
    filter_end_date: bool,
    end_date: chrono::NaiveDate,

    is_importing: bool,
    progress_states: Vec<SourceProgress>,
    
    tx: Sender<ProgressMessage>,
    rx: Receiver<ProgressMessage>,
    cancel_flag: Arc<AtomicBool>,
}

impl ImportUtilityApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (tx, rx) = channel();
        let mut app = Self {
            sources: Vec::new(),
            destination: None,
            project_label: String::new(),
            conflict_res: ConflictResolution::Skip,
            extensions_input: "mp4, mkv, nev, wav".to_string(),
            new_source_input: String::new(),
            dest_input: String::new(),
            filter_start_date: false,
            start_date: chrono::Local::now().date_naive(),
            filter_end_date: false,
            end_date: chrono::Local::now().date_naive(),
            is_importing: false,
            progress_states: Vec::new(),
            tx,
            rx,
            cancel_flag: Arc::new(AtomicBool::new(false)),
        };

        if let Some(storage) = cc.storage {
            if let Some(state) = eframe::get_value::<AppState>(storage, eframe::APP_KEY) {
                app.destination = state.destination.clone();
                if let Some(d) = &app.destination {
                    app.dest_input = d.to_string_lossy().to_string();
                }
                app.project_label = state.project_label;
                app.conflict_res = state.conflict_res;
                app.extensions_input = state.extensions_input;
                app.filter_start_date = state.filter_start_date;
                app.filter_end_date = state.filter_end_date;
            }
        }
        
        app
    }

    fn start_import(&mut self) {
        if self.sources.is_empty() || self.destination.is_none() || self.project_label.trim().is_empty() {
            return;
        }

        self.is_importing = true;
        self.progress_states.clear();
        self.cancel_flag.store(false, Ordering::SeqCst);

        let dest = self.destination.as_ref().unwrap().clone();
        let label = self.project_label.clone();
        let conflict = self.conflict_res.clone();
        
        let start_date = if self.filter_start_date { Some(self.start_date) } else { None };
        let end_date = if self.filter_end_date { Some(self.end_date) } else { None };
        
        let exts: Vec<String> = self.extensions_input
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();

        for (i, (source, camera_label)) in self.sources.iter().enumerate() {
            self.progress_states.push(SourceProgress {
                path: source.to_string_lossy().to_string(),
                status: "Waiting...".to_string(),
                progress: 0.0,
                speed_bytes_per_sec: 0.0,
                eta_seconds: None,
                total_size_bytes: 0,
                copied_size_bytes: 0,
                done: false,
            });

            importer::start_import(
                i,
                source.clone(),
                dest.clone(),
                label.clone(),
                camera_label.clone(),
                exts.clone(),
                start_date.clone(),
                end_date.clone(),
                conflict.clone(),
                self.tx.clone(),
                self.cancel_flag.clone(),
            );
        }
    }
}

impl eframe::App for ImportUtilityApp {
    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        let state = AppState {
            destination: self.destination.clone(),
            project_label: self.project_label.clone(),
            conflict_res: self.conflict_res.clone(),
            extensions_input: self.extensions_input.clone(),
            filter_start_date: self.filter_start_date,
            filter_end_date: self.filter_end_date,
        };
        eframe::set_value(storage, eframe::APP_KEY, &state);
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Process any messages from worker threads
        if self.is_importing {
            while let Ok(msg) = self.rx.try_recv() {
                if let Some(state) = self.progress_states.get_mut(msg.source_index) {
                    state.status = msg.status;
                    state.progress = msg.progress_percent;
                    state.speed_bytes_per_sec = msg.speed_bytes_per_sec;
                    state.eta_seconds = msg.eta_seconds;
                    state.total_size_bytes = msg.total_size_bytes;
                    state.copied_size_bytes = msg.copied_size_bytes;
                    state.done = msg.done;
                }
            }

            // Request continuous repaints so progress bars update smoothly
            ctx.request_repaint();

            // Check if all done
            if self.progress_states.iter().all(|s| s.done) {
                self.is_importing = false;
            }
        }

        let is_importing = self.is_importing;

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Video/Audio Import Utility");
            ui.add_space(10.0);

            ui.group(|ui| {
                ui.label(egui::RichText::new("Sources (Cameras / SD Cards)").strong());
                
                let mut to_remove = None;
                egui::ScrollArea::vertical().max_height(150.0).show(ui, |ui| {
                    for (i, (source, label)) in self.sources.iter_mut().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(source.to_string_lossy().to_string());
                            ui.label("Folder Name:");
                            ui.add_enabled(
                                !is_importing,
                                egui::TextEdit::singleline(label).desired_width(100.0),
                            );
                            if ui.button("Remove").clicked() && !is_importing {
                                to_remove = Some(i);
                            }
                        });
                    }
                });

                if let Some(idx) = to_remove {
                    self.sources.remove(idx);
                }

                ui.horizontal(|ui| {
                    ui.label("Path:");
                    ui.add_enabled(
                        !self.is_importing,
                        egui::TextEdit::singleline(&mut self.new_source_input),
                    );
                    if ui.button("Add").clicked() && !self.is_importing && !self.new_source_input.trim().is_empty() {
                        let path = PathBuf::from(self.new_source_input.trim());
                        if !self.sources.iter().any(|(p, _)| p == &path) {
                            let default_label = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                            self.sources.push((path, default_label));
                        }
                        self.new_source_input.clear();
                    }
                    ui.label("OR");
                    if ui.button("Browse...").clicked() && !self.is_importing {
                        if let Some(folder) = FileDialog::new().pick_folder() {
                            if !self.sources.iter().any(|(p, _)| p == &folder) {
                                let default_label = folder.file_name().unwrap_or_default().to_string_lossy().into_owned();
                                self.sources.push((folder, default_label));
                            }
                        }
                    }
                });
            });

            ui.add_space(10.0);
            
            ui.group(|ui| {
                ui.label(egui::RichText::new("Destination").strong());
                
                ui.horizontal(|ui| {
                    ui.label("Base Folder:");
                    ui.add_enabled(
                        !self.is_importing,
                        egui::TextEdit::singleline(&mut self.dest_input),
                    );
                    if ui.button("Browse...").clicked() && !self.is_importing {
                        if let Some(folder) = FileDialog::new().pick_folder() {
                            self.dest_input = folder.to_string_lossy().to_string();
                            self.destination = Some(folder);
                        }
                    }
                });
                
                // Keep `self.destination` in sync with the text input when Start Import is pressed
                
                ui.horizontal(|ui| {
                    ui.label("Label / Project Name:");
                    ui.add_enabled(
                        !self.is_importing,
                        egui::TextEdit::singleline(&mut self.project_label).hint_text("e.g. Day_1_Shoot"),
                    );
                });
            });

            ui.add_space(10.0);

            ui.group(|ui| {
                ui.label(egui::RichText::new("Settings & Filters").strong());
                ui.horizontal(|ui| {
                    ui.label("If file already exists:");
                    egui::ComboBox::from_id_source("conflict_combo")
                        .selected_text(format!("{:?}", self.conflict_res))
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.conflict_res, ConflictResolution::Skip, "Skip");
                            ui.selectable_value(&mut self.conflict_res, ConflictResolution::Overwrite, "Overwrite");
                            ui.selectable_value(&mut self.conflict_res, ConflictResolution::Rename, "Rename");
                        });
                });
                
                ui.horizontal(|ui| {
                    ui.label("File Extensions:");
                    ui.add_enabled(
                        !self.is_importing,
                        egui::TextEdit::singleline(&mut self.extensions_input).desired_width(200.0),
                    );
                });
                
                ui.horizontal(|ui| {
                    ui.label("Filter Dates:");
                    
                    ui.checkbox(&mut self.filter_start_date, "Start Date");
                    if self.filter_start_date {
                        ui.add_enabled(
                            !self.is_importing,
                            DatePickerButton::new(&mut self.start_date).id_source("start_date_picker"),
                        );
                    }
                    
                    ui.add_space(10.0);
                    
                    ui.checkbox(&mut self.filter_end_date, "End Date");
                    if self.filter_end_date {
                        ui.add_enabled(
                            !self.is_importing,
                            DatePickerButton::new(&mut self.end_date).id_source("end_date_picker"),
                        );
                    }
                });
            });

            ui.add_space(15.0);
            
            // Sync destination with dest_input manually if typed
            let dest_path = if !self.dest_input.trim().is_empty() {
                Some(PathBuf::from(self.dest_input.trim()))
            } else {
                None
            };
            self.destination = dest_path;

            let ready_to_start = !self.sources.is_empty() 
                && self.destination.is_some() 
                && !self.project_label.trim().is_empty();

            if self.is_importing {
                if ui.button("Cancel Import").clicked() {
                    self.cancel_flag.store(true, Ordering::SeqCst);
                }
            } else {
                ui.add_enabled_ui(ready_to_start, |ui| {
                    if ui.button("Start Import").clicked() {
                        self.start_import();
                    }
                });
            }

            ui.add_space(15.0);

            if !self.progress_states.is_empty() {
                ui.label(egui::RichText::new("Progress").strong());
                egui::ScrollArea::vertical().show(ui, |ui| {
                    for state in &self.progress_states {
                        ui.group(|ui| {
                            ui.label(egui::RichText::new(&state.path).strong());
                            ui.add(egui::ProgressBar::new(state.progress / 100.0).show_percentage());
                            ui.horizontal(|ui| {
                                ui.label(&state.status);
                                if !state.done && state.progress > 0.0 {
                                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                        ui.label(format!("{} / {} | {} | {}", 
                                            format_size(state.copied_size_bytes), 
                                            format_size(state.total_size_bytes),
                                            format_speed(state.speed_bytes_per_sec), 
                                            format_eta(state.eta_seconds)
                                        ));
                                    });
                                }
                            });
                        });
                    }
                });
            }
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.cancel_flag.store(true, Ordering::SeqCst);
    }
}
