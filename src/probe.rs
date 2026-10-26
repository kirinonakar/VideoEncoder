use crate::encoder::{Cancel, command};
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
};

#[derive(Debug, Default, Deserialize)]
pub struct Stream {
    pub index: u32,
    #[serde(default)]
    pub codec_type: String,
    #[serde(default)]
    pub codec_name: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    #[serde(default)]
    pub avg_frame_rate: String,
    #[serde(default)]
    pub duration: Option<String>,
    #[serde(default)]
    pub disposition: Disposition,
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
            .context("비디오 스트림이 없습니다")
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

pub async fn analyze(ffmpeg: &str, path: &Path, cancel: &Cancel) -> Result<Media> {
    let ffprobe = resolve_tool(ffmpeg, "ffprobe")
        .context("ffprobe를 찾을 수 없습니다. FFmpeg와 같은 폴더 또는 PATH에 설치하세요.")?;
    let mut cmd = command(ffprobe);
    cmd.args([
        "-v",
        "error",
        "-show_streams",
        "-show_format",
        "-of",
        "json",
    ])
    .arg(path)
    .stdin(Stdio::null());
    let output = tokio::select! {
        result = cmd.output() => result.context("FFprobe 실행 실패")?,
        _ = cancel.cancelled() => anyhow::bail!("작업 취소됨"),
    };
    ensure!(
        output.status.success(),
        "FFprobe 실패: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).context("FFprobe JSON 분석 실패")
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
        "영상 길이/해상도를 읽을 수 없습니다"
    );
    Ok((
        media.duration(),
        video.width as f32,
        video.height as f32,
        fps,
    ))
}
