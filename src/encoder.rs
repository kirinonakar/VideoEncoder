use crate::{
    output::OutputFile,
    probe::{self, Media},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::VecDeque,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::{io::AsyncReadExt, process::Command};

#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);
impl Cancel {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
    pub fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
    pub async fn cancelled(&self) {
        while !self.stopped() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

pub fn command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut cmd = Command::new(program);
    cmd.kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    cmd
}

#[derive(Clone, Copy, Default)]
pub struct Tracks {
    pub all_audio: bool,
    pub subtitles: bool,
}
#[derive(Clone)]
pub struct EncodeOptions {
    pub encoder: i32,
    pub quality: i32,
    pub tracks: Tracks,
}
pub struct EncodeJob {
    pub src: PathBuf,
    pub output: PathBuf,
    pub range: Option<(f32, f32)>,
    /// None means stream-copy; Some means exact trim / re-encoding.
    pub encode: Option<EncodeOptions>,
    pub filter: Option<String>,
    pub tracks: Tracks,
}
#[derive(Default, Clone)]
pub struct Progress {
    pub fraction: f32,
    pub speed: String,
    pub bitrate: String,
}

pub fn add_encoder_args(cmd: &mut Command, encoder: i32, quality: i32) {
    if encoder == 1 {
        cmd.args(["-c:v", "hevc_nvenc", "-preset", "p7", "-rc", "vbr", "-cq"])
            .arg(quality.to_string())
            .args(["-b:v", "0"]);
    } else {
        cmd.args(["-c:v", "libx265", "-crf"])
            .arg(quality.to_string())
            .args(["-preset", "medium"]);
    }
    cmd.args(["-pix_fmt", "yuv420p"]);
}

fn audio_copy_safe(codec: &str) -> bool {
    // Conservative MP4 playback compatibility. Other muxable codecs (DTS,
    // FLAC, Opus, Vorbis, etc.) are deliberately converted to AAC.
    matches!(codec, "aac" | "mp3" | "ac3" | "eac3" | "alac")
}
fn text_subtitle(codec: &str) -> bool {
    matches!(
        codec,
        "subrip" | "ass" | "ssa" | "webvtt" | "mov_text" | "text"
    )
}

pub fn map_tracks(
    cmd: &mut Command,
    media: &Media,
    tracks: Tracks,
    mp4: bool,
    exact: bool,
) -> Result<()> {
    cmd.arg("-map").arg(format!("0:{}", media.video()?.index));
    for (i, stream) in media
        .streams
        .iter()
        .filter(|s| s.codec_type == "audio")
        .take(if tracks.all_audio { usize::MAX } else { 1 })
        .enumerate()
    {
        cmd.arg("-map").arg(format!("0:{}", stream.index));
        cmd.arg(format!("-c:a:{i}")).arg(
            if exact || (mp4 && !audio_copy_safe(&stream.codec_name)) {
                "aac"
            } else {
                "copy"
            },
        );
    }
    if tracks.subtitles {
        for (i, stream) in media
            .streams
            .iter()
            .filter(|s| s.codec_type == "subtitle")
            .enumerate()
        {
            ensure!(
                !mp4 || text_subtitle(&stream.codec_name),
                "MP4에 보존할 수 없는 자막 코덱: {}. 자막 보존을 끄거나 MKV 빠른 컷을 사용하세요.",
                stream.codec_name
            );
            cmd.arg("-map").arg(format!("0:{}", stream.index));
            cmd.arg(format!("-c:s:{i}"))
                .arg(if mp4 { "mov_text" } else { "copy" });
        }
    }
    cmd.args(["-map_metadata", "0", "-map_chapters", "0"]);
    Ok(())
}

/// Read a bounded stderr tail, drain while waiting, and check cancellation even
/// when FFmpeg produces no output (including after closing its stderr).
pub async fn watch_progress(
    mut child: tokio::process::Child,
    duration: f32,
    cancel: &Cancel,
    on_progress: impl Fn(Progress),
) -> Result<()> {
    let mut stderr = child.stderr.take().context("FFmpeg 오류 파이프 없음")?;
    let mut tail = VecDeque::new();
    let mut line = Vec::new();
    let mut buf = [0u8; 4096];
    let mut progress = Progress::default();
    let status = loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                child.kill().await.context("FFmpeg 중단 실패")?;
                anyhow::bail!("작업 취소됨");
            }
            read = stderr.read(&mut buf) => {
                let n = read.context("FFmpeg 로그 읽기 실패")?;
                if n == 0 {
                    break tokio::select! {
                        s = child.wait() => s?,
                        _ = cancel.cancelled() => {
                            child.kill().await?;
                            anyhow::bail!("작업 취소됨");
                        }
                    };
                }
                for &byte in &buf[..n] {
                    tail.push_back(byte);
                    if tail.len() > 8192 { tail.pop_front(); }
                    if byte == b'\n' || byte == b'\r' {
                        let text = String::from_utf8_lossy(&line);
                        if let Some(value) = text.strip_prefix("out_time_us=") {
                            if let Ok(us) = value.trim().parse::<f64>() {
                                progress.fraction = if duration > 0.0 { (us / 1e6 / duration as f64).clamp(0.0, 1.0) as f32 } else { 0.0 };
                                on_progress(progress.clone());
                            }
                        } else if let Some(value) = text.strip_prefix("speed=") { progress.speed = value.trim().to_owned(); }
                        else if let Some(value) = text.strip_prefix("bitrate=") { progress.bitrate = value.trim().to_owned(); }
                        line.clear();
                    } else if line.len() < 8192 { line.push(byte); }
                }
            }
        }
    };
    ensure!(!cancel.stopped(), "작업 취소됨");
    let bytes: Vec<u8> = tail.into_iter().collect();
    ensure!(
        status.success(),
        "FFmpeg {status}:\n{}",
        String::from_utf8_lossy(&bytes).trim()
    );
    Ok(())
}

