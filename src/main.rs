#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

slint::include_modules!();

use anyhow::Result;
use slint::{Model, SharedString, VecModel, ModelRc};
use std::rc::Rc;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, OnceLock};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use regex::Regex;
use std::time::Instant;

// --- Windows-specific Drag & Drop Hooks ---
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM, GetLastError};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, ChangeWindowMessageFilterEx, SetWindowLongPtrW, GWLP_WNDPROC, 
    MSGFLT_ALLOW, WM_DROPFILES, WNDPROC,
};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Ole::RevokeDragDrop;

static APP_WINDOW_HANDLE: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();
#[cfg(target_os = "windows")]
static mut ORIGINAL_WNDPROC: WNDPROC = None;

#[cfg(target_os = "windows")]
unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_DROPFILES => {
            let hdrop = wparam as HDROP;
            let mut path_buf = [0u16; 1024]; 
            
            unsafe {
                let count = DragQueryFileW(hdrop, 0xFFFFFFFF, std::ptr::null_mut(), 0);
                let mut paths = Vec::new();
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, path_buf.as_mut_ptr(), 1024);
                    if len > 0 {
                        paths.push(String::from_utf16_lossy(&path_buf[..len as usize]));
                    }
                }
                
                if !paths.is_empty() {
                    let paths_str = paths.join("|");
                    if let Some(weak) = APP_WINDOW_HANDLE.get() {
                        let weak_clone = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak_clone.upgrade() {
                                ui.invoke_files_dropped(slint::SharedString::from(paths_str.as_str()));
                            }
                        });
                    }
                }
                DragFinish(hdrop);
            }
            return 0;
        }
        _ => {}
    }
    
    unsafe {
        if let Some(orig) = ORIGINAL_WNDPROC {
            CallWindowProcW(Some(orig), hwnd, msg, wparam, lparam)
        } else {
            windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

fn time_str_to_seconds(time_str: &str) -> f32 {
    let parts: Vec<&str> = time_str.split(':').collect();
    if parts.len() == 3 {
        let h: f32 = parts[0].trim().parse().unwrap_or(0.0);
        let m: f32 = parts[1].trim().parse().unwrap_or(0.0);
        let s: f32 = parts[2].trim().parse().unwrap_or(0.0);
        return h * 3600.0 + m * 60.0 + s;
    }
    0.0
}

fn seconds_to_hms(seconds: f32) -> String {
    let total_s = seconds as i32;
    let h = total_s / 3600;
    let m = (total_s % 3600) / 60;
    let s = total_s % 60;
    format!("{:02}:{:02}:{:02}", h, m, s)
}

// --- Video Editor (Trim / Crop) Helpers ---

struct FrameRequester {
    busy: AtomicBool,
    pending: std::sync::Mutex<Option<f32>>,
}

impl FrameRequester {
    fn request(self: &Arc<Self>, weak: slint::Weak<MainWindow>, ffmpeg: String, path: PathBuf, t: f32) {
        if self.busy.swap(true, Ordering::SeqCst) {
            *self.pending.lock().unwrap() = Some(t);
            return;
        }
        let this = self.clone();
        let w = weak.clone();
        tokio::spawn(async move {
            let mut t = t;
            loop {
                let res = extract_frame(&ffmpeg, &path, t)
                    .await
                    .and_then(|png| decode_png_rgba(&png));
                let next = { this.pending.lock().unwrap().take() };
                if next.is_none() {
                    this.busy.store(false, Ordering::SeqCst);
                }
                let w2 = w.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w2.upgrade() {
                        match &res {
                            Ok((bytes, width, height)) => {
                                let buf = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                    bytes, *width, *height,
                                );
                                ui.set_video_image(slint::Image::from_rgba8(buf));
                            }
                            Err(e) => {
                                ui.set_edit_status_text(format!("프레임 추출 실패: {}", e).into());
                            }
                        }
                    }
                });
                match next {
                    Some(n) => t = n,
                    None => break,
                }
            }
        });
    }
}

async fn probe_video(ffmpeg: &str, path: &Path) -> anyhow::Result<(f32, f32, f32)> {
    let mut cmd = Command::new(ffmpeg);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    let out = cmd
        .args(["-hide_banner", "-i"])
        .arg(path)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .output()
        .await?;
    let stderr = String::from_utf8_lossy(&out.stderr);
    let dur_re = Regex::new(r"Duration:\s*(\d{2,}:\d{2}:\d{2}\.\d+)").unwrap();
    let res_re = Regex::new(r"Video:.*?(\d{2,5})x(\d{2,5})").unwrap();
    let duration = dur_re
        .captures(&stderr)
        .and_then(|c| c.get(1))
        .map(|m| time_str_to_seconds(m.as_str()))
        .ok_or_else(|| anyhow::anyhow!("Duration 정보를 찾지 못했습니다"))?;
    let (w, h) = res_re
        .captures(&stderr)
        .map(|c| {
            (
                c[1].parse::<f32>().unwrap_or(0.0),
                c[2].parse::<f32>().unwrap_or(0.0),
            )
        })
        .unwrap_or((0.0, 0.0));
    Ok((duration, w, h))
}

