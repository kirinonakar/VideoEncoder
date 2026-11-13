use crate::{
    JobItem, JobStatus, LocalizedText, MainWindow,
    encoder::{self, Cancel, EncodeJob},
    language::{Language, diagnostic, message},
    output, probe, seconds_to_hms, settings,
    windows::TaskbarProgress,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
    time::Instant,
};
use tokio::task::JoinHandle;

pub struct Queue {
    pub files: Rc<VecModel<SharedString>>,
    jobs: Rc<VecModel<JobItem>>,
    next_id: Cell<i32>,
    probe_cancel: Cancel,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
}
impl Queue {
    pub fn new(
        ui: &MainWindow,
        probe_cancel: Cancel,
        workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    ) -> Rc<Self> {
        let queue = Rc::new(Self {
            files: Rc::new(VecModel::default()),
            jobs: Rc::new(VecModel::default()),
            next_id: Cell::new(0),
            probe_cancel,
            workers,
        });
        ui.set_file_list(ModelRc::from(queue.files.clone()));
        ui.set_job_list(ModelRc::from(queue.jobs.clone()));
        queue
    }
    fn push(&self, path: &Path, source_info: LocalizedText) -> i32 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        let path: SharedString = path.to_string_lossy().into_owned().into();
        self.files.push(path.clone());
        self.jobs.push(JobItem {
            id,
            path,
            source_info,
            status: JobStatus::Waiting,
            detail: LocalizedText::default(),
        });
        id
    }

    pub fn add_files(&self, ui: &MainWindow, paths: Vec<PathBuf>) {
        let ffmpeg = ui.get_ffmpeg_path().to_string();
        if ffmpeg.is_empty() {
            for path in paths {
                self.push(
                    &path,
                    message!("Set the FFmpeg path.", "FFmpeg 경로를 설정하세요."),
                );
            }
            return;
        }
        if paths.is_empty() {
            return;
        }
        let pending: Vec<_> = paths
            .into_iter()
            .map(|path| (self.push(&path, probe::reading_source_info()), path))
            .collect();
        let weak = ui.as_weak();
        let cancel = self.probe_cancel.clone();
        let worker = tokio::spawn(async move {
            for (id, path) in pending {
                if cancel.stopped() {
                    break;
                }
                let info = probe::source_info(&ffmpeg, &path, &cancel)
                    .await
                    .unwrap_or_else(|_| probe::missing_source_info());
                let _ = weak.upgrade_in_event_loop(move |ui| {
                    let jobs = ui.get_job_list();
                    if let Some((index, mut job)) =
                        jobs.iter().enumerate().find(|(_, job)| job.id == id)
                    {
                        job.source_info = info;
                        jobs.set_row_data(index, job);
                    }
                });
            }
        });
        let mut handles = self.workers.lock().unwrap();
        handles.retain(|handle| !handle.is_finished());
        handles.push(worker);
    }
    fn remove(&self, index: usize) {
        self.files.remove(index);
        self.jobs.remove(index);
    }
    fn move_row(&self, from: usize, to: usize) {
        let a = self.jobs.row_data(from).unwrap();
        let b = self.jobs.row_data(to).unwrap();
        self.files.set_row_data(from, b.path.clone());
        self.files.set_row_data(to, a.path.clone());
        self.jobs.set_row_data(from, b);
        self.jobs.set_row_data(to, a);
    }
    fn scheduled(&self, retry: bool) -> Vec<(usize, PathBuf)> {
        (0..self.jobs.row_count())
            .filter_map(|index| {
                let job = self.jobs.row_data(index)?;
                let selected = if retry {
                    job.status == JobStatus::Failed
                } else {
                    matches!(job.status, JobStatus::Waiting | JobStatus::Cancelled)
                };
                selected.then(|| (index, PathBuf::from(job.path.as_str())))
            })
            .collect()
    }
}

