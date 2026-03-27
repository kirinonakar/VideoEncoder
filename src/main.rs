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

    // Handle files dropped via hook
    {
        let model = files_model.clone();
        let weak = ui_weak.clone();
        main_window.on_files_dropped(move |paths_str| {
            let paths: Vec<&str> = paths_str.split('|').collect();
            let mut added = 0;
            for p_str in paths {
                let path = PathBuf::from(p_str);
                if path.is_dir() {
                    if let Ok(entries) = std::fs::read_dir(&path) {
                        for entry in entries.flatten() {
                            let p = entry.path();
                            if is_video_file(&p) {
                                model.push(p.to_string_lossy().to_string().into());
                                added += 1;
                            }
                        }
                    }
                } else if is_video_file(&path) {
                    model.push(path.to_string_lossy().to_string().into());
                    added += 1;
                }
            }
            if let Some(ui) = weak.upgrade() {
                ui.set_current_file_text(format!("{} items added", added).into());
            }
        });
    }

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
                    let file_base_progress = idx as f32 / total_files as f32;

                    let mut cur_bitrate = "N/A".to_string();
                    let mut cur_speed = "N/A".to_string();

                    loop {
                        tokio::select! {
                            _ = async {
                                while !signal_task.load(Ordering::SeqCst) {
                                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                                }
                            } => {
                                let _ = child.kill().await;
                                break;
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
                                                cur_speed = caps.get(1).unwrap().as_str().to_string();
                                            }

                                            if let Some(caps) = time_re.captures(trimmed) {
                                                let current_time = time_str_to_seconds(caps.get(1).unwrap().as_str());
                                                if total_duration > 0.0 {
                                                    let cur_file_pc = (current_time / total_duration).clamp(0.0, 1.0);
                                                    let overall_pc = file_base_progress + (cur_file_pc / total_files as f32);
                                                    
                                                    let remaining_file_time = (total_duration - current_time).max(0.0);
                                                    let time_info = format!("{}/{} ({} remaining)", 
                                                        seconds_to_hms(current_time), 
                                                        seconds_to_hms(total_duration), 
                                                        seconds_to_hms(remaining_file_time));

                                                    let batch_elapsed = batch_start_time.elapsed().as_secs_f32();
                                                    let overall_time_info = if overall_pc > 0.01 {
                                                        let total_est = batch_elapsed / overall_pc;
                                                        let remaining_batch = total_est - batch_elapsed;
                                                        format!("Started: {} / Est: {} ({} remaining)", 
                                                            seconds_to_hms(batch_elapsed), 
                                                            seconds_to_hms(total_est), 
                                                            seconds_to_hms(remaining_batch))
                                                    } else {
                                                        format!("Started: {}", seconds_to_hms(batch_elapsed))
                                                    };

                                                    let progress_info = format!("{:.1}% ({}x, {})", cur_file_pc * 100.0, cur_speed, cur_bitrate);

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
                                        }
                                    }
                                }
                            }
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