async fn extract_frame(ffmpeg: &str, path: &Path, t: f32) -> anyhow::Result<Vec<u8>> {
    let mut cmd = Command::new(ffmpeg);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    let out = cmd
        .args(["-hide_banner", "-loglevel", "error", "-ss"])
        .arg(format!("{:.3}", t))
        .arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-an", "-f", "image2pipe", "-vcodec", "png", "-y", "pipe:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .output()
        .await?;
    anyhow::ensure!(
        !out.stdout.is_empty(),
        "프레임 추출 실패 (시간이 영상 끝을 벗어났을 수 있습니다)"
    );
    Ok(out.stdout)
}

fn decode_png_rgba(png: &[u8]) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let img = image::load_from_memory(png)?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Ok((rgba.into_raw(), w, h))
}

/// Watches ffmpeg stderr for progress. Returns success.
async fn watch_progress(mut child: tokio::process::Child, total_dur: f32, on_progress: impl Fn(f32) + Send + 'static) -> bool {
    let mut stderr = match child.stderr.take() {
        Some(s) => s,
        None => return false,
    };
    let time_re = Regex::new(r"(?:out_time|time)=\s*(\d{2,}:\d{2}:\d{2}\.\d+)").unwrap();
    let mut acc = String::new();
    let mut buf = [0u8; 4096];
    let mut last = -1.0f32;
    loop {
        let n = match stderr.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        acc.push_str(&String::from_utf8_lossy(&buf[..n]));
        while let Some(pos) = acc.find(|c| c == '\n' || c == '\r') {
            let line = acc[..pos].to_string();
            acc = acc[pos + 1..].to_string();
            if let Some(caps) = time_re.captures(&line) {
                let t = time_str_to_seconds(caps.get(1).unwrap().as_str());
                let pct = if total_dur > 0.0 { (t / total_dur).clamp(0.0, 1.0) } else { 0.0 };
                if (pct - last).abs() > 0.001 {
                    last = pct;
                    on_progress(pct);
                }
            }
        }
    }
    match child.wait().await {
        Ok(s) => s.success(),
        Err(_) => false,
    }
}

async fn do_cut(weak: slint::Weak<MainWindow>, ffmpeg: String, src: PathBuf, out: PathBuf, start: f32, dur: f32, label: String) {
    let mut cmd = Command::new(&ffmpeg);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    cmd.arg("-y")
        .arg("-hide_banner")
        .arg("-ss").arg(format!("{:.3}", start))
        .arg("-i").arg(&src)
        .arg("-t").arg(format!("{:.3}", dur))
        .arg("-c").arg("copy")
        .arg("-avoid_negative_ts").arg("make_zero")
        .arg("-map").arg("0")
        .arg("-map_metadata").arg("0")
        .arg("-progress").arg("pipe:2")
        .arg(&out)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .stdin(Stdio::null());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = slint::invoke_from_event_loop({
                let weak = weak.clone();
                move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_edit_busy(false);
                        ui.set_edit_status_text(format!("FFmpeg 실행 실패: {}", e).into());
                    }
                }
            });
            return;
        }
    };
    let ok = watch_progress(child, dur, {
        let weak = weak.clone();
        move |pct| {
            let weak2 = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak2.upgrade() {
                    ui.set_edit_progress(pct);
                }
            });
        }
    })
    .await;
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_edit_busy(false);
            if ok {
                ui.set_edit_progress(1.0);
                ui.set_edit_status_text(format!("{} 완료: {}", label, out.display()).into());
            } else {
                ui.set_edit_status_text(format!("{} 실패", label).into());
            }
        }
    });
}

