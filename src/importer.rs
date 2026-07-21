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

pub fn should_include_file(
    path: &std::path::Path,
    metadata: &fs::Metadata,
    extensions: &[String],
    start_date: Option<chrono::NaiveDate>,
    end_date: Option<chrono::NaiveDate>,
) -> (bool, String) {
    if !path.is_file() {
        return (false, "Unknown".to_string());
    }
    
    let ext = match path.extension().and_then(|s| s.to_str()) {
        Some(e) => e,
        None => return (false, "Unknown".to_string()),
    };

    if !extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)) {
        return (false, "Unknown".to_string());
    }

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
    } else if start_date.is_some() || end_date.is_some() {
        include = false;
    }

    (include, date_str)
}

pub fn resolve_path_conflict(mut dest_path: PathBuf, file_path: &std::path::Path, final_dest: &std::path::Path) -> PathBuf {
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
    dest_path
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
            if let Ok(metadata) = fs::metadata(path) {
                let (include, _) = should_include_file(path, &metadata, &extensions, start_date, end_date);
                if include {
                    files_to_copy.push(path.to_path_buf());
                    total_size += metadata.len();
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
                        dest_path = resolve_path_conflict(dest_path, &file_path, &final_dest);
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
            if let Ok(metadata) = fs::metadata(path) {
                let (include, date_str) = should_include_file(path, &metadata, &extensions, start_date, end_date);
                if include {
                    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
                        let key = (date_str, ext.to_lowercase());
                        let entry = stats.entry(key).or_insert((0, 0));
                        entry.0 += 1;
                        entry.1 += metadata.len();
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::sync::mpsc;
    use tempfile::tempdir;

    #[test]
    fn test_resolve_path_conflict() {
        let dir = tempdir().unwrap();
        let final_dest = dir.path().to_path_buf();
        
        let file_path = final_dest.join("test.mp4");
        File::create(&file_path).unwrap(); // create dummy

        // Simulate conflict
        let conflict_path = final_dest.join("test.mp4");
        
        // Target file exists
        let new_path = resolve_path_conflict(conflict_path, &file_path, &final_dest);
        assert_eq!(new_path.file_name().unwrap(), "test_1.mp4");
        
        // If test_1.mp4 also exists
        File::create(final_dest.join("test_1.mp4")).unwrap();
        let conflict_path = final_dest.join("test.mp4");
        let new_path2 = resolve_path_conflict(conflict_path, &file_path, &final_dest);
        assert_eq!(new_path2.file_name().unwrap(), "test_2.mp4");
    }

    #[test]
    fn test_should_include_file() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("video.mp4");
        File::create(&file_path).unwrap();
        let metadata = fs::metadata(&file_path).unwrap();
        
        let extensions = vec!["mp4".to_string(), "mov".to_string()];
        
        let (include, _) = should_include_file(&file_path, &metadata, &extensions, None, None);
        assert!(include);
        
        let extensions_bad = vec!["txt".to_string()];
        let (include_bad, _) = should_include_file(&file_path, &metadata, &extensions_bad, None, None);
        assert!(!include_bad);
    }
    
    #[test]
    fn test_start_scan_integration() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().to_path_buf();
        
        File::create(source_path.join("vid1.mp4")).unwrap();
        File::create(source_path.join("vid2.mp4")).unwrap();
        File::create(source_path.join("audio.wav")).unwrap(); // Should be ignored
        
        let (tx, rx) = mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));
        
        start_scan(
            source_path,
            "CamA".to_string(),
            vec!["mp4".to_string()],
            None,
            None,
            tx,
            cancel_flag
        );
        
        let mut final_results = Vec::new();
        while let Ok(msg) = rx.recv() {
            if msg.is_done {
                final_results = msg.results;
                break;
            }
        }
        
        assert_eq!(final_results.len(), 1); // Only 1 date/extension combination (today, mp4)
        assert_eq!(final_results[0].extension, "mp4");
        assert_eq!(final_results[0].count, 2);
    }

    #[test]
    fn test_start_import_integration() {
        let dir = tempdir().unwrap();
        let source_path = dir.path().join("source");
        let dest_base = dir.path().join("dest");
        fs::create_dir_all(&source_path).unwrap();
        fs::create_dir_all(&dest_base).unwrap();

        // Create some dummy files to copy
        let dummy_data = b"hello world";
        let mut f1 = File::create(source_path.join("vid1.mp4")).unwrap();
        f1.write_all(dummy_data).unwrap();
        let mut f2 = File::create(source_path.join("vid2.mp4")).unwrap();
        f2.write_all(dummy_data).unwrap();

        let (tx, rx) = mpsc::channel();
        let cancel_flag = Arc::new(AtomicBool::new(false));

        start_import(
            0,
            source_path,
            dest_base.clone(),
            "MyProject".to_string(),
            "CamA".to_string(),
            vec!["mp4".to_string()],
            None,
            None,
            ConflictResolution::Overwrite,
            tx,
            cancel_flag,
        );

        // Wait for the import to finish
        while let Ok(msg) = rx.recv() {
            if msg.done {
                break;
            }
        }

        // Verify that the files were copied to the correct structure
        let final_dest = dest_base.join("MyProject").join("CamA");
        assert!(final_dest.exists(), "Destination folder was not created");
        assert!(final_dest.join("vid1.mp4").exists(), "vid1.mp4 was not copied");
        assert!(final_dest.join("vid2.mp4").exists(), "vid2.mp4 was not copied");

        // Verify content
        let copied_content = fs::read(final_dest.join("vid1.mp4")).unwrap();
        assert_eq!(copied_content, dummy_data);
    }
}
