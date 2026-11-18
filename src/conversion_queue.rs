use super::{
    ANIMATION_SOURCE_EXTENSIONS, AnimationFormat, Cancel, ConversionFormat, ConversionJob,
    IMAGE_SOURCE_EXTENSIONS, ImageFormat, ImageOptions, Resize,
};
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
    image_mode: bool,
    format: i32,
    lossless: bool,
    quality: i32,
    compression: usize,
    width: SharedString,
    height: SharedString,
    folder: SharedString,
    suffix: SharedString,
}

impl OutputOptions {
    fn read(ui: &MainWindow) -> Self {
        let image_mode = ui.get_conversion_mode() == 1;
        Self {
            image_mode,
            format: if image_mode {
                ui.get_image_format()
            } else {
                ui.get_conversion_format()
            },
            lossless: image_mode && ui.get_image_lossless(),
            quality: if image_mode {
                ui.get_image_quality().round().clamp(1.0, 100.0) as i32
            } else {
                0
            },
            compression: if image_mode {
                ui.get_image_compression().clamp(0, 2) as usize
            } else {
                0
            },
            width: ui.get_conversion_width(),
            height: ui.get_conversion_height(),
            folder: ui.get_conversion_output_folder(),
            suffix: ui.get_conversion_suffix(),
        }
    }

    fn format(&self) -> anyhow::Result<ConversionFormat> {
        if self.image_mode {
            Ok(ConversionFormat::Image(ImageOptions {
                format: ImageFormat::from_index(self.format)?,
                lossless: self.lossless,
                quality: self.quality,
                compression: self.compression,
            }))
        } else {
            AnimationFormat::from_index(self.format).map(ConversionFormat::Animation)
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

fn source_extensions(ui: &MainWindow) -> &'static [&'static str] {
    if ui.get_conversion_mode() == 1 {
        IMAGE_SOURCE_EXTENSIONS
    } else {
        ANIMATION_SOURCE_EXTENSIONS
    }
}

fn folder_files(folder: &Path, extensions: &[&str]) -> Vec<PathBuf> {
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
                        extensions
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
            ui.set_conversion_status(if ui.get_conversion_mode() == 1 {
                message!("No image files to add.", "추가할 이미지 파일이 없습니다.")
            } else {
                message!(
                    "No video or animation files to add.",
                    "추가할 동영상이나 애니메이션 파일이 없습니다.",
                )
            });
            return;
        }
        ui.set_conversion_status(LocalizedText::default());
        ui.set_conversion_detail(LocalizedText::default());
        ui.set_conversion_progress(0.0);
        let ffmpeg = ui.get_ffmpeg_path().to_string();
        let image_mode = ui.get_conversion_mode() == 1;
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
                    let info = if image_mode {
                        probe::image_info(&ffmpeg, &path, &cancel).await
                    } else {
                        probe::source_info(&ffmpeg, &path, &cancel).await
                    }
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
        if options
            .suffix
            .contains(['<', '>', ':', '"', '/', '\\', '|', '?', '*'])
            || options.suffix.chars().any(char::is_control)
        {
            ui.set_conversion_status(message!(
                "Enter a suffix without characters that are invalid in filenames.",
                "출력 접미사에 파일 이름으로 사용할 수 없는 문자가 있습니다."
            ));
            return;
        }
        let settings = options.format().and_then(|format| {
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
        let folder = PathBuf::from(options.folder.as_str());
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
                        suffix: options.suffix.to_string(),
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
    ui.on_select_conversion_output_folder(move || {
        if let Some(ui) = weak.upgrade().filter(|ui| !ui.get_conversion_busy()) {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                ui.set_conversion_output_folder(folder.to_string_lossy().into_owned().into());
            }
        }
    });

    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_pick_conversion_files(move || {
        if let Some(ui) = weak.upgrade().filter(|ui| !ui.get_conversion_busy()) {
            let language = Language::current(&ui);
            let label = if ui.get_conversion_mode() == 1 {
                language.select("Images", "이미지")
            } else {
                language.select("Videos and animations", "동영상 및 애니메이션")
            };
            if let Some(paths) = rfd::FileDialog::new()
                .add_filter(label, source_extensions(&ui))
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
                q.add(&ui, folder_files(&folder, source_extensions(&ui)));
            }
        }
    });
    let weak = ui.as_weak();
    let q = queue.clone();
    ui.on_conversion_files_dropped(move |paths| {
        if let Some(ui) = weak.upgrade().filter(|ui| !ui.get_conversion_busy()) {
            let mut files = Vec::new();
            let extensions = source_extensions(&ui);
            for path in paths.split('|').map(Path::new) {
                if path.is_dir() {
                    files.extend(folder_files(path, extensions));
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
    ui.on_remove_completed_conversion_jobs(move || {
        if let Some(ui) = weak.upgrade().filter(|ui| !ui.get_conversion_busy()) {
            for index in (0..q.jobs.row_count()).rev() {
                if q.jobs
                    .row_data(index)
                    .is_some_and(|job| job.status == JobStatus::Done)
                {
                    ui.invoke_remove_conversion_job(index as i32);
                }
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