async fn do_crop(weak: slint::Weak<MainWindow>, ffmpeg: String, src: PathBuf, out: PathBuf, start: f32, dur: f32, filter: String, crf: i32, label: String) {
    let mut cmd = Command::new(&ffmpeg);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    cmd.arg("-y")
        .arg("-hide_banner")
        .arg("-ss").arg(format!("{:.3}", start))
        .arg("-i").arg(&src)
        .arg("-t").arg(format!("{:.3}", dur))
        .arg("-vf").arg(&filter)
        .arg("-c:v").arg("libx265")
        .arg("-crf").arg(crf.to_string())
        .arg("-preset").arg("medium")
        .arg("-c:a").arg("copy")
        .arg("-map_metadata").arg("0")
        .arg("-progress").arg("pipe:2")
        .arg(&out)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .stdin(Stdio::null());
    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = slint::invoke_from_event_loop({
                let weak = weak.clone();
                move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_edit_busy(false);
                        ui.set_edit_status_text(format!("FFmpeg 실행 실패: {}", e).into());
                    }
                }
            });
            return;
        }
    };
    let ok = watch_progress(child, dur, {
        let weak = weak.clone();
        move |pct| {
            let weak2 = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak2.upgrade() {
                    ui.set_edit_progress(pct);
                }
            });
        }
    })
    .await;
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_edit_busy(false);
            if ok {
                ui.set_edit_progress(1.0);
                ui.set_edit_status_text(format!("{} 완료: {}", label, out.display()).into());
            } else {
                ui.set_edit_status_text(format!("{} 실패", label).into());
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    let main_window = MainWindow::new()?;
    let ui_weak = main_window.as_weak();
    let _ = APP_WINDOW_HANDLE.set(ui_weak.clone());

    // 1. Initial FFmpeg setup
    let initial_ffmpeg = if Path::new("./ffmpeg.exe").exists() {
        std::env::current_dir().unwrap().join("ffmpeg.exe").to_string_lossy().to_string()
    } else {
        which::which("ffmpeg")
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "".to_string())
    };
    main_window.set_ffmpeg_path(initial_ffmpeg.into());

    // State management
    let files_model = Rc::new(VecModel::<SharedString>::default());
    main_window.set_file_list(ModelRc::from(files_model.clone()));

    let stop_signal = Arc::new(AtomicBool::new(false));

    // --- Callbacks ---

    // Select FFmpeg Path
    {
        let weak = ui_weak.clone();
        main_window.on_select_ffmpeg_path(move || {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("Executable", &["exe"])
                .pick_file()
            {
                if let Some(ui) = weak.upgrade() {
                    ui.set_ffmpeg_path(path.to_string_lossy().to_string().into());
                }
            }
        });
    }

    // Select Output Folder
    {
        let weak = ui_weak.clone();
        main_window.on_select_output_folder(move || {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                if let Some(ui) = weak.upgrade() {
                    ui.set_output_folder(path.to_string_lossy().to_string().into());
                }
            }
        });
    }

    // Reset Options
    {
        let weak = ui_weak.clone();
        main_window.on_reset_options(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_output_suffix("_h265".into());
                ui.set_crf_value(19.0);
                ui.set_output_folder("".into());

            }
        });
    }

    // Pick Files
    {
        let weak = ui_weak.clone();
        let model = files_model.clone();
        main_window.on_pick_files(move || {
            if let Some(paths) = rfd::FileDialog::new()
                .add_filter("Video", &["mp4", "mkv", "avi", "mov", "wmv", "flv", "mpg", "mpeg"])
                .pick_files()
            {
                for p in paths {
                    model.push(p.to_string_lossy().to_string().into());
                }
                if let Some(ui) = weak.upgrade() {
                    ui.set_current_file_text(format!("{} files added", model.row_count()).into());
                }
            }
        });
    }

    // Pick Folder
    {
        let weak = ui_weak.clone();
        let model = files_model.clone();
        main_window.on_pick_folder(move || {
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                let mut added = 0;
                if let Ok(entries) = std::fs::read_dir(&path) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if is_video_file(&p) {
                            model.push(p.to_string_lossy().to_string().into());
                            added += 1;
                        }
                    }
                }
                if let Some(ui) = weak.upgrade() {
                    ui.set_current_file_text(format!("{} files added from folder", added).into());
                }
            }
        });
    }

    // Clear List
    {
        let weak = ui_weak.clone();
        let model = files_model.clone();
        main_window.on_clear_list(move || {
            while model.row_count() > 0 {
                model.remove(0);
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_current_file_text("List cleared".into());
            }
        });
    }

    // Stop Encoding
    {
        let signal = stop_signal.clone();
        main_window.on_stop_encoding(move || {
            signal.store(true, Ordering::SeqCst);
        });
    }

    // Start Encoding
    {
        let weak = ui_weak.clone();
        let model = files_model.clone();
        let signal = stop_signal.clone();
        
        main_window.on_start_encoding(move || {
            let ui = weak.upgrade().unwrap();
            let ffmpeg = ui.get_ffmpeg_path().to_string();
            let suffix = ui.get_output_suffix().to_string();
            let crf = ui.get_crf_value() as i32;
            let out_folder = ui.get_output_folder().to_string();
            
            let mut file_paths = Vec::new();
            for i in 0..model.row_count() {
                if let Some(s) = model.row_data(i) {
                    file_paths.push(PathBuf::from(s.as_str()));
                }
            }

            if ffmpeg.is_empty() {
                ui.set_current_file_text("Error: FFmpeg path not set".into());
                return;
            }

            ui.set_is_encoding(true);
            ui.set_overall_progress(0.0);
            ui.set_file_progress(0.0);
            ui.set_overall_time_text("".into());
            signal.store(false, Ordering::SeqCst);

            let weak_task = weak.clone();
            let signal_task = signal.clone();

            tokio::spawn(async move {
                let total_files = file_paths.len();
                let mut success_count = 0;
                let batch_start_time = Instant::now();

                // Regex patterns
                let duration_re = Regex::new(r"Duration:\s*(\d{2,}:\d{2}:\d{2}\.\d{2})").unwrap();
                let time_re = Regex::new(r"(?:time|out_time)=\s*(\d{2,}:\d{2}:\d{2}\.\d{2})").unwrap();
                let bitrate_re = Regex::new(r"bitrate=\s*(\S+)").unwrap();
                let speed_re = Regex::new(r"speed=\s*(\S+)").unwrap();

                for (idx, input_path) in file_paths.iter().enumerate() {
                    if signal_task.load(Ordering::SeqCst) {
                        break;
                    }

                    let file_name = input_path.file_stem().map(|s| s.to_string_lossy()).unwrap_or_default();
                    let output_name = format!("{}{}.mp4", file_name, suffix);
                    
                    let output_parent = if out_folder.is_empty() {
                        input_path.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| PathBuf::from("."))
                    } else {
                        PathBuf::from(&out_folder)
                    };
                    
                    let output_path = output_parent.join(output_name);

                    let base_msg = format!("[{}/{}] {}", idx + 1, total_files, input_path.file_name().unwrap_or_default().to_string_lossy());
                    let _ = slint::invoke_from_event_loop({
                        let weak = weak_task.clone();
                        let msg = base_msg.clone();
                        move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_current_file_text(msg.into());
                                ui.set_current_progress_text("Analyzing...".into());
                                ui.set_current_time_text("".into());
                            }
                        }
                    });

                    // Run FFmpeg
                    let mut cmd = Command::new(&ffmpeg);
                    #[cfg(windows)]
                    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW

                    cmd.arg("-y")
                        .arg("-i").arg(input_path)
                        .arg("-c:v").arg("libx265")
                        .arg("-crf").arg(crf.to_string())
                        .arg("-preset").arg("medium")
                        .arg("-c:a").arg("copy")
                        .arg("-progress").arg("pipe:2")
                        .arg(&output_path)
                        .stderr(Stdio::piped())
                        .stdin(Stdio::null())
                        .stdout(Stdio::null());

                    let mut child = match cmd.spawn() {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = slint::invoke_from_event_loop({
                                let weak = weak_task.clone();
                                let err_msg = format!("FFmpeg execution failed: {}", e);
                                move || {
                                    if let Some(ui) = weak.upgrade() {
                                        ui.set_current_file_text(err_msg.into());
                                    }
                                }
                            });
                            continue;
                        }
                    };

                    let mut stderr = child.stderr.take().unwrap();
                    let mut buffer = [0u8; 4096];
                    let mut stderr_acc = String::new();
                    
                    let mut total_duration = 0.0;
                    let mut current_video_time = 0.0;
                    let mut cur_bitrate = "N/A".to_string();
                    let mut cur_speed = "N/A".to_string();
                    let file_start_time = Instant::now();
                    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
                    
                    let file_base_progress = idx as f32 / total_files as f32;

                    loop {
                        let mut should_update_ui = false;
                        
                        tokio::select! {
                            _ = interval.tick() => {
                                if signal_task.load(Ordering::SeqCst) {
                                    let _ = child.kill().await;
                                    break;
                                }
                                should_update_ui = true;
                            }
                            
                            read_res = stderr.read(&mut buffer) => {
                                match read_res {
                                    Ok(0) | Err(_) => break,
                                    Ok(n) => {
                                        let fragment = String::from_utf8_lossy(&buffer[..n]);
                                        stderr_acc.push_str(&fragment);
                                        
                                        while let Some(pos) = stderr_acc.find(|c| c == '\n' || c == '\r') {
                                            let line = stderr_acc[..pos].to_string();
                                            stderr_acc = stderr_acc[pos+1..].to_string();
                                            let trimmed = line.trim();
                                            if trimmed.is_empty() { continue; }

                                            if total_duration == 0.0 {
                                                if let Some(caps) = duration_re.captures(trimmed) {
                                                    total_duration = time_str_to_seconds(caps.get(1).unwrap().as_str());
                                                }
                                            }
                                            if let Some(caps) = bitrate_re.captures(trimmed) {
                                                cur_bitrate = caps.get(1).unwrap().as_str().to_string();
                                            }
                                            if let Some(caps) = speed_re.captures(trimmed) {
                                                cur_speed = caps.get(1).unwrap().as_str().trim_end_matches('x').to_string();
                                            }
                                            if let Some(caps) = time_re.captures(trimmed) {
                                                current_video_time = time_str_to_seconds(caps.get(1).unwrap().as_str());
                                                should_update_ui = true;
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        if should_update_ui {
                            let file_elapsed = file_start_time.elapsed().as_secs_f32();
                            let batch_elapsed = batch_start_time.elapsed().as_secs_f32();
                            
                            let cur_file_pc = if total_duration > 0.0 {
                                (current_video_time / total_duration).clamp(0.0, 1.0)
                            } else {
                                0.0
                            };
                            
                            let overall_pc = file_base_progress + (cur_file_pc / total_files as f32);
                            
                            // Calculate real elapsed time and estimated total/remaining wall-clock time for the current file
                            let (file_est_total, remaining_file_time) = if cur_file_pc > 0.01 {
                                let est = file_elapsed / cur_file_pc;
                                (est, (est - file_elapsed).max(0.0))
                            } else {
                                (total_duration, (total_duration - current_video_time).max(0.0))
                            };

                            let time_info = if total_duration > 0.0 {
                                format!("{} / {} ({} remaining)", 
                                    seconds_to_hms(file_elapsed), 
                                    seconds_to_hms(file_est_total), 
                                    seconds_to_hms(remaining_file_time))
                            } else {
                                format!("{} (Analyzing...)", seconds_to_hms(file_elapsed))
                            };

                            let overall_time_info = if overall_pc > 0.01 {
                                let total_est = batch_elapsed / overall_pc;
                                let remaining_batch = (total_est - batch_elapsed).max(0.0);
                                format!("Started: {} / Est: {} ({} remaining)", 
                                    seconds_to_hms(batch_elapsed), 
                                    seconds_to_hms(total_est), 
                                    seconds_to_hms(remaining_batch))
                            } else {
                                format!("Started: {}", seconds_to_hms(batch_elapsed))
                            };

                            let progress_info = format!("{:.1}% (x{}, {})", cur_file_pc * 100.0, cur_speed, cur_bitrate);

                            let _ = slint::invoke_from_event_loop({
                                let weak = weak_task.clone();
                                let f_name = base_msg.clone();
                                move || {
                                    if let Some(ui) = weak.upgrade() {
                                        ui.set_overall_progress(overall_pc);
                                        ui.set_file_progress(cur_file_pc);
                                        ui.set_current_file_text(f_name.into());
                                        ui.set_current_progress_text(progress_info.into());
                                        ui.set_current_time_text(time_info.into());
                                        ui.set_overall_time_text(overall_time_info.into());
                                    }
                                }
                            });
                        }
                    }

                    match child.wait().await {
                        Ok(status) if status.success() => {
                            if !signal_task.load(Ordering::SeqCst) {
                                success_count += 1;
                            } else {
                                if output_path.exists() { let _ = std::fs::remove_file(&output_path); }
                            }
                        }
                        _ => {
                            if signal_task.load(Ordering::SeqCst) && output_path.exists() {
                                let _ = std::fs::remove_file(&output_path);
                            }
                        }
                    }
                }

                let final_msg = if signal_task.load(Ordering::SeqCst) {
                    "Task stopped".to_string()
                } else {
                    format!("Finished: {}/{} Success", success_count, total_files)
                };

                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak_task.upgrade() {
                        ui.set_current_file_text(final_msg.into());
                        ui.set_current_progress_text("".into());
                        ui.set_current_time_text("".into());
                        ui.set_overall_progress(1.0);
                        ui.set_file_progress(1.0);
                        ui.set_is_encoding(false);
                    }
                });
            });
        });
    }

    // === Video Editor (Trim / Crop) Callbacks ===
    let play_timer = Rc::new(slint::Timer::default());
    let frame_requester = Arc::new(FrameRequester {
        busy: AtomicBool::new(false),
        pending: std::sync::Mutex::new(None),
    });

    fn load_video_async(
        ui_weak: slint::Weak<MainWindow>,
        requester: Arc<FrameRequester>,
        ffmpeg: String,
        path: PathBuf,
    ) {
        let weak = ui_weak.clone();
        tokio::spawn(async move {
            let res = probe_video(&ffmpeg, &path).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_is_playing(false);
                    match res {
                        Ok((dur, w, h)) => {
                            ui.set_video_duration(dur);
                            ui.set_video_width(w);
                            ui.set_video_height(h);
                            ui.set_video_loaded(true);
                            ui.set_preview_time(0.0);
                            ui.set_trim_start(0.0);
                            ui.set_trim_end(dur);
                            ui.set_crop_x(0.0);
                            ui.set_crop_y(0.0);
                            ui.set_crop_w(1.0);
                            ui.set_crop_h(1.0);
                            ui.set_time_preview_text(seconds_to_hms(0.0).into());
                            ui.set_time_start_text(seconds_to_hms(0.0).into());
                            ui.set_time_end_text(seconds_to_hms(dur).into());
                            ui.set_video_info_text(
                                format!(
                                    "{}  |  길이 {}  |  해상도 {}x{}",
                                    path.file_name().map(|s| s.to_string_lossy()).unwrap_or_default(),
                                    seconds_to_hms(dur),
                                    w as i32,
                                    h as i32
                                )
                                .into(),
                            );
                            ui.set_crop_info_text(format!("크롭 영역: 전체 ({}x{})", w as i32, h as i32).into());
                            ui.set_edit_status_text("비디오를 불러왔습니다. 타임라인에서 구간을 선택하고 크롭 영역을 지정하세요.".into());
                            requester.request(weak.clone(), ffmpeg.clone(), path.clone(), 0.0);
                        }
                        Err(e) => {
                            ui.set_video_loaded(false);
                            ui.set_edit_status_text(format!("비디오 정보를 읽지 못했습니다: {}", e).into());
                        }
                    }
                }
            });
        });
    }

    // Handle files dropped via hook (auto-open the first video in the editor)
    {
        let model = files_model.clone();
        let weak = ui_weak.clone();
        let requester = frame_requester.clone();
        main_window.on_files_dropped(move |paths_str| {
            let paths: Vec<&str> = paths_str.split('|').collect();
            let mut added = 0;
            let mut first_video: Option<(PathBuf, usize)> = None;
            for p_str in paths {
                let path = PathBuf::from(p_str);
                if path.is_dir() {
                    if let Ok(entries) = std::fs::read_dir(&path) {
                        for entry in entries.flatten() {
                            let p = entry.path();
                            if is_video_file(&p) {
                                if first_video.is_none() {
                                    first_video = Some((p.clone(), model.row_count()));
                                }
                                model.push(p.to_string_lossy().to_string().into());
                                added += 1;
                            }
                        }
                    }
                } else if is_video_file(&path) {
                    if first_video.is_none() {
                        first_video = Some((path.clone(), model.row_count()));
                    }
                    model.push(path.to_string_lossy().to_string().into());
                    added += 1;
                }
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_current_file_text(format!("{} items added", added).into());
                if let Some((p, idx)) = first_video {
                    let ffmpeg = ui.get_ffmpeg_path().to_string();
                    if ffmpeg.is_empty() {
                        ui.set_edit_status_text("먼저 FFmpeg 경로를 설정하세요.".into());
                    } else {
                        ui.set_current_video_path(p.to_string_lossy().to_string().into());
                        ui.set_selected_index(idx as i32);
                        ui.set_edit_status_text("비디오 정보를 읽는 중...".into());
                        load_video_async(weak.clone(), requester.clone(), ffmpeg, p);
                    }
                }
            }
        });
    }

    // Pick video file
    {
        let weak = ui_weak.clone();
        let requester = frame_requester.clone();
        main_window.on_pick_video(move || {
            if let Some(ui) = weak.upgrade() {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("Video", &["mp4", "mkv", "avi", "mov", "wmv", "flv", "mpg", "mpeg"])
                    .pick_file()
                {
                    let ffmpeg = ui.get_ffmpeg_path().to_string();
                    if ffmpeg.is_empty() {
                        ui.set_edit_status_text("먼저 FFmpeg 경로를 설정하세요.".into());
                        return;
                    }
                    ui.set_current_video_path(path.to_string_lossy().to_string().into());
                    ui.set_edit_status_text("비디오 정보를 읽는 중...".into());
                    load_video_async(weak.clone(), requester.clone(), ffmpeg, path);
                }
            }
        });
    }

    // Open video from list selection
    {
        let weak = ui_weak.clone();
        let model = files_model.clone();
        let requester = frame_requester.clone();
        main_window.on_open_video(move || {
            if let Some(ui) = weak.upgrade() {
                let ffmpeg = ui.get_ffmpeg_path().to_string();
                if ffmpeg.is_empty() {
                    ui.set_edit_status_text("먼저 FFmpeg 경로를 설정하세요.".into());
                    return;
                }
                let idx = ui.get_selected_index();
                let path = if idx >= 0 && (idx as usize) < model.row_count() {
                    model.row_data(idx as usize).map(|s| PathBuf::from(s.as_str()))
                } else {
                    None
                };
                let path = path.or_else(|| {
                    let s = ui.get_current_video_path().to_string();
                    if s.is_empty() { None } else { Some(PathBuf::from(s)) }
                });
                if let Some(p) = path {
                    if p.exists() {
                        ui.set_current_video_path(p.to_string_lossy().to_string().into());
                        ui.set_edit_status_text("비디오 정보를 읽는 중...".into());
                        load_video_async(weak.clone(), requester.clone(), ffmpeg, p);
                        return;
                    }
                }
                ui.set_edit_status_text("선택된 비디오가 없습니다. 파일을 추가하거나 '파일 선택'을 누르세요.".into());
            }
        });
    }

    // Seek (preview frame)
    {
        let weak = ui_weak.clone();
        let requester = frame_requester.clone();
        main_window.on_seek_time(move |t| {
            if let Some(ui) = weak.upgrade() {
                let dur = ui.get_video_duration();
                let t = t.clamp(0.0, dur);
                ui.set_preview_time(t);
                ui.set_time_preview_text(seconds_to_hms(t).into());
                if ui.get_video_loaded() {
                    let ffmpeg = ui.get_ffmpeg_path().to_string();
                    if !ffmpeg.is_empty() {
                        let path = PathBuf::from(ui.get_current_video_path().to_string());
                        let t2 = (t - 0.05).max(0.0);
                        requester.request(weak.clone(), ffmpeg, path, t2);
                    }
                }
            }
        });
    }

    // Mark trim start / end
    {
        let weak = ui_weak.clone();
        main_window.on_mark_trim_start(move || {
            if let Some(ui) = weak.upgrade() {
                let t = ui.get_preview_time();
                let min_end = (ui.get_trim_end() - 0.05).max(0.0);
                ui.set_trim_start(t.min(min_end).max(0.0));
                ui.set_time_start_text(seconds_to_hms(ui.get_trim_start()).into());
            }
        });
    }
    {
        let weak = ui_weak.clone();
        main_window.on_mark_trim_end(move || {
            if let Some(ui) = weak.upgrade() {
                let t = ui.get_preview_time();
                let max_start = ui.get_trim_start() + 0.05;
                ui.set_trim_end(t.max(max_start).min(ui.get_video_duration()));
                ui.set_time_end_text(seconds_to_hms(ui.get_trim_end()).into());
            }
        });
    }

    // Trim values changed -> refresh labels
    {
        let weak = ui_weak.clone();
        main_window.on_trim_changed(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_time_start_text(seconds_to_hms(ui.get_trim_start()).into());
                ui.set_time_end_text(seconds_to_hms(ui.get_trim_end()).into());
            }
        });
    }

    // Crop changed -> refresh crop info
    {
        let weak = ui_weak.clone();
        main_window.on_crop_changed(move || {
            if let Some(ui) = weak.upgrade() {
                let vw = ui.get_video_width();
                let vh = ui.get_video_height();
                let x = (ui.get_crop_x() * vw).round() as i32;
                let y = (ui.get_crop_y() * vh).round() as i32;
                let w = ((ui.get_crop_w() * vw).round() as i32).max(0);
                let h = ((ui.get_crop_h() * vh).round() as i32).max(0);
                ui.set_crop_info_text(format!("크롭: x={} y={} 크기={}x{} (전체 {}x{})", x, y, w, h, vw as i32, vh as i32).into());
            }
        });
    }

    // Play / Pause
    {
        let weak = ui_weak.clone();
        let timer = play_timer.clone();
        let requester = frame_requester.clone();
        main_window.on_toggle_play(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_is_playing() {
                    timer.stop();
                    ui.set_is_playing(false);
                } else {
                    if !ui.get_video_loaded() { return; }
                    ui.set_is_playing(true);
                    let w = weak.clone();
                    let ffmpeg = ui.get_ffmpeg_path().to_string();
                    let path = PathBuf::from(ui.get_current_video_path().to_string());
                    let rq = requester.clone();
                    let timer2 = timer.clone();
                    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(500), move || {
                        if let Some(ui) = w.upgrade() {
                            if !ui.get_is_playing() {
                                timer2.stop();
                                return;
                            }
                            let t = ui.get_preview_time() + 0.5;
                            let dur = ui.get_video_duration();
                            if t >= dur {
                                ui.set_preview_time(dur);
                                ui.set_time_preview_text(seconds_to_hms(dur).into());
                                ui.set_is_playing(false);
                                timer2.stop();
                            } else {
                                ui.set_preview_time(t);
                                ui.set_time_preview_text(seconds_to_hms(t).into());
                                rq.request(w.clone(), ffmpeg.clone(), path.clone(), t);
                            }
                        }
                    });
                }
            }
        });
    }

    // Reset edit options
    {
        let weak = ui_weak.clone();
        let timer = play_timer.clone();
        let requester = frame_requester.clone();
        main_window.on_reset_edit(move || {
            if let Some(ui) = weak.upgrade() {
                timer.stop();
                ui.set_is_playing(false);
                let dur = ui.get_video_duration();
                ui.set_preview_time(0.0);
                ui.set_trim_start(0.0);
                ui.set_trim_end(dur);
                ui.set_crop_x(0.0);
                ui.set_crop_y(0.0);
                ui.set_crop_w(1.0);
                ui.set_crop_h(1.0);
                ui.set_time_preview_text(seconds_to_hms(0.0).into());
                ui.set_time_start_text(seconds_to_hms(0.0).into());
                ui.set_time_end_text(seconds_to_hms(dur).into());
                if ui.get_video_loaded() {
                    ui.set_crop_info_text(
                        format!("크롭 영역: 전체 ({}x{})", ui.get_video_width() as i32, ui.get_video_height() as i32).into(),
                    );
                    let ffmpeg = ui.get_ffmpeg_path().to_string();
                    if !ffmpeg.is_empty() {
                        let path = PathBuf::from(ui.get_current_video_path().to_string());
                        requester.request(weak.clone(), ffmpeg, path, 0.0);
                    }
                }
                ui.set_edit_status_text("편집 옵션을 초기화했습니다.".into());
            }
        });
    }

    // Run Cut (no re-encode)
    {
        let weak = ui_weak.clone();
        let timer = play_timer.clone();
        main_window.on_run_cut(move || {
            if let Some(ui) = weak.upgrade() {
                timer.stop();
                ui.set_is_playing(false);
                let ffmpeg = ui.get_ffmpeg_path().to_string();
                if ffmpeg.is_empty() {
                    ui.set_edit_status_text("FFmpeg 경로가 설정되지 않았습니다.".into());
                    return;
                }
                let src = PathBuf::from(ui.get_current_video_path().to_string());
                if !src.exists() {
                    ui.set_edit_status_text("비디오 파일이 없습니다.".into());
                    return;
                }
                let start = ui.get_trim_start();
                let end = ui.get_trim_end();
                let dur = end - start;
                if dur <= 0.05 {
                    ui.set_edit_status_text("잘라낼 구간이 올바르지 않습니다 (시작 < 끝).".into());
                    return;
                }
                let ext = src.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_else(|| "mp4".to_string());
                let stem = src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "output".to_string());
                let out = src.with_file_name(format!("{}_cut.{}", stem, ext));
                ui.set_edit_busy(true);
                ui.set_edit_progress(0.0);
                ui.set_edit_status_text(format!("잘라내기 시작: {}", src.display()).into());
                tokio::spawn(do_cut(weak.clone(), ffmpeg, src, out, start, dur, "잘라내기".to_string()));
            }
        });
    }

    // Run Crop + Cut (re-encode)
    {
        let weak = ui_weak.clone();
        let timer = play_timer.clone();
        main_window.on_run_crop(move || {
            if let Some(ui) = weak.upgrade() {
                timer.stop();
                ui.set_is_playing(false);
                let ffmpeg = ui.get_ffmpeg_path().to_string();
                if ffmpeg.is_empty() {
                    ui.set_edit_status_text("FFmpeg 경로가 설정되지 않았습니다.".into());
                    return;
                }
                let src = PathBuf::from(ui.get_current_video_path().to_string());
                if !src.exists() {
                    ui.set_edit_status_text("비디오 파일이 없습니다.".into());
                    return;
                }
                let start = ui.get_trim_start();
                let end = ui.get_trim_end();
                let dur = end - start;
                if dur <= 0.05 {
                    ui.set_edit_status_text("잘라낼 구간이 올바르지 않습니다 (시작 < 끝).".into());
                    return;
                }
                let stem = src.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "output".to_string());
                let vw = ui.get_video_width();
                let vh = ui.get_video_height();
                let cx = ui.get_crop_x();
                let cy = ui.get_crop_y();
                let cw = ui.get_crop_w();
                let ch = ui.get_crop_h();
                ui.set_edit_busy(true);
                ui.set_edit_progress(0.0);
                let full_frame = cw >= 0.98 && ch >= 0.98 && cx <= 0.02 && cy <= 0.02;
                if full_frame {
                    let ext = src.extension().map(|e| e.to_string_lossy().to_string()).unwrap_or_else(|| "mp4".to_string());
                    let out = src.with_file_name(format!("{}_cut.{}", stem, ext));
                    ui.set_edit_status_text("크롭 영역이 전체이므로 무손실 잘라내기로 실행합니다.".into());
                    tokio::spawn(do_cut(weak.clone(), ffmpeg, src, out, start, dur, "잘라내기".to_string()));
                    return;
                }
                if vw <= 0.0 || vh <= 0.0 {
                    ui.set_edit_busy(false);
                    ui.set_edit_status_text("비디오 해상도를 알 수 없어 크롭을 실행할 수 없습니다.".into());
                    return;
                }
                let mut pw = ((cw * vw).round() as i32).clamp(2, vw as i32);
                let mut ph = ((ch * vh).round() as i32).clamp(2, vh as i32);
                pw -= pw % 2;
                ph -= ph % 2;
                pw = pw.max(2);
                ph = ph.max(2);
                let mut px = ((cx * vw).round() as i32).clamp(0, vw as i32 - pw);
                let mut py = ((cy * vh).round() as i32).clamp(0, vh as i32 - ph);
                px -= px % 2;
                py -= py % 2;
                px = px.clamp(0, vw as i32 - pw);
                py = py.clamp(0, vh as i32 - ph);
                let filter = format!("crop={}:{}:{}:{}", pw, ph, px, py);
                let crf = ui.get_crf_value() as i32;
                let out = src.with_file_name(format!("{}_crop.mp4", stem));
                ui.set_edit_status_text(format!("크롭+컷 시작 ({}): {}", filter, src.display()).into());
                tokio::spawn(do_crop(weak.clone(), ffmpeg, src, out, start, dur, filter, crf, "크롭+컷".to_string()));
            }
        });
    }

    // --- Windows-specific Hook Implementation ---
    #[cfg(target_os = "windows")]
    {
        let ui_handle_clone = ui_weak.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
            if let Some(ui) = ui_handle_clone.upgrade() {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                
                let window_handle = ui.window().window_handle();
                if let Ok(handle) = window_handle.window_handle() {
                    if let RawWindowHandle::Win32(h) = handle.as_raw() {
                        let hwnd = h.hwnd.get() as HWND;
                        println!("Slint HWND success (deferred): {:?}", hwnd);

                        unsafe {
                            let hr = RevokeDragDrop(hwnd);
                            println!("RevokeDragDrop (S_OK=0): {}", hr);

                            ChangeWindowMessageFilterEx(hwnd, WM_DROPFILES, MSGFLT_ALLOW, std::ptr::null_mut());
                            ChangeWindowMessageFilterEx(hwnd, 0x0049, MSGFLT_ALLOW, std::ptr::null_mut()); 
                            ChangeWindowMessageFilterEx(hwnd, 0x004A, MSGFLT_ALLOW, std::ptr::null_mut());
                            
                            DragAcceptFiles(hwnd, 1);
                            println!("DragAcceptFiles enabled");

                            let prev_proc = SetWindowLongPtrW(
                                hwnd,
                                GWLP_WNDPROC,
                                wnd_proc as *const () as isize,
                            );
                            
                            if prev_proc != 0 {
                                println!("WndProc hook success. Prev addr: 0x{:X}", prev_proc);
                                type WndProcFn = unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;
                                ORIGINAL_WNDPROC = Some(core::mem::transmute::<isize, WndProcFn>(prev_proc));
                            } else {
                                println!("Warning: SetWindowLongPtrW failed: {}", GetLastError());
                            }
                        }
                    }
                }
            }
        });
    }

    main_window.run()?;
    Ok(())
}

fn is_video_file(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase();
    matches!(ext.as_str(), "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "mpg" | "mpeg")
}
