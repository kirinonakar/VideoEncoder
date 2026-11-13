use super::{AnimationFormat, Cancel, ConversionJob, Resize, SOURCE_EXTENSIONS};
use crate::{
    ConversionItem, JobStatus, LocalizedText, MainWindow,
    language::{Language, diagnostic, message},
    probe,
    windows::TaskbarProgress,
};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use std::{
    cell::Cell,
    fs,
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;

type Workers = Arc<Mutex<Vec<JoinHandle<()>>>>;

struct Queue {
    jobs: Rc<VecModel<ConversionItem>>,
    next_id: Cell<i32>,
    probe_cancel: Cancel,
    workers: Workers,
    completed_options: Cell<Option<OutputOptions>>,
}

#[derive(PartialEq, Eq)]
struct OutputOptions {
    format: i32,
    width: SharedString,
    height: SharedString,
}

impl OutputOptions {
    fn read(ui: &MainWindow) -> Self {
        Self {
            format: ui.get_conversion_format(),
            width: ui.get_conversion_width(),
            height: ui.get_conversion_height(),
        }
    }
}

fn update_job(ui: &MainWindow, id: i32, update: impl FnOnce(&mut ConversionItem)) {
    let jobs = ui.get_conversion_jobs();
    if let Some((index, mut job)) = jobs.iter().enumerate().find(|(_, job)| job.id == id) {
        update(&mut job);
        jobs.set_row_data(index, job);
    }
}

fn folder_files(folder: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = fs::read_dir(folder)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| {
                        SOURCE_EXTENSIONS
                            .iter()
                            .any(|supported| ext.eq_ignore_ascii_case(supported))
                    })
        })
        .collect();
    files.sort();
    files
}

fn track(workers: &Workers, worker: JoinHandle<()>) {
    let mut handles = workers.lock().unwrap();
    handles.retain(|handle| !handle.is_finished());
    handles.push(worker);
}

impl Queue {
    fn requeue_completed(&self, ui: &MainWindow) {
        let Some(completed_options) = self.completed_options.take() else {
            return;
        };
        if completed_options == OutputOptions::read(ui) {
            self.completed_options.set(Some(completed_options));
            return;
        }
        let mut changed = false;
        for (index, mut job) in self.jobs.iter().enumerate() {
            if job.status == JobStatus::Done {
                job.status = JobStatus::Waiting;
                job.detail = LocalizedText::default();
                self.jobs.set_row_data(index, job);
                changed = true;
            }
        }
        if changed {
            ui.set_conversion_status(message!(
                "Output settings changed. Completed files were added back to the queue.",
                "출력 설정이 변경되어 완료 파일을 대기 목록으로 돌렸습니다.",
            ));
            ui.set_conversion_detail(LocalizedText::default());
            ui.set_conversion_progress(0.0);
        }
    }

