//! Opt-in integration checks against real FFmpeg/FFprobe, with synthetic media.
use super::*;
use std::{fs, path::Path};

struct TestDir(PathBuf);
impl TestDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "videoencoder-media-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDir {
    fn drop(&mut self) {
        // Only remove known files created by these tests, never recurse.
        if let Ok(entries) = fs::read_dir(&self.0) {
            for entry in entries.flatten() {
                let _ = fs::remove_file(entry.path());
            }
        }
        let _ = fs::remove_dir(&self.0);
    }
}
fn ffmpeg() -> String {
    which::which("ffmpeg")
        .unwrap()
        .to_string_lossy()
        .into_owned()
}
async fn fixture(dir: &Path) -> PathBuf {
    fs::write(
        dir.join("captions.srt"),
        "1\n00:00:00,000 --> 00:00:02,900\nTest caption\n",
    )
    .unwrap();
    let src = dir.join("fixture.mkv");
    let result = command(ffmpeg())
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "testsrc2=size=160x120:rate=10",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=48000",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=880:sample_rate=48000",
            "-i",
        ])
        .arg(dir.join("captions.srt"))
        .args([
            "-t",
            "3",
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-map",
            "2:a",
            "-map",
            "3:s",
            "-c:v",
            "libx264",
            "-g",
            "20",
            "-keyint_min",
            "20",
            "-sc_threshold",
            "0",
            "-c:a:0",
            "aac",
            "-c:a:1",
            "flac",
            "-c:s",
            "srt",
            "-metadata:s:a:0",
            "language=eng",
            "-metadata:s:a:1",
            "language=jpn",
        ])
        .arg(&src)
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    src
}
async fn first_frame(path: &Path, start: &str) -> Vec<u8> {
    let result = command(ffmpeg())
        .args(["-v", "error", "-ss", start, "-i"])
        .arg(path)
        .args([
            "-map",
            "0:v:0",
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            "pipe:1",
        ])
        .output()
        .await
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    result.stdout
}
fn mae(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    a.iter()
        .zip(b)
        .map(|(&a, &b)| (a as f64 - b as f64).abs())
        .sum::<f64>()
        / a.len() as f64
}

