use crate::{
    LocalizedText,
    encoder::{Cancel, command},
    language::message,
};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};

#[derive(Debug, Default, Deserialize)]
pub struct Stream {
    pub index: u32,
    #[serde(default)]
    pub codec_type: String,
    #[serde(default)]
    pub codec_name: String,
    #[serde(default)]
    pub pix_fmt: String,
    #[serde(default)]
    pub tags: StreamTags,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub avg_frame_rate: String,
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub nb_frames: Option<String>,
    #[serde(default)]
    pub disposition: Disposition,
    #[serde(default)]
    pub side_data_list: Vec<SideData>,
}
#[derive(Debug, Default, Deserialize)]
pub struct StreamTags {
    #[serde(default, alias = "ALPHA_MODE")]
    pub alpha_mode: Option<String>,
    #[serde(default)]
    title: String,
}
#[derive(Debug, Deserialize)]
pub struct SideData {
    #[serde(default)]
    pub rotation: Option<f64>,
}
#[derive(Debug, Default, Deserialize)]
pub struct Disposition {
    #[serde(default)]
    pub attached_pic: i32,
}
#[derive(Debug, Default, Deserialize)]
pub struct Format {
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub tags: FormatTags,
}
#[derive(Debug, Default, Deserialize)]
pub struct FormatTags {
    #[serde(default)]
    pub major_brand: String,
}
#[derive(Debug, Deserialize)]
pub struct Media {
    #[serde(default)]
    pub streams: Vec<Stream>,
    #[serde(default)]
    pub format: Format,
}
impl Media {
    pub fn video(&self) -> Result<&Stream> {
        self.streams
            .iter()
            .find(|s| s.codec_type == "video" && s.disposition.attached_pic == 0)
            .context(message!(
                "No video stream found",
                "비디오 스트림이 없습니다"
            ))
    }
    pub fn duration(&self) -> f32 {
        self.format
            .duration
            .as_deref()
            .and_then(|d| d.parse().ok())
            .or_else(|| self.video().ok()?.duration.as_deref()?.parse().ok())
            .unwrap_or(0.0)
    }
}

pub fn resolve_tool(ffmpeg: &str, name: &str) -> Option<PathBuf> {
    let name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_owned()
    };
    let resolved = which::which(ffmpeg).unwrap_or_else(|_| PathBuf::from(ffmpeg));
    let sibling = resolved.with_file_name(&name);
    sibling
        .is_file()
        .then_some(sibling)
        .or_else(|| which::which(name).ok())
}

pub async fn input_options(path: &Path) -> &'static [&'static str] {
    let Some(extension) = path.extension() else {
        return &[];
    };
    if extension.eq_ignore_ascii_case("jxl") {
        // The default image demuxer uses 25 fps, which rounds embedded JPEG XL timestamps.
        &["-f", "jpegxl_pipe", "-framerate", "1000"]
    } else if extension.eq_ignore_ascii_case("webp") && animated_webp(path).await {
        // Some FFmpeg builds let the still image demuxer win format detection.
        &["-f", "webp_anim"]
    } else {
        &[]
    }
}

async fn animated_webp(path: &Path) -> bool {
    let Ok(mut file) = tokio::fs::File::open(path).await else {
        return false;
    };
    let mut header = [0; 21];
    file.read_exact(&mut header).await.is_ok()
        && &header[..4] == b"RIFF"
        && &header[8..16] == b"WEBPVP8X"
        && header[20] & 0x02 != 0
}

