use crate::{
    MainWindow,
    encoder::{self, Cancel, Progress},
    output, probe,
    windows::TaskbarProgress,
};
use anyhow::{Context, Result, ensure};
use slint::ComponentHandle;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use tokio::task::JoinHandle;

const SOURCE_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "mpg", "mpeg", "webm", "m4v", "gif", "webp", "avif",
    "apng", "png",
];

#[derive(Clone, Copy)]
enum AnimationFormat {
    Gif,
    Webp,
    Avif,
}

impl AnimationFormat {
    fn from_index(index: i32) -> Result<Self> {
        match index {
            0 => Ok(Self::Gif),
            1 => Ok(Self::Webp),
            2 => Ok(Self::Avif),
            _ => anyhow::bail!("출력 형식을 선택하세요."),
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Avif => "avif",
        }
    }
}

#[derive(Clone, Copy)]
struct Resize {
    width: Option<i32>,
    height: Option<i32>,
}

impl Resize {
    fn parse(width: &str, height: &str) -> Result<Self> {
        fn dimension(value: &str, label: &str) -> Result<Option<i32>> {
            let value = value.trim();
            if value.is_empty() {
                return Ok(None);
            }
            let size = value
                .parse::<i32>()
                .ok()
                .filter(|size| *size > 0)
                .with_context(|| format!("{label} 크기는 1 이상의 정수로 입력하세요."))?;
            Ok(Some(size))
        }
        Ok(Self {
            width: dimension(width, "가로")?,
            height: dimension(height, "세로")?,
        })
    }

    fn filter(self) -> String {
        let (width, height) = match (self.width, self.height) {
            (None, None) => return "null".into(),
            (Some(width), Some(height)) => (width.to_string(), height.to_string()),
            (Some(width), None) => (width.to_string(), "'max(1,round(ow/dar))'".into()),
            (None, Some(height)) => ("'max(1,round(oh*dar))'".into(), height.to_string()),
        };
        format!("scale={width}:{height}:flags=lanczos,setsar=1")
    }
}

struct ConversionJob {
    source: PathBuf,
    output_folder: PathBuf,
    format: AnimationFormat,
    resize: Resize,
}

impl ConversionJob {
    fn output_path(&self) -> PathBuf {
        let mut name = PathBuf::from(self.source.file_name().unwrap_or_default());
        name.set_extension(self.format.extension());
        output::directory(&self.source, &self.output_folder).join(name)
    }

    async fn run(
        &self,
        ffmpeg: &str,
        cancel: &Cancel,
        on_progress: impl Fn(Progress),
    ) -> Result<PathBuf> {
        let media = probe::analyze(ffmpeg, &self.source, cancel).await?;
        let mut video = media.video()?;
        if media.format.tags.major_brand == "avis" {
            // FFprobe can list the primary still image before the animation track.
            if let Some(animation) = media.streams.iter().find(|stream| {
                stream.codec_type == "video"
                    && stream.disposition.attached_pic == 0
                    && stream
                        .nb_frames
                        .as_deref()
                        .and_then(|count| count.parse::<u64>().ok())
                        .is_some_and(|count| count > 1)
            }) {
                video = animation;
            }
        }
        let mut cmd = encoder::command(ffmpeg);
        cmd.args([
            "-hide_banner",
            "-nostdin",
            "-nostats",
            "-y",
            "-abort_on",
            "empty_output",
            "-xerror",
        ]);
        cmd.arg("-i").arg(&self.source);
        let filter = self.resize.filter();
        if matches!(self.format, AnimationFormat::Gif) {
            // Generate a palette per frame to avoid buffering the whole video.
            cmd.arg("-filter_complex").arg(format!(
                "[0:{}]{filter},split[frames][colors];[colors]palettegen=stats_mode=single[palette];[frames][palette]paletteuse=new=1[gif]",
                video.index
            ));
            cmd.args(["-map", "[gif]"]);
        } else {
            cmd.arg("-map").arg(format!("0:{}", video.index));
            cmd.arg("-vf").arg(filter);
        }
        match self.format {
            AnimationFormat::Gif => cmd.args(["-c:v", "gif"]),
            AnimationFormat::Webp => cmd.args(["-c:v", "libwebp_anim"]),
            AnimationFormat::Avif => cmd.args([
                "-c:v",
                "libaom-av1",
                "-crf",
                "30",
                "-b:v",
                "0",
                "-cpu-used",
                "6",
                "-pix_fmt",
                "yuv420p",
            ]),
        };
        cmd.args(["-fps_mode", "passthrough", "-enc_time_base", "1:1000"]);
        cmd.args(["-loop", "0", "-f", self.format.extension()]);
        let duration = video
            .duration
            .as_deref()
            .and_then(|value| value.parse::<f32>().ok())
            .filter(|duration| *duration > 0.0)
            .unwrap_or_else(|| media.duration());
        encoder::run_to_output(cmd, &self.output_path(), duration, cancel, on_progress).await
    }
}