    fn add(&self, ui: &MainWindow, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            ui.set_conversion_status(message!(
                "No video or animation files to add.",
                "추가할 동영상이나 애니메이션 파일이 없습니다.",
            ));
            return;
        }
        ui.set_conversion_status(LocalizedText::default());
        ui.set_conversion_detail(LocalizedText::default());
        ui.set_conversion_progress(0.0);
        let ffmpeg = ui.get_ffmpeg_path().to_string();
        let mut added = Vec::new();
        for path in paths {
            let id = self.next_id.get();
            self.next_id.set(id + 1);
            self.jobs.push(ConversionItem {
                id,
                path: path.to_string_lossy().as_ref().into(),
                source_info: if ffmpeg.is_empty() {
                    message!(
                        "Set the FFmpeg path on the Encode tab.",
                        "인코딩 탭에서 FFmpeg 경로를 설정하세요."
                    )
                } else {
                    probe::reading_source_info()
                },
                status: JobStatus::Waiting,
                detail: LocalizedText::default(),
            });
            added.push((id, path));
        }
        if ffmpeg.is_empty() {
            return;
        }
        let weak = ui.as_weak();
        let cancel = self.probe_cancel.clone();
        track(
            &self.workers,
            tokio::spawn(async move {
                for (id, path) in added {
                    if cancel.stopped() {
                        break;
                    }
                    let info = probe::source_info(&ffmpeg, &path, &cancel)
                        .await
                        .unwrap_or_else(|_| probe::missing_source_info());
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        update_job(&ui, id, |job| job.source_info = info);
                    });
                }
            }),
        );
    }

    fn start(&self, ui: &MainWindow, retry: bool, cancel: Cancel, taskbar: TaskbarProgress) {
        if ui.get_conversion_busy() || ui.get_is_encoding() || ui.get_edit_busy() {
            return;
        }
        self.requeue_completed(ui);
        ui.set_conversion_detail(LocalizedText::default());
        let ffmpeg = ui.get_ffmpeg_path().to_string();
        if ffmpeg.is_empty() {
            ui.set_conversion_status(message!(
                "Set the FFmpeg path on the Encode tab.",
                "인코딩 탭에서 FFmpeg 경로를 설정하세요."
            ));
            return;
        }
        let options = OutputOptions::read(ui);
        let settings = AnimationFormat::from_index(options.format).and_then(|format| {
            Resize::parse(options.width.as_str(), options.height.as_str())
                .map(|resize| (format, resize))
        });
        let (format, resize) = match settings {
            Ok(settings) => settings,
            Err(error) => {
                ui.set_conversion_status(diagnostic(&error));
                return;
            }
        };
        let folder = PathBuf::from(ui.get_output_folder().as_str());
        let jobs: Vec<_> = self
            .jobs
            .iter()
            .filter(|job| {
                if retry {
                    job.status == JobStatus::Failed
                } else {
                    matches!(job.status, JobStatus::Waiting | JobStatus::Cancelled)
                }
            })
            .map(|job| {
                (
                    job.id,
                    ConversionJob {
                        source: PathBuf::from(job.path.as_str()),
                        output_folder: folder.clone(),
                        format,
                        resize,
                    },
                )
            })
            .collect();
        if jobs.is_empty() {
            ui.set_conversion_status(message!(
                "No conversion jobs to run.",
                "실행할 변환 작업이 없습니다."
            ));
            return;
        }
        for (id, _) in &jobs {
            update_job(ui, *id, |job| {
                job.status = JobStatus::Waiting;
                job.detail = LocalizedText::default();
            });
        }
        cancel.reset();
        self.completed_options.set(Some(options));
        ui.set_conversion_progress(0.0);
        ui.set_conversion_busy(true);
        ui.set_conversion_status(message!("Converting…", "변환 중…"));
        let weak = ui.as_weak();
        track(
            &self.workers,
            tokio::spawn(run_batch(weak, ffmpeg, jobs, cancel, taskbar)),
        );
    }
}

async fn run_batch(
    weak: slint::Weak<MainWindow>,
    ffmpeg: String,
    jobs: Vec<(i32, ConversionJob)>,
    cancel: Cancel,
    taskbar: TaskbarProgress,
) {
    let total = jobs.len();
    let mut completed = 0;
    let mut failed = 0;
    let mut processed = 0;
    taskbar.set_progress(0.0);
    for (index, (id, job)) in jobs.into_iter().enumerate() {
        if cancel.stopped() {
            break;
        }
        let _ = weak.upgrade_in_event_loop(move |ui| {
            update_job(&ui, id, |job| job.status = JobStatus::Running);
            ui.set_conversion_status(message!(
                "Converting · {}/{total}",
                "변환 중 · {}/{total}",
                index + 1
            ));
        });
        let result = job
            .run(&ffmpeg, &cancel, |progress| {
                let fraction = (index as f32 + progress.fraction) / total as f32;
                taskbar.set_progress(fraction);
                let _ = weak.upgrade_in_event_loop(move |ui| ui.set_conversion_progress(fraction));
            })
            .await;
        let (status, detail) = match result {
            Ok(path) => {
                completed += 1;
                (
                    JobStatus::Done,
                    LocalizedText::same(path.display().to_string()),
                )
            }
            Err(error) if cancel.stopped() => (JobStatus::Cancelled, diagnostic(&error)),
            Err(error) => {
                failed += 1;
                (JobStatus::Failed, diagnostic(&error))
            }
        };
        processed = completed + failed;
        let fraction = processed as f32 / total as f32;
        taskbar.set_progress(fraction);
        let _ = weak.upgrade_in_event_loop(move |ui| {
            update_job(&ui, id, |job| {
                job.status = status;
                job.detail = detail;
            });
            ui.set_conversion_progress(fraction);
        });
    }
    taskbar.clear();
    let stopped = cancel.stopped();
    let _ = weak.upgrade_in_event_loop(move |ui| {
        ui.set_conversion_status(if stopped {
            message!(
                "Conversion stopped · {completed}/{total} completed · {failed} failed",
                "변환 중단 · 완료 {completed}/{total} · 실패 {failed}",
            )
        } else {
            message!(
                "Conversion finished · {completed}/{total} completed · {failed} failed",
                "변환 종료 · 완료 {completed}/{total} · 실패 {failed}",
            )
        });
        ui.set_conversion_progress(processed as f32 / total as f32);
        ui.set_conversion_busy(false);
    });
}