pub async fn analyze(ffmpeg: &str, path: &Path, cancel: &Cancel) -> Result<Media> {
    let ffprobe = resolve_tool(ffmpeg, "ffprobe").context(message!(
        "ffprobe was not found. Install it next to FFmpeg or on PATH.",
        "ffprobe를 찾을 수 없습니다. FFmpeg와 같은 폴더 또는 PATH에 설치하세요.",
    ))?;
    let mut cmd = command(ffprobe);
    cmd.args([
        "-v",
        "error",
        "-show_streams",
        "-show_format",
        "-of",
        "json",
    ])
    .args(input_options(path).await)
    .arg(path)
    .stdin(Stdio::null());
    let output = tokio::select! {
        result = cmd.output() => result.context(message!("Could not run ffprobe", "FFprobe 실행 실패"))?,
        _ = cancel.cancelled() => anyhow::bail!(message!("Job cancelled", "작업 취소됨")),
    };
    ensure!(
        output.status.success(),
        message!(
            "ffprobe failed: {}",
            "FFprobe 실패: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    );
    serde_json::from_slice(&output.stdout).context(message!(
        "Could not parse ffprobe JSON",
        "FFprobe JSON 분석 실패"
    ))
}

async fn stream_duration(
    ffmpeg: &str,
    path: &Path,
    stream: &Stream,
    cancel: &Cancel,
) -> Option<f64> {
    let ffprobe = resolve_tool(ffmpeg, "ffprobe")?;
    let entries = if stream.codec_name == "jpegxl" {
        // JPEG XL can contain many decoded frames in one demuxed packet.
        "frame=pts_time,duration_time"
    } else {
        "packet=pts_time,duration_time"
    };
    let mut child = command(ffprobe)
        .args(["-v", "error", "-select_streams"])
        .arg(stream.index.to_string())
        .args(["-show_entries", entries, "-of", "csv=p=0"])
        .args(input_options(path).await)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut lines = BufReader::new(child.stdout.take()?).lines();
    let duration = async {
        let mut start = f64::INFINITY;
        let mut end = f64::NEG_INFINITY;
        while let Some(line) = lines.next_line().await.ok()? {
            if let Some((pts, duration)) = line.split_once(',') {
                if let (Ok(pts), Ok(duration)) = (pts.parse::<f64>(), duration.parse::<f64>()) {
                    start = start.min(pts);
                    end = end.max(pts + duration);
                }
            }
        }
        let duration = end - start;
        (child.wait().await.ok()?.success() && duration.is_finite() && duration > 0.0)
            .then_some(duration)
    };
    tokio::select! {
        result = duration => result,
        _ = cancel.cancelled() => None,
    }
}

pub fn source_video(media: &Media) -> Result<&Stream> {
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

pub fn still_avif_alpha<'a>(media: &'a Media, color: &Stream) -> Option<&'a Stream> {
    if media.format.tags.major_brand != "avif" {
        return None;
    }
    media.streams.iter().find(|stream| {
        stream.index != color.index
            && stream.codec_name == "av1"
            && stream.pix_fmt.starts_with("gray")
            && stream.width == color.width
            && stream.height == color.height
            // FFmpeg exposes this AVIF auxiliary image as a separate stream.
            && stream.tags.title.eq_ignore_ascii_case("alpha")
    })
}

pub fn source_duration(media: &Media, video: &Stream) -> Option<f64> {
    [&video.duration, &media.format.duration]
        .into_iter()
        .filter_map(|value| value.as_deref()?.parse::<f64>().ok())
        .find(|duration| duration.is_finite() && *duration > 0.0)
}

pub fn reading_source_info() -> LocalizedText {
    message!("Reading source information…", "원본 정보 읽는 중…")
}

pub fn missing_source_info() -> LocalizedText {
    message!(
        "Could not read source information.",
        "원본 정보를 읽을 수 없습니다."
    )
}

pub async fn source_info(ffmpeg: &str, path: &Path, cancel: &Cancel) -> Result<LocalizedText> {
    let media = analyze(ffmpeg, path, cancel).await?;
    let video = source_video(&media)?;
    let mut duration = source_duration(&media, video);
    if duration.is_none() {
        duration = stream_duration(ffmpeg, path, video, cancel).await;
    }
    let size = source_size(video);
    let time = if let Some(duration) = duration {
        let ms = (duration * 1000.0).round() as u64;
        let seconds = ms / 1000;
        LocalizedText::same(format!(
            "{:02}:{:02}:{:02}.{:03}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60,
            ms % 1000
        ))
    } else {
        message!("Unknown duration", "길이 확인 불가")
    };
    Ok(LocalizedText::new(
        format!("{} · {}", size.english, time.english),
        format!("{} · {}", size.korean, time.korean),
    ))
}

pub async fn image_info(ffmpeg: &str, path: &Path, cancel: &Cancel) -> Result<LocalizedText> {
    let media = analyze(ffmpeg, path, cancel).await?;
    Ok(source_size(source_video(&media)?))
}

fn source_size(video: &Stream) -> LocalizedText {
    let rotation = video.side_data_list.iter().find_map(|side| side.rotation);
    let (width, height) = if rotation.is_some_and(|angle| angle.abs() % 180.0 == 90.0) {
        (video.height, video.width)
    } else {
        (video.width, video.height)
    };
    if width > 0 && height > 0 {
        LocalizedText::same(format!("{width} × {height} px"))
    } else {
        message!("Unknown resolution", "해상도 확인 불가")
    }
}

pub async fn probe_video(ffmpeg: &str, path: &Path) -> Result<(f32, f32, f32, f32)> {
    let media = analyze(ffmpeg, path, &Cancel::default()).await?;
    let video = media.video()?;
    let fps = video
        .avg_frame_rate
        .split_once('/')
        .and_then(|(n, d)| {
            let n: f32 = n.parse().ok()?;
            let d: f32 = d.parse().ok()?;
            (d > 0.0 && n > 0.0).then_some(n / d)
        })
        .unwrap_or(30.0);
    ensure!(
        video.width > 0 && video.height > 0 && media.duration() > 0.0,
        message!(
            "Could not read video duration or resolution",
            "영상 길이/해상도를 읽을 수 없습니다"
        )
    );
    Ok((
        media.duration(),
        video.width as f32,
        video.height as f32,
        fps,
    ))
}