fn select_source(ui: &MainWindow, path: &Path) {
    ui.set_conversion_source(path.to_string_lossy().as_ref().into());
    ui.set_conversion_status("".into());
    ui.set_conversion_detail("".into());
    ui.set_conversion_progress(0.0);
}

pub fn select_dropped_source(ui: &MainWindow, paths: &str) {
    if ui.get_conversion_busy() {
        return;
    }
    if let Some(path) = paths.split('|').map(Path::new).find(|path| path.is_file()) {
        select_source(ui, path);
    } else {
        ui.set_conversion_status("변환할 파일을 놓아 주세요.".into());
    }
}

pub fn setup(
    ui: &MainWindow,
    cancel: Cancel,
    workers: Arc<Mutex<Vec<JoinHandle<()>>>>,
    taskbar: TaskbarProgress,
) {
    let weak = ui.as_weak();
    ui.on_pick_conversion_source(move || {
        let Some(ui) = weak.upgrade() else { return };
        if !ui.get_conversion_busy() {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("동영상 및 애니메이션", SOURCE_EXTENSIONS)
                .add_filter("모든 파일", &["*"])
                .pick_file()
            {
                select_source(&ui, &path);
            }
        }
    });

    let stop = cancel.clone();
    ui.on_cancel_conversion(move || stop.stop());

    let weak = ui.as_weak();
    ui.on_start_conversion(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_conversion_busy() || ui.get_is_encoding() || ui.get_edit_busy() {
            return;
        }
        ui.set_conversion_detail("".into());
        ui.set_conversion_progress(0.0);
        let ffmpeg = ui.get_ffmpeg_path().to_string();
        let job = (|| -> Result<ConversionJob> {
            ensure!(
                !ffmpeg.is_empty(),
                "인코딩 탭에서 FFmpeg 경로를 설정하세요."
            );
            let source = PathBuf::from(ui.get_conversion_source().as_str());
            ensure!(source.is_file(), "변환할 파일을 선택하세요.");
            Ok(ConversionJob {
                source,
                output_folder: PathBuf::from(ui.get_output_folder().as_str()),
                format: AnimationFormat::from_index(ui.get_conversion_format())?,
                resize: Resize::parse(
                    ui.get_conversion_width().as_str(),
                    ui.get_conversion_height().as_str(),
                )?,
            })
        })();
        let job = match job {
            Ok(job) => job,
            Err(error) => {
                ui.set_conversion_status(error.to_string().into());
                return;
            }
        };
        cancel.reset();
        ui.set_conversion_busy(true);
        ui.set_conversion_status("변환 중…".into());
        let cancel = cancel.clone();
        let taskbar = taskbar.clone();
        let weak = weak.clone();
        let worker = tokio::spawn(async move {
            taskbar.set_progress(0.0);
            let result = job
                .run(&ffmpeg, &cancel, |progress| {
                    let fraction = progress.fraction;
                    taskbar.set_progress(fraction);
                    let _ = weak.upgrade_in_event_loop(move |ui| {
                        ui.set_conversion_progress(fraction);
                    });
                })
                .await;
            taskbar.clear();
            let (status, detail, completed) = match result {
                Ok(path) => ("변환 완료", path.display().to_string(), true),
                Err(error) => (
                    if cancel.stopped() {
                        "변환 취소"
                    } else {
                        "변환 실패"
                    },
                    format!("{error:#}"),
                    false,
                ),
            };
            let _ = weak.upgrade_in_event_loop(move |ui| {
                ui.set_conversion_progress(if completed { 1.0 } else { 0.0 });
                ui.set_conversion_status(status.into());
                ui.set_conversion_detail(detail.into());
                ui.set_conversion_busy(false);
            });
        });
        let mut handles = workers.lock().unwrap();
        handles.retain(|handle| !handle.is_finished());
        handles.push(worker);
    });
}

#[cfg(test)]
#[path = "conversion_tests.rs"]
mod tests;
