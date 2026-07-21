use std::fs;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use walkdir::WalkDir;
use std::io::{Read, Write};
use std::collections::HashMap;

#[derive(Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
pub enum ConflictResolution {
    Skip,
    Overwrite,
    Rename,
}

pub struct ProgressMessage {
    pub source_index: usize,
    pub status: String,
    pub progress_percent: f32,
    pub speed_bytes_per_sec: f64,
    pub eta_seconds: Option<u64>,
    pub total_size_bytes: u64,
    pub copied_size_bytes: u64,
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ScanResult {
    pub camera_label: String,
    pub date: String,
    pub extension: String,
    pub count: usize,
    pub total_size: u64,
}

pub struct ScanMessage {
    pub is_done: bool,
    pub status: String,
    pub results: Vec<ScanResult>,
}

pub fn start_import(
    source_index: usize,
    source_path: PathBuf,
    dest_base: PathBuf,
    project_label: String,
    camera_label: String,
    extensions: Vec<String>,
    start_date: Option<chrono::NaiveDate>,
    end_date: Option<chrono::NaiveDate>,
    conflict_res: ConflictResolution,
    tx: Sender<ProgressMessage>,
    cancel_flag: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let _ = tx.send(ProgressMessage {
            source_index,
            status: "Scanning for files...".to_string(),
            progress_percent: 0.0,
            speed_bytes_per_sec: 0.0,
            eta_seconds: None,
            total_size_bytes: 0,
            copied_size_bytes: 0,
            done: false,
        });

        let mut files_to_copy = Vec::new();
        let mut total_size = 0;

        for entry in WalkDir::new(&source_path).into_iter().filter_map(|e| e.ok()) {
            if cancel_flag.load(Ordering::SeqCst) {
                return;
            }
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                    if extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
                        if let Ok(metadata) = fs::metadata(path) {
                            let mut include = true;
                            
                            // Check date bounds if provided
                            if start_date.is_some() || end_date.is_some() {
                                if let Ok(modified) = metadata.modified() {
                                    use chrono::{DateTime, Local};
                                    let dt: DateTime<Local> = modified.into();
                                    let date = dt.naive_local().date();
                                    
                                    if let Some(start) = start_date {
                                        if date < start { include = false; }
                                    }
                                    if let Some(end) = end_date {
                                        if date > end { include = false; }
                                    }
                                }
                            }
                            
                            if include {
                                files_to_copy.push(path.to_path_buf());
                                total_size += metadata.len();
                            }
                        }
                    }
                }
            }
        }

        if files_to_copy.is_empty() {
            let _ = tx.send(ProgressMessage {
                source_index,
                status: "No matching files found.".to_string(),
                progress_percent: 100.0,
                speed_bytes_per_sec: 0.0,
                eta_seconds: None,
                total_size_bytes: 0,
                copied_size_bytes: 0,
                done: true,
            });
            return;
        }

        if let Ok(free_space) = fs2::free_space(&dest_base) {
            if total_size > free_space.saturating_sub(100 * 1024 * 1024) { // keep a 100MB buffer
                let req_mb = total_size / 1_048_576;
                let free_mb = free_space / 1_048_576;
                let _ = tx.send(ProgressMessage {
                    source_index,
                    status: format!("Error: Not enough space! Need {} MB, have {} MB", req_mb, free_mb),
                    progress_percent: 100.0,
                    speed_bytes_per_sec: 0.0,
                    eta_seconds: None,
                    total_size_bytes: total_size,
                    copied_size_bytes: 0,
                    done: true,
                });
                return;
            }
        }

        let mut final_dest = dest_base.clone();
        final_dest.push(&project_label);
        final_dest.push(&camera_label);

        if let Err(e) = fs::create_dir_all(&final_dest) {
            let _ = tx.send(ProgressMessage {
                source_index,
                status: format!("Error creating dir: {}", e),
                progress_percent: 0.0,
                speed_bytes_per_sec: 0.0,
                eta_seconds: None,
                total_size_bytes: total_size,
                copied_size_bytes: 0,
                done: true,
            });
            return;
        }

        let _ = tx.send(ProgressMessage {
            source_index,
            status: format!("Found {} files to copy", files_to_copy.len()),
            progress_percent: 0.0,
            speed_bytes_per_sec: 0.0,
            eta_seconds: None,
            total_size_bytes: total_size,
            copied_size_bytes: 0,
            done: false,
        });

        let mut copied_size: u64 = 0;
        let start_time = std::time::Instant::now();

        for file_path in files_to_copy {
            if cancel_flag.load(Ordering::SeqCst) {
                return;
            }

            let filename = file_path.file_name().unwrap();
            let mut dest_path = final_dest.join(filename);

            if dest_path.exists() {
                match conflict_res {
                    ConflictResolution::Skip => {
                        if let Ok(meta) = fs::metadata(&file_path) {
                            copied_size += meta.len();
                        }
                        let pct = if total_size > 0 {
                            (copied_size as f32 / total_size as f32) * 100.0
                        } else {
                            0.0
                        };
                        let _ = tx.send(ProgressMessage {
                            source_index,
                            status: format!("Skipped {}", filename.to_string_lossy()),
                            progress_percent: pct,
                            speed_bytes_per_sec: 0.0,
                            eta_seconds: None,
                            total_size_bytes: total_size,
                            copied_size_bytes: copied_size,
                            done: false,
                        });
                        continue;
                    }
                    ConflictResolution::Rename => {
                        let base = file_path.file_stem().unwrap().to_string_lossy();
                        let ext = file_path.extension().unwrap_or_default().to_string_lossy();
                        let mut counter = 1;
                        while dest_path.exists() {
                            let new_name = if ext.is_empty() {
                                format!("{}_{}", base, counter)
                            } else {
                                format!("{}_{}.{}", base, counter, ext)
                            };
                            dest_path = final_dest.join(new_name);
                            counter += 1;
                        }
                    }
                    ConflictResolution::Overwrite => {} // Default behavior
                }
            }

            let _ = tx.send(ProgressMessage {
                source_index,
                status: format!("Copying {}...", filename.to_string_lossy()),
                progress_percent: if total_size > 0 { (copied_size as f32 / total_size as f32) * 100.0 } else { 0.0 },
                speed_bytes_per_sec: 0.0,
                eta_seconds: None,
                total_size_bytes: total_size,
                copied_size_bytes: copied_size,
                done: false,
            });

            // Perform chunked copy
            if let Ok(mut src_file) = fs::File::open(&file_path) {
                if let Ok(mut dst_file) = fs::File::create(&dest_path) {
                    let mut buffer = vec![0; 4 * 1024 * 1024]; // 4 MB chunk
                    loop {
                        if cancel_flag.load(Ordering::SeqCst) {
                            let _ = fs::remove_file(&dest_path);
                            return;
                        }
                        match src_file.read(&mut buffer) {
                            Ok(0) => break,
                            Ok(n) => {
                                if dst_file.write_all(&buffer[..n]).is_err() {
                                    break; // Error writing
                                }
                                copied_size += n as u64;
                                let pct = if total_size > 0 {
                                    (copied_size as f32 / total_size as f32) * 100.0
                                } else {
                                    0.0
                                };
                                let elapsed = start_time.elapsed().as_secs_f64();
                                let speed = if elapsed > 0.0 {
                                    copied_size as f64 / elapsed
                                } else {
                                    0.0
                                };
                                let remaining = total_size.saturating_sub(copied_size);
                                let eta = if speed > 0.0 {
                                    Some((remaining as f64 / speed) as u64)
                                } else {
                                    None
                                };

                                let _ = tx.send(ProgressMessage {
                                    source_index,
                                    status: format!("Copying: {}", filename.to_string_lossy()),
                                    progress_percent: pct,
                                    speed_bytes_per_sec: speed,
                                    eta_seconds: eta,
                                    total_size_bytes: total_size,
                                    copied_size_bytes: copied_size,
                                    done: false,
                                });
                            }
                            Err(_) => break, // Error reading
                        }
                    }
                }
            }
        }

        let _ = tx.send(ProgressMessage {
            source_index,
            status: "Done".to_string(),
            progress_percent: 100.0,
            speed_bytes_per_sec: 0.0,
            eta_seconds: None,
            total_size_bytes: total_size,
            copied_size_bytes: copied_size,
            done: true,
        });
    });
}

