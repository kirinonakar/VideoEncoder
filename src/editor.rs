use crate::{
    MainWindow,
    encoder::{self, Cancel, EncodeJob},
    job_queue::Queue,
    job_queue::is_video_file,
    preview::{FrameRequester, enforce_playback_range, move_preview, playback_range},
    probe::probe_video,
    seconds_to_hms, settings,
    windows::TaskbarProgress,
};
use slint::{ComponentHandle, Model};
use std::{
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;

pub fn setup(
    main_window: &MainWindow,
    queue: Rc<Queue>,
    edit_cancel: Cancel,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    taskbar_progress: TaskbarProgress,
) -> Arc<FrameRequester> {
    let ui_weak = main_window.as_weak();
    let files_model = queue.files.clone();
    // === Video Editor (Trim / Crop) Callbacks ===
    let frame_requester = FrameRequester::new();
    fn load_video_async(
        ui_weak: slint::Weak<MainWindow>,
        requester: Arc<FrameRequester>,
        ffmpeg: String,
        path: PathBuf,
    ) {
        requester.stop_playback();
        if let Some(ui) = ui_weak.upgrade() {
            ui.set_video_loaded(false);
            ui.set_is_playing(false);
        }
        let weak = ui_weak.clone();
        tokio::spawn(async move {
            let res = probe_video(&ffmpeg, &path).await;
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    if ui.get_current_video_path().as_str() != path.to_string_lossy() {
                        return;
                    }
                    ui.set_is_playing(false);
                    match res {
                        Ok((dur, w, h, fps)) => {
                            ui.set_video_duration(dur);
                            ui.set_video_fps(fps);
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
                                    path.file_name()
                                        .map(|s| s.to_string_lossy())
                                        .unwrap_or_default(),
                                    seconds_to_hms(dur),
                                    w as i32,
                                    h as i32
                                )
                                .into(),
                            );
                            ui.set_crop_info_text(
                                format!("크롭 영역: 전체 ({}x{})", w as i32, h as i32).into(),
                            );
                            ui.set_edit_status_text("비디오를 불러왔습니다. 타임라인에서 구간을 선택하고 크롭 영역을 지정하세요.".into());
                            requester.request(
                                weak.clone(),
                                ffmpeg.clone(),
                                path.clone(),
                                0.0,
                                ui.get_video_width(),
                                ui.get_video_height(),
                            );
                        }
                        Err(e) => {
                            ui.set_video_loaded(false);
                            ui.set_edit_status_text(
                                format!("비디오 정보를 읽지 못했습니다: {}", e).into(),
                            );
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
        let queue = queue.clone();
        main_window.on_files_dropped(move |paths_str| {
            let Some(ui) = weak.upgrade() else {
                return;
            };
            if ui.get_is_encoding() || ui.get_edit_busy() {
                return;
            }
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
                                queue.add(&p);
                                added += 1;
                            }
                        }
                    }
                } else if is_video_file(&path) {
                    if first_video.is_none() {
                        first_video = Some((path.clone(), model.row_count()));
                    }
                    queue.add(&path);
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
        let model = files_model.clone();
        let requester = frame_requester.clone();
        let queue = queue.clone();
        main_window.on_pick_video(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_is_encoding() || ui.get_edit_busy() {
                    return;
                }
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter(
                        "Video",
                        &[
                            "mp4", "mkv", "avi", "mov", "wmv", "flv", "mpg", "mpeg", "webm",
                        ],
                    )
                    .pick_file()
                {
                    let ffmpeg = ui.get_ffmpeg_path().to_string();
                    if ffmpeg.is_empty() {
                        ui.set_edit_status_text("먼저 FFmpeg 경로를 설정하세요.".into());
                        return;
                    }
                    // 목록에 이미 있으면 해당 항목을, 없으면 새로 추가한 뒤 선택해 연다.
                    let path_str = path.to_string_lossy().to_string();
                    let mut idx = -1;
                    for i in 0..model.row_count() {
                        if model.row_data(i).as_deref() == Some(path_str.as_str()) {
                            idx = i as i32;
                            break;
                        }
                    }
                    if idx < 0 {
                        queue.add(&path);
                        idx = (model.row_count() - 1) as i32;
                    }
                    ui.set_selected_index(idx);
                    ui.set_current_file_text(format!("{} files in list", model.row_count()).into());
                    ui.set_current_video_path(path_str.into());
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
                if ui.get_edit_busy() {
                    return;
                }
                let ffmpeg = ui.get_ffmpeg_path().to_string();
                if ffmpeg.is_empty() {
                    ui.set_edit_status_text("먼저 FFmpeg 경로를 설정하세요.".into());
                    return;
                }
                let idx = ui.get_selected_index();
                let path = if idx >= 0 && (idx as usize) < model.row_count() {
                    model
                        .row_data(idx as usize)
                        .map(|s| PathBuf::from(s.as_str()))
                } else {
                    None
                };
                let path = path.or_else(|| {
                    let s = ui.get_current_video_path().to_string();
                    if s.is_empty() {
                        None
                    } else {
                        Some(PathBuf::from(s))
                    }
                });
                if let Some(p) = path {
                    if p.exists() {
                        ui.set_current_video_path(p.to_string_lossy().to_string().into());
                        ui.set_edit_status_text("비디오 정보를 읽는 중...".into());
                        load_video_async(weak.clone(), requester.clone(), ffmpeg, p);
                        return;
                    }
                }
                ui.set_edit_status_text(
                    "선택된 비디오가 없습니다. 파일을 추가하거나 '파일 선택'을 누르세요.".into(),
                );
            }
        });
    }

    // Seek (preview frame); the active playback limit also applies to scrubbing.
    {
        let weak = ui_weak.clone();
        let requester = frame_requester.clone();
        main_window.on_seek_time(move |t| {
            if let Some(ui) = weak.upgrade() {
                move_preview(&ui, &requester, t, false);
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
                ui.invoke_trim_changed();
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
                ui.invoke_trim_changed();
            }
        });
    }

    // Trim edits only affect playback when the range limit requires a correction.
    {
        let weak = ui_weak.clone();
        let requester = frame_requester.clone();
        main_window.on_trim_changed(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_time_start_text(seconds_to_hms(ui.get_trim_start()).into());
                ui.set_time_end_text(seconds_to_hms(ui.get_trim_end()).into());
                enforce_playback_range(&ui, &requester);
            }
        });
    }

    // Enabling the limit immediately constrains an out-of-range position.
    {
        let weak = ui_weak.clone();
        let requester = frame_requester.clone();
        main_window.on_playback_limit_changed(move || {
            if let Some(ui) = weak.upgrade() {
                enforce_playback_range(&ui, &requester);
            }
        });
    }
    {
        let weak = ui_weak.clone();
        let requester = frame_requester.clone();
        main_window.on_rewind(move || {
            if let Some(ui) = weak.upgrade() {
                let start = playback_range(&ui).start;
                move_preview(&ui, &requester, start, true);
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
                ui.set_crop_info_text(
                    format!(
                        "크롭: x={} y={} 크기={}x{} (전체 {}x{})",
                        x, y, w, h, vw as i32, vh as i32
                    )
                    .into(),
                );
            }
        });
    }

    // Play / Pause (ffmpeg 스트림 실시간 재생)
    {
        let weak = ui_weak.clone();
        let requester = frame_requester.clone();
        main_window.on_toggle_play(move || {
            if let Some(ui) = weak.upgrade() {
                if ui.get_is_playing() {
                    requester.stop_playback();
                    ui.set_is_playing(false);
                    return;
                }
                if !ui.get_video_loaded() {
                    return;
                }
                let range = playback_range(&ui);
                if range.end <= range.start {
                    return;
                }
                let t = range.play_from(ui.get_preview_time());
                ui.set_is_playing(true);
                move_preview(&ui, &requester, t, true);
            }
        });
    }

    // Reset edit options
    {
        let weak = ui_weak.clone();
        let requester = frame_requester.clone();
        main_window.on_reset_edit(move || {
            if let Some(ui) = weak.upgrade() {
                requester.stop_playback();
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
                        format!(
                            "크롭 영역: 전체 ({}x{})",
                            ui.get_video_width() as i32,
                            ui.get_video_height() as i32
                        )
                        .into(),
                    );
                    let ffmpeg = ui.get_ffmpeg_path().to_string();
                    if !ffmpeg.is_empty() {
                        let path = PathBuf::from(ui.get_current_video_path().to_string());
                        requester.request(
                            weak.clone(),
                            ffmpeg,
                            path,
                            0.0,
                            ui.get_video_width(),
                            ui.get_video_height(),
                        );
                    }
                }
                ui.set_edit_status_text("편집 옵션을 초기화했습니다.".into());
            }
        });
    }

    {
        let weak = ui_weak.clone();
        let requester = frame_requester.clone();
        let cancel = edit_cancel.clone();
        let workers = workers.clone();
        let taskbar = taskbar_progress.clone();
        main_window.on_run_edit(move || {
            if let Some(ui) = weak.upgrade() {
                launch_edit(&ui, &requester, cancel.clone(), &workers, taskbar.clone());
            }
        });
    }
    main_window.on_cancel_edit(move || edit_cancel.stop());

    frame_requester
}