fn update_job(ui: &MainWindow, index: usize, status: JobStatus, detail: LocalizedText) {
    let model = ui.get_job_list();
    if let Some(mut job) = model.row_data(index) {
        job.status = status;
        job.detail = detail;
        model.set_row_data(index, job);
    }
}
pub fn is_video_file(path: &Path) -> bool {
    path.is_file()
        && matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str(),
            "mp4" | "mkv" | "avi" | "mov" | "wmv" | "flv" | "mpg" | "mpeg" | "webm"
        )
}

fn start(
    ui: &MainWindow,
    queue: &Queue,
    retry: bool,
    cancel: Cancel,
    workers: &Arc<Mutex<Vec<JoinHandle<()>>>>,
    taskbar: TaskbarProgress,
) {
    if ui.get_is_encoding() || ui.get_edit_busy() || ui.get_conversion_busy() {
        return;
    }
    let ffmpeg = ui.get_ffmpeg_path().to_string();
    if ffmpeg.is_empty() {
        ui.set_current_file_text(message!(
            "Set the FFmpeg path.",
            "FFmpeg 경로를 설정하세요."
        ));
        return;
    }
    let suffix = ui.get_output_suffix().to_string();
    if suffix.contains(['/', '\\', ':']) {
        ui.set_current_file_text(message!(
            "The output suffix cannot contain path separators or colons.",
            "출력 Suffix에는 경로 문자를 사용할 수 없습니다.",
        ));
        return;
    }
    let paths = queue.scheduled(retry);
    if paths.is_empty() {
        ui.set_current_file_text(message!(
            "No jobs to run. You can add completed files again.",
            "실행할 작업이 없습니다. 완료 파일은 다시 추가할 수 있습니다.",
        ));
        return;
    }
    let options = settings::encode_options(ui);
    let out_folder = PathBuf::from(ui.get_output_folder().as_str());
    ui.set_is_encoding(true);
    ui.set_overall_progress(0.0);
    ui.set_file_progress(0.0);
    ui.set_error_details(LocalizedText::default());
    for (index, _) in &paths {
        update_job(ui, *index, JobStatus::Waiting, LocalizedText::default());
    }
    cancel.reset();
    let weak = ui.as_weak();
    let worker = tokio::spawn(async move {
        let total = paths.len();
        let batch_start = Instant::now();
        let mut successes = 0;
        let mut finished = 0;
        taskbar.set_progress(0.0);
        for (position, (index, src)) in paths.into_iter().enumerate() {
            if cancel.stopped() {
                break;
            }
            let stem = src.file_stem().unwrap_or_default().to_string_lossy();
            let output = output::directory(&src, &out_folder).join(format!("{stem}{suffix}.mp4"));
            let name = src
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let w = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    update_job(&ui, index, JobStatus::Running, LocalizedText::default());
                    ui.set_current_file_text(LocalizedText::same(format!(
                        "[{}/{}] {}",
                        position + 1,
                        total,
                        name
                    )));
                    ui.set_current_progress_text(message!("Analyzing…", "분석 중..."));
                    ui.set_file_progress(0.0);
                }
            });
            let file_start = Instant::now();
            let progress_weak = weak.clone();
            let progress_taskbar = taskbar.clone();
            let result = encoder::run(
                &ffmpeg,
                EncodeJob {
                    src,
                    output,
                    range: None,
                    encode: Some(options.clone()),
                    filter: None,
                    tracks: options.tracks,
                },
                &cancel,
                move |p| {
                    let overall = (position as f32 + p.fraction) / total as f32;
                    progress_taskbar.set_progress(overall);
                    let elapsed = file_start.elapsed().as_secs_f32();
                    let w = progress_weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            ui.set_overall_progress(overall);
                            ui.set_file_progress(p.fraction);
                            ui.set_current_progress_text(LocalizedText::same(format!(
                                "{:.1}% ({}, {})",
                                p.fraction * 100.0,
                                p.speed,
                                p.bitrate
                            )));
                            let elapsed_text = seconds_to_hms(elapsed);
                            let eta = if p.fraction > 0.01 {
                                LocalizedText::same(seconds_to_hms(
                                    (elapsed / p.fraction - elapsed).max(0.0),
                                ))
                            } else {
                                message!("Calculating…", "계산 중")
                            };
                            ui.set_current_time_text(LocalizedText::new(
                                format!("Elapsed {elapsed_text} / Remaining {}", eta.english),
                                format!("경과 {elapsed_text} / 남은 시간 {}", eta.korean),
                            ));
                            let total_time = seconds_to_hms(batch_start.elapsed().as_secs_f32());
                            ui.set_overall_time_text(message!(
                                "Total elapsed {total_time}",
                                "전체 경과 {total_time}"
                            ));
                        }
                    });
                },
            )
            .await;
            finished += 1;
            let stopped = cancel.stopped();
            let (status, detail) = match result {
                Ok(path) => {
                    successes += 1;
                    (
                        JobStatus::Done,
                        LocalizedText::same(path.display().to_string()),
                    )
                }
                Err(e) => (
                    if stopped {
                        JobStatus::Cancelled
                    } else {
                        JobStatus::Failed
                    },
                    diagnostic(&e),
                ),
            };
            let w = weak.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = w.upgrade() {
                    update_job(&ui, index, status, detail.clone());
                    if matches!(status, JobStatus::Failed | JobStatus::Cancelled) {
                        ui.set_error_details(detail);
                    }
                }
            });
            if stopped {
                break;
            }
        }
        taskbar.clear();
        let stopped = cancel.stopped();
        let _ = slint::invoke_from_event_loop(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_current_file_text(if stopped {
                    message!(
                        "Stopped: {successes}/{total} completed",
                        "중단: 완료 {successes}/{total}"
                    )
                } else {
                    message!(
                        "Finished: {successes}/{total} completed",
                        "처리 종료: 완료 {successes}/{total}"
                    )
                });
                ui.set_current_progress_text(LocalizedText::default());
                ui.set_current_time_text(LocalizedText::default());
                ui.set_overall_progress(finished as f32 / total as f32);
                ui.set_is_encoding(false);
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
    fn moving_and_removing_jobs_keeps_editor_list_and_retry_targets_in_sync() {
        let queue = Queue {
            files: Rc::new(VecModel::default()),
            jobs: Rc::new(VecModel::default()),
            next_id: Cell::new(0),
            probe_cancel: Cancel::default(),
            workers: Arc::new(Mutex::new(Vec::new())),
        };
        for name in ["pending.mp4", "done.mp4", "failed.mp4", "cancelled.mp4"] {
            queue.push(Path::new(name), LocalizedText::default());
        }
        for (index, status) in [
            (1, JobStatus::Done),
            (2, JobStatus::Failed),
            (3, JobStatus::Cancelled),
        ] {
            let mut job = queue.jobs.row_data(index).unwrap();
            job.status = status;
            job.detail = LocalizedText::same(format!("{status:?} detail"));
            queue.jobs.set_row_data(index, job);
        }
        queue.move_row(2, 1);
        queue.remove(2);
        assert_eq!(
            queue.scheduled(true),
            vec![(1, PathBuf::from("failed.mp4"))]
        );
        assert_eq!(
            queue.scheduled(false),
            vec![
                (0, PathBuf::from("pending.mp4")),
                (2, PathBuf::from("cancelled.mp4"))
            ]
        );
        for index in 0..queue.files.row_count() {
            assert_eq!(
                queue.files.row_data(index).unwrap(),
                queue.jobs.row_data(index).unwrap().path
            );
        }
        assert_eq!(
            queue.jobs.row_data(1).unwrap().detail.english,
            "Failed detail"
        );
    }
}

