use crate::{
    encoder::{self, Cancel, Progress},
    output, probe,
};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

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

fn source_video(media: &probe::Media) -> Result<&probe::Stream> {
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
            return Ok(animation);
        }
    }
    media.video()
}

fn source_duration(media: &probe::Media, video: &probe::Stream) -> Option<f64> {
    [&video.duration, &media.format.duration]
        .into_iter()
        .filter_map(|value| value.as_deref()?.parse::<f64>().ok())
        .find(|duration| duration.is_finite() && *duration > 0.0)
}

async fn source_info(ffmpeg: &str, path: &Path, cancel: &Cancel) -> Result<String> {
    let media = probe::analyze(ffmpeg, path, cancel).await?;
    let video = source_video(&media)?;
    let mut duration = source_duration(&media, video);
    if duration.is_none() {
        duration = probe::packet_duration(ffmpeg, path, video.index, cancel).await;
    }
    let rotation = video.side_data_list.iter().find_map(|side| side.rotation);
    let (width, height) = if rotation.is_some_and(|angle| angle.abs() % 180.0 == 90.0) {
        (video.height, video.width)
    } else {
        (video.width, video.height)
    };
    let size = if width > 0 && height > 0 {
        format!("{width} × {height} px")
    } else {
        "해상도 확인 불가".into()
    };
    let time = if let Some(duration) = duration {
        let ms = (duration * 1000.0).round() as u64;
        let seconds = ms / 1000;
        format!(
            "{:02}:{:02}:{:02}.{:03}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60,
            ms % 1000
        )
    } else {
        "길이 확인 불가".into()
    };
    Ok(format!("{size} · {time}"))
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
        let video = source_video(&media)?;
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
            cmd.arg("-vf")
                .arg(if matches!(self.format, AnimationFormat::Webp) {
                    format!("format=bgra,{filter}")
                } else {
                    filter
                });
        }
        match self.format {
            AnimationFormat::Gif => cmd.args(["-c:v", "gif"]),
            AnimationFormat::Webp => cmd.args([
                "-c:v",
                "libwebp_anim",
                "-lossless",
                "0",
                "-quality",
                "95",
                "-compression_level",
                "6",
                "-pix_fmt",
                "bgra",
            ]),
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
        let duration = source_duration(&media, video).unwrap_or_default() as f32;
        encoder::run_to_output(cmd, &self.output_path(), duration, cancel, on_progress).await
    }
}

#[path = "conversion_queue.rs"]
mod queue;
pub use queue::setup;

#[cfg(test)]
#[path = "conversion_tests.rs"]
mod tests;