#[derive(Debug, PartialEq)]
struct CropRect {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}
impl CropRect {
    fn from_normalized(vw: f32, vh: f32, x: f32, y: f32, w: f32, h: f32) -> anyhow::Result<Self> {
        anyhow::ensure!(
            [vw, vh, x, y, w, h].iter().all(|n| n.is_finite())
                && vw >= 2.0
                && vh >= 2.0
                && w > 0.0
                && h > 0.0,
            "비디오 해상도 또는 크롭 영역이 올바르지 않습니다"
        );
        let (vw, vh) = (vw.round() as u32, vh.round() as u32);
        let x = (x.clamp(0.0, 1.0) * vw as f32).round() as u32;
        let y = (y.clamp(0.0, 1.0) * vh as f32).round() as u32;
        let w = ((w.clamp(0.0, 1.0) * vw as f32).round() as u32).min(vw - x);
        let h = ((h.clamp(0.0, 1.0) * vh as f32).round() as u32).min(vh - y);
        anyhow::ensure!(w >= 2 && h >= 2, "크롭 영역은 최소 2×2 픽셀이어야 합니다");
        Ok(Self { x, y, w, h })
    }
    fn is_full(&self, vw: f32, vh: f32) -> bool {
        self.x == 0 && self.y == 0 && self.w == vw.round() as u32 && self.h == vh.round() as u32
    }
    fn filter(&self) -> String {
        format!(
            "crop={}:{}:{}:{}:exact=1",
            self.w - self.w % 2,
            self.h - self.h % 2,
            self.x,
            self.y
        )
    }
}