#[tokio::test]
#[ignore = "Requires FFmpeg/FFprobe with libx264/libx265"]
async fn real_media_batch_crop_exact_and_fast_cut() {
    let dir = TestDir::new();
    let src = fixture(&dir.0).await;
    let ffmpeg = ffmpeg();
    let tracks = Tracks {
        all_audio: true,
        subtitles: true,
    };
    let options = EncodeOptions {
        encoder: 0,
        quality: 20,
        tracks,
    };
    let target = dir.0.join("result.mp4");
    fs::write(&target, b"existing-result").unwrap();
    let result = run(
        &ffmpeg,
        EncodeJob {
            src: src.clone(),
            output: target.clone(),
            range: None,
            encode: Some(options.clone()),
            filter: None,
            tracks,
        },
        &Cancel::default(),
        |_| {},
    )
    .await
    .unwrap();
    assert_ne!(result, target);
    assert_eq!(fs::read(target).unwrap(), b"existing-result");
    let media = probe::analyze(&ffmpeg, &result, &Cancel::default())
        .await
        .unwrap();
    assert_eq!(media.video().unwrap().codec_name, "hevc");
    let audios: Vec<_> = media
        .streams
        .iter()
        .filter(|s| s.codec_type == "audio")
        .collect();
    assert_eq!(audios.len(), 2);
    assert!(audios.iter().all(|s| s.codec_name == "aac"));
    assert!(media.streams.iter().any(|s| s.codec_name == "mov_text"));

    let exact = run(
        &ffmpeg,
        EncodeJob {
            src: src.clone(),
            output: dir.0.join("exact.mp4"),
            range: Some((0.7, 0.9)),
            encode: Some(options.clone()),
            filter: None,
            tracks,
        },
        &Cancel::default(),
        |_| {},
    )
    .await
    .unwrap();
    let output_frame = first_frame(&exact, "0").await;
    let expected_frame = first_frame(&src, "0.7").await;
    let keyframe = first_frame(&src, "0").await;
    let expected_error = mae(&output_frame, &expected_frame);
    assert!(expected_error < 5.0, "start-frame error={expected_error}");
    assert!(
        expected_error < mae(&output_frame, &keyframe),
        "cut incorrectly includes keyframe pre-roll"
    );
    let probe = probe::resolve_tool(&ffmpeg, "ffprobe").unwrap();
    let count = command(probe)
        .args([
            "-v",
            "error",
            "-count_frames",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=nb_read_frames",
            "-of",
            "csv=p=0",
        ])
        .arg(&exact)
        .output()
        .await
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "9");

    let cropped = run(
        &ffmpeg,
        EncodeJob {
            src: src.clone(),
            output: dir.0.join("crop.mp4"),
            range: Some((0.7, 0.9)),
            encode: Some(options),
            filter: Some("crop=158:120:2:0:exact=1".into()),
            tracks,
        },
        &Cancel::default(),
        |_| {},
    )
    .await
    .unwrap();
    let cropped_media = probe::analyze(&ffmpeg, &cropped, &Cancel::default())
        .await
        .unwrap();
    assert_eq!(
        (
            cropped_media.video().unwrap().width,
            cropped_media.video().unwrap().height
        ),
        (158, 120)
    );

    let fast = run(
        &ffmpeg,
        EncodeJob {
            src: src.clone(),
            output: dir.0.join("fast.mkv"),
            range: Some((0.7, 0.9)),
            encode: None,
            filter: None,
            tracks,
        },
        &Cancel::default(),
        |_| {},
    )
    .await
    .unwrap();
    let fast_media = probe::analyze(&ffmpeg, &fast, &Cancel::default())
        .await
        .unwrap();
    assert_eq!(fast_media.video().unwrap().codec_name, "h264");
    assert!(fast_media.streams.iter().any(|s| s.codec_name == "flac"));
    assert!(fast_media.streams.iter().any(|s| s.codec_name == "subrip"));
    let single = run(
        &ffmpeg,
        EncodeJob {
            src,
            output: dir.0.join("single.mkv"),
            range: None,
            encode: None,
            filter: None,
            tracks: Tracks::default(),
        },
        &Cancel::default(),
        |_| {},
    )
    .await
    .unwrap();
    let single = probe::analyze(&ffmpeg, &single, &Cancel::default())
        .await
        .unwrap();
    assert_eq!(
        single
            .streams
            .iter()
            .filter(|s| s.codec_type == "audio")
            .count(),
        1
    );
    assert!(!single.streams.iter().any(|s| s.codec_type == "subtitle"));
}

#[tokio::test]
#[ignore = "Requires FFmpeg/FFprobe"]
async fn real_failure_has_log_and_cancel_stops_silent_process() {
    let dir = TestDir::new();
    let src = fixture(&dir.0).await;
    let ffmpeg = ffmpeg();
    let error = run(
        &ffmpeg,
        EncodeJob {
            src,
            output: dir.0.join("broken.mp4"),
            range: None,
            encode: Some(EncodeOptions {
                encoder: 0,
                quality: 20,
                tracks: Tracks::default(),
            }),
            filter: Some("videoencoder_invalid_filter".into()),
            tracks: Tracks::default(),
        },
        &Cancel::default(),
        |_| {},
    )
    .await
    .unwrap_err();
    let detail = format!("{error:#}");
    assert!(detail.contains("videoencoder_invalid_filter"), "{detail}");
    assert!(detail.contains("broken_incomplete.mp4"), "{detail}");
    assert!(!dir.0.join("broken.mp4").exists());
    let mut cmd = command(&ffmpeg);
    cmd.args([
        "-v",
        "quiet",
        "-re",
        "-f",
        "lavfi",
        "-i",
        "color=size=16x16:rate=1",
        "-f",
        "null",
        "-",
    ])
    .stderr(Stdio::piped())
    .stdout(Stdio::null())
    .stdin(Stdio::null());
    let child = cmd.spawn().unwrap();
    let cancel = Cancel::default();
    let c = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c.stop();
    });
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        watch_progress(child, 1.0, &cancel, |_| {}),
    )
    .await
    .expect("cancellation must not wait for stderr")
    .unwrap_err();
    assert!(error.to_string().contains("취소"));
}