pub fn setup(
    ui: &MainWindow,
    queue: Rc<Queue>,
    cancel: Cancel,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    taskbar: TaskbarProgress,
) {
    let weak = ui.as_weak();
    ui.on_select_ffmpeg_path(move || {
        if let Some(ui) = weak.upgrade() {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter(
                    Language::current(&ui).select("Executable", "실행 파일"),
                    &["exe"],
                )
                .pick_file()
            {
                ui.set_ffmpeg_path(path.to_string_lossy().into_owned().into());
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_select_output_folder(move || {
        if let Some(path) = rfd::FileDialog::new().pick_folder() {
            if let Some(ui) = weak.upgrade() {
                ui.set_output_folder(path.to_string_lossy().into_owned().into());
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_reset_options(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_is_encoding() || ui.get_edit_busy() {
                return;
            }
            ui.set_output_suffix("_h265".into());
            ui.set_encoder_index(1);
            ui.set_crf_value(19.0);
            ui.set_cq_value(20.0);
            ui.set_output_folder("".into());
            ui.set_all_audio(true);
            ui.set_preserve_subtitles(false);
            ui.set_cut_mode(0);
        }
    });
    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_pick_files(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_is_encoding() {
                return;
            }
            if let Some(paths) = rfd::FileDialog::new()
                .add_filter(
                    Language::current(&ui).select("Video", "동영상"),
                    &[
                        "mp4", "mkv", "avi", "mov", "wmv", "flv", "mpg", "mpeg", "webm",
                    ],
                )
                .pick_files()
            {
                q.add_files(&ui, paths);
            }
        }
    });
    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_pick_folder(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_is_encoding() {
                return;
            }
            if let Some(path) = rfd::FileDialog::new().pick_folder() {
                if let Ok(entries) = std::fs::read_dir(path) {
                    let paths = entries
                        .flatten()
                        .map(|entry| entry.path())
                        .filter(|path| is_video_file(path))
                        .collect();
                    q.add_files(&ui, paths);
                }
            }
        }
    });
    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_clear_list(move || {
        if let Some(ui) = weak.upgrade() {
            if !ui.get_is_encoding() {
                while q.files.row_count() > 0 {
                    q.remove(0);
                }
                ui.set_selected_index(0);
            }
        }
    });
    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_remove_job(move |index| {
        if let Some(ui) = weak.upgrade() {
            if !ui.get_is_encoding() && index >= 0 && (index as usize) < q.files.row_count() {
                q.remove(index as usize);
                let selected = ui.get_selected_index();
                ui.set_selected_index(if selected > index {
                    selected - 1
                } else {
                    selected.min(q.files.row_count().saturating_sub(1) as i32)
                });
            }
        }
    });
    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_move_job(move |index, delta| {
        if let Some(ui) = weak.upgrade() {
            let target = index + delta;
            if !ui.get_is_encoding()
                && index >= 0
                && target >= 0
                && (index as usize) < q.files.row_count()
                && (target as usize) < q.files.row_count()
            {
                q.move_row(index as usize, target as usize);
                if ui.get_selected_index() == index {
                    ui.set_selected_index(target);
                } else if ui.get_selected_index() == target {
                    ui.set_selected_index(index);
                }
            }
        }
    });
    let signal = cancel.clone();
    ui.on_stop_encoding(move || signal.stop());
    let weak = ui.as_weak();
    let q = queue.clone();
    let c = cancel.clone();
    let w = workers.clone();
    let t = taskbar.clone();
    ui.on_start_encoding(move || {
        if let Some(ui) = weak.upgrade() {
            start(&ui, &q, false, c.clone(), &w, t.clone());
        }
    });
    let weak = ui.as_weak();
    ui.on_retry_failed(move || {
        if let Some(ui) = weak.upgrade() {
            start(&ui, &queue, true, cancel.clone(), &workers, taskbar.clone());
        }
    });
}