#[derive(Debug)]
struct EditPlan {
    range: Option<(f32, f32)>,
    filter: Option<String>,
    reencode: bool,
}

impl EditPlan {
    fn new(
        duration: f32,
        start: f32,
        end: f32,
        fps: f32,
        filter: Option<String>,
        exact_cut: bool,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            [duration, start, end].iter().all(|n| n.is_finite())
                && duration > 0.0
                && start >= 0.0
                && end <= duration
                && end > start,
            "잘라낼 구간이 올바르지 않습니다."
        );
        let trimmed = start > 0.0 || end < duration;
        anyhow::ensure!(
            trimmed || filter.is_some(),
            "크롭 영역 또는 잘라낼 구간을 설정하세요."
        );
        let reencode = filter.is_some() || exact_cut;
        let range = if trimmed {
            let (start, end) = if reencode {
                let fps = fps.max(1.0);
                (
                    (start * fps).round() / fps,
                    ((end * fps).round() / fps).min(duration),
                )
            } else {
                (start, end)
            };
            anyhow::ensure!(end > start, "잘라낼 구간이 올바르지 않습니다.");
            Some((start, end - start))
        } else {
            None
        };
        Ok(Self {
            range,
            filter,
            reencode,
        })
    }
}

fn launch_edit(
    ui: &MainWindow,
    requester: &Arc<FrameRequester>,
    cancel: Cancel,
    workers: &Arc<Mutex<Vec<JoinHandle<()>>>>,
    taskbar: TaskbarProgress,
) {
    if ui.get_is_encoding() || ui.get_edit_busy() {
        return;
    }
    let ffmpeg = ui.get_ffmpeg_path().to_string();
    let src = PathBuf::from(ui.get_current_video_path().to_string());
    if ffmpeg.is_empty() || !src.is_file() {
        ui.set_edit_status_text("FFmpeg 경로와 입력 파일을 확인하세요.".into());
        return;
    }
    let filter = match CropRect::from_normalized(
        ui.get_video_width(),
        ui.get_video_height(),
        ui.get_crop_x(),
        ui.get_crop_y(),
        ui.get_crop_w(),
        ui.get_crop_h(),
    ) {
        Ok(rect) => {
            (!rect.is_full(ui.get_video_width(), ui.get_video_height())).then(|| rect.filter())
        }
        Err(e) => {
            ui.set_edit_status_text(e.to_string().into());
            return;
        }
    };
    let plan = match EditPlan::new(
        ui.get_video_duration(),
        ui.get_trim_start(),
        ui.get_trim_end(),
        ui.get_video_fps(),
        filter,
        ui.get_cut_mode() == 1,
    ) {
        Ok(plan) => plan,
        Err(e) => {
            ui.set_edit_status_text(e.to_string().into());
            return;
        }
    };
    let EditPlan {
        range,
        filter,
        reencode: exact,
    } = plan;
    let stem = src.file_stem().unwrap_or_default().to_string_lossy();
    let ext = if exact {
        "mp4"
    } else {
        src.extension().and_then(|s| s.to_str()).unwrap_or("mp4")
    };
    let suffix = if filter.is_some() {
        "crop"
    } else if exact {
        "cut_exact"
    } else {
        "cut"
    };
    let folder = ui.get_output_folder().to_string();
    let parent = if folder.is_empty() {
        src.parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_owned()
    } else {
        PathBuf::from(folder)
    };
    let output = parent.join(format!("{stem}_{suffix}.{ext}"));
    let label = if filter.is_some() && range.is_some() {
        "크롭 + 정확한 컷"
    } else if filter.is_some() {
        "크롭"
    } else if exact {
        "정확한 컷"
    } else {
        "빠른 컷"
    };
    let tracks = settings::tracks(ui);
    let encode = exact.then(|| settings::encode_options(ui));
    requester.stop_playback();
    ui.set_is_playing(false);
    ui.set_edit_busy(true);
    ui.set_edit_progress(0.0);
    ui.set_error_details("".into());
    ui.set_edit_status_text(format!("{label} 진행 중...").into());
    cancel.reset();
    let weak = ui.as_weak();
    let worker = tokio::spawn(async move {
        taskbar.set_progress(0.0);
        let w = weak.clone();
        let t = taskbar.clone();
        let result = encoder::run(
            &ffmpeg,
            EncodeJob {
                src,
                output,
                range,
                encode,
                filter,
                tracks,
            },
            &cancel,
            move |p| {
                t.set_progress(p.fraction);
                let w = w.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        ui.set_edit_progress(p.fraction);
                    }
                });
            },
        )
        .await;
        taskbar.clear();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_edit_busy(false);
                match result {
                    Ok(path) => {
                        ui.set_edit_progress(1.0);
                        ui.set_edit_status_text(format!("{label} 완료: {}", path.display()).into());
                    }
                    Err(e) => {
                        let detail = format!("{e:#}");
                        ui.set_edit_status_text(
                            format!(
                                "{label} {}.",
                                if cancel.stopped() { "취소" } else { "실패" }
                            )
                            .into(),
                        );
                        ui.set_error_details(detail.into());
                    }
                }
            }
        });
    });
    let mut handles = workers.lock().unwrap();
    handles.retain(|h| !h.is_finished());
    handles.push(worker);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatically_combines_crop_and_trim_without_trimming_full_length_crop() {
        let crop = "crop=158:120:2:0:exact=1".to_owned();
        for exact_cut in [false, true] {
            assert!(EditPlan::new(3.03, 0.0, 3.03, 10.0, None, exact_cut).is_err());
            let crop_only =
                EditPlan::new(3.03, 0.0, 3.03, 10.0, Some(crop.clone()), exact_cut).unwrap();
            assert_eq!(crop_only.range, None);
            assert_eq!(crop_only.filter.as_deref(), Some(crop.as_str()));
            assert!(crop_only.reencode);

            let cut_only = EditPlan::new(3.03, 0.73, 1.66, 10.0, None, exact_cut).unwrap();
            assert!(cut_only.filter.is_none());
            assert_eq!(cut_only.reencode, exact_cut);
            let (start, length) = cut_only.range.unwrap();
            let (expected_start, expected_length) =
                if exact_cut { (0.7, 1.0) } else { (0.73, 0.93) };
            assert!((start - expected_start).abs() < 0.0001);
            assert!((length - expected_length).abs() < 0.0001);

            let both =
                EditPlan::new(3.03, 0.73, 1.66, 10.0, Some(crop.clone()), exact_cut).unwrap();
            assert_eq!(both.filter.as_deref(), Some(crop.as_str()));
            assert!(both.reencode);
            let (start, length) = both.range.unwrap();
            assert!((start - 0.7).abs() < 0.0001);
            assert!((length - 1.0).abs() < 0.0001);
        }
        for (start, end) in [(0.0, 2.0), (1.0, 3.03)] {
            assert!(
                EditPlan::new(3.03, start, end, 10.0, None, false)
                    .unwrap()
                    .range
                    .is_some()
            );
        }
        for (start, end) in [(2.0, 1.0), (-1.0, 2.0), (0.0, 4.0), (f32::NAN, 2.0)] {
            assert!(EditPlan::new(3.03, start, end, 10.0, Some(crop.clone()), false).is_err());
        }
    }

    #[test]
    fn only_exact_pixel_bounds_are_full_frame() {
        let full = CropRect::from_normalized(1920.0, 1080.0, 0.0, 0.0, 1.0, 1.0).unwrap();
        assert!(full.is_full(1920.0, 1080.0));
        for (x, y, w, h) in [
            (0.01, 0.0, 0.99, 1.0),
            (0.0, 0.01, 1.0, 0.99),
            (0.0, 0.0, 0.99, 0.99),
            (1.0 / 1920.0, 0.0, 1919.0 / 1920.0, 1.0),
        ] {
            let rect = CropRect::from_normalized(1920.0, 1080.0, x, y, w, h).unwrap();
            assert!(!rect.is_full(1920.0, 1080.0), "{rect:?}");
            assert!(rect.filter().contains("exact=1"));
        }
        assert!(
            CropRect::from_normalized(1921.0, 1081.0, 0.0, 0.0, 1.0, 1.0)
                .unwrap()
                .is_full(1921.0, 1081.0)
        );
        assert!(CropRect::from_normalized(0.0, 0.0, 0.0, 0.0, 1.0, 1.0).is_err());
    }
}