pub fn start_scan(
    source_path: PathBuf,
    camera_label: String,
    extensions: Vec<String>,
    start_date: Option<chrono::NaiveDate>,
    end_date: Option<chrono::NaiveDate>,
    tx: Sender<ScanMessage>,
    cancel_flag: Arc<AtomicBool>,
) {
    std::thread::spawn(move || {
        let _ = tx.send(ScanMessage {
            is_done: false,
            status: "Scanning...".to_string(),
            results: vec![],
        });

        // Key: (Date String, Extension)
        let mut stats: HashMap<(String, String), (usize, u64)> = HashMap::new();

        for entry in WalkDir::new(&source_path).into_iter().filter_map(|e| e.ok()) {
            if cancel_flag.load(Ordering::SeqCst) {
                return;
            }
            let path = entry.path();
            if path.is_file() {
                if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                    if extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
                        if let Ok(metadata) = fs::metadata(path) {
                            let mut include = true;
                            let mut date_str = "Unknown".to_string();
                            
                            if let Ok(modified) = metadata.modified() {
                                use chrono::{DateTime, Local};
                                let dt: DateTime<Local> = modified.into();
                                let date = dt.naive_local().date();
                                date_str = date.to_string();
                                
                                if let Some(start) = start_date {
                                    if date < start { include = false; }
                                }
                                if let Some(end) = end_date {
                                    if date > end { include = false; }
                                }
                            }
                            
                            if include {
                                let key = (date_str, ext.to_lowercase());
                                let entry = stats.entry(key).or_insert((0, 0));
                                entry.0 += 1;
                                entry.1 += metadata.len();
                            }
                        }
                    }
                }
            }
        }

        let mut results = Vec::new();
        for ((date, ext), (count, size)) in stats {
            results.push(ScanResult {
                camera_label: camera_label.clone(),
                date,
                extension: ext,
                count,
                total_size: size,
            });
        }
        
        // Sort results by date then extension
        results.sort_by(|a, b| {
            let cmp = a.date.cmp(&b.date);
            if cmp == std::cmp::Ordering::Equal {
                a.extension.cmp(&b.extension)
            } else {
                cmp
            }
        });

        let _ = tx.send(ScanMessage {
            is_done: true,
            status: "Done".to_string(),
            results,
        });
    });
}