pub async fn run(
    ffmpeg: &str,
    job: EncodeJob,
    cancel: &Cancel,
    on_progress: impl Fn(Progress),
) -> Result<PathBuf> {
    let media = probe::analyze(ffmpeg, &job.src, cancel).await?;
    ensure!(!cancel.stopped(), "작업 취소됨");
    let mp4 = job
        .output
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("mp4"));
    let mut cmd = command(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-y"]);
    if let Some((start, _)) = job.range {
        cmd.arg("-ss").arg(format!("{start:.6}"));
    }
    cmd.arg("-i").arg(&job.src);
    let duration = if let Some((_, duration)) = job.range {
        cmd.arg("-t").arg(format!("{duration:.6}"));
        duration
    } else {
        media.duration()
    };
    if let Some(options) = &job.encode {
        if let Some(filter) = &job.filter {
            cmd.arg("-vf").arg(filter);
        }
        add_encoder_args(&mut cmd, options.encoder, options.quality);
        // Exact trim re-encodes audio too, avoiding copied packets before the
        // requested start. FFmpeg accurate_seek is enabled by default.
        map_tracks(&mut cmd, &media, options.tracks, mp4, job.range.is_some())?;
    } else {
        cmd.args(["-c:v", "copy", "-avoid_negative_ts", "make_zero"]);
        map_tracks(&mut cmd, &media, job.tracks, mp4, false)?;
    }
    let output = OutputFile::reserve(&job.output).context("출력 파일 생성 실패")?;
    cmd.args(["-progress", "pipe:2"])
        .arg(&output.incomplete)
        .stderr(Stdio::piped())
        .stdout(Stdio::null())
        .stdin(Stdio::null());
    let result = async {
        let child = cmd.spawn().context("FFmpeg 실행 실패")?;
        watch_progress(child, duration, cancel, on_progress).await?;
        ensure!(!cancel.stopped(), "작업 취소됨");
        output.finalize().context("최종 출력 저장 실패")
    }
    .await;
    result.with_context(|| format!("미완성/복구 파일: {}", output.incomplete.display()))
}

#[cfg(test)]
#[path = "encoder_tests.rs"]
mod integration_tests;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_each_audio_codec_and_rejects_bitmap_subtitles() {
        let media: Media = serde_json::from_value(serde_json::json!({"streams": [
            {"index":0,"codec_type":"video","codec_name":"h264"},
            {"index":1,"codec_type":"audio","codec_name":"aac"},
            {"index":2,"codec_type":"audio","codec_name":"flac"},
            {"index":3,"codec_type":"audio","codec_name":"dts"},
            {"index":4,"codec_type":"subtitle","codec_name":"subrip"}
        ]}))
        .unwrap();
        let mut cmd = command("ffmpeg");
        map_tracks(
            &mut cmd,
            &media,
            Tracks {
                all_audio: true,
                subtitles: true,
            },
            true,
            false,
        )
        .unwrap();
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.windows(2).any(|a| a == ["-c:a:0", "copy"]));
        assert!(args.windows(2).any(|a| a == ["-c:a:1", "aac"]));
        assert!(args.windows(2).any(|a| a == ["-c:a:2", "aac"]));
        assert!(args.windows(2).any(|a| a == ["-c:s:0", "mov_text"]));
        let mut media = media;
        media.streams.last_mut().unwrap().codec_name = "hdmv_pgs_subtitle".into();
        assert!(
            map_tracks(
                &mut cmd,
                &media,
                Tracks {
                    all_audio: true,
                    subtitles: true
                },
                true,
                false
            )
            .is_err()
        );
    }
}