pub fn setup(
    ui: &MainWindow,
    cancel: Cancel,
    probe_cancel: Cancel,
    workers: Workers,
    taskbar: TaskbarProgress,
) {
    let queue = Rc::new(Queue {
        jobs: Rc::new(VecModel::default()),
        next_id: Cell::new(0),
        probe_cancel,
        workers,
        completed_options: Cell::new(None),
    });
    ui.set_conversion_jobs(ModelRc::from(queue.jobs.clone()));

    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_conversion_options_changed(move || {
        if let Some(ui) = weak.upgrade().filter(|ui| !ui.get_conversion_busy()) {
            q.requeue_completed(&ui);
        }
    });

    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_pick_conversion_files(move || {
        if let Some(ui) = weak.upgrade().filter(|ui| !ui.get_conversion_busy()) {
            let language = Language::current(&ui);
            if let Some(paths) = rfd::FileDialog::new()
                .add_filter(
                    language.select("Videos and animations", "동영상 및 애니메이션"),
                    SOURCE_EXTENSIONS,
                )
                .add_filter(language.select("All files", "모든 파일"), &["*"])
                .pick_files()
            {
                q.add(&ui, paths);
            }
        }
    });
    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_pick_conversion_folder(move || {
        if let Some(ui) = weak.upgrade().filter(|ui| !ui.get_conversion_busy()) {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                q.add(&ui, folder_files(&folder));
            }
        }
    });
    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_conversion_files_dropped(move |paths| {
        if let Some(ui) = weak.upgrade().filter(|ui| !ui.get_conversion_busy()) {
            let mut files = Vec::new();
            for path in paths.split('|').map(Path::new) {
                if path.is_dir() {
                    files.extend(folder_files(path));
                } else if path.is_file() {
                    files.push(path.to_owned());
                }
            }
            q.add(&ui, files);
        }
    });
    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_remove_conversion_job(move |index| {
        if let Some(ui) = weak.upgrade().filter(|ui| !ui.get_conversion_busy()) {
            if (index as usize) < q.jobs.row_count() {
                q.jobs.remove(index as usize);
                ui.set_conversion_detail(LocalizedText::default());
            }
        }
    });
    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_move_conversion_job(move |index, delta| {
        if weak.upgrade().is_some_and(|ui| !ui.get_conversion_busy()) {
            let Some(target) = index.checked_add(delta) else {
                return;
            };
            let (index, target) = (index as usize, target as usize);
            if let (Some(a), Some(b)) = (q.jobs.row_data(index), q.jobs.row_data(target)) {
                q.jobs.set_row_data(index, b);
                q.jobs.set_row_data(target, a);
            }
        }
    });
    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_clear_conversion_list(move || {
        if let Some(ui) = weak.upgrade().filter(|ui| !ui.get_conversion_busy()) {
            q.jobs.set_vec(Vec::new());
            ui.set_conversion_detail(LocalizedText::default());
            ui.set_conversion_status(LocalizedText::default());
            ui.set_conversion_progress(0.0);
        }
    });
    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_show_conversion_result(move |index| {
        if let Some(ui) = weak.upgrade() {
            if let Some(job) = q.jobs.row_data(index as usize) {
                ui.set_conversion_detail(job.detail);
            }
        }
    });
    let stop = cancel.clone();
    ui.on_cancel_conversion(move || stop.stop());
    let weak = ui.as_weak();
    ui.on_start_conversion(move |retry| {
        if let Some(ui) = weak.upgrade() {
            queue.start(&ui, retry, cancel.clone(), taskbar.clone());
        }
    });
}
