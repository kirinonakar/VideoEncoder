use super::*;
use serde_json::Value;
use std::{fs, process::Output};

fn check_output(result: Output) -> Output {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.stderr.is_empty(), "{:?}", result.stderr);
    result
}

async fn check_frames(ffmpeg: &str, path: &Path, size: (u64, u64), count: u64) {
    let ffprobe = probe::resolve_tool(ffmpeg, "ffprobe").unwrap();
    let result = check_output(
        encoder::command(ffprobe)
            .args([
                "-v",
                "error",
                "-count_frames",
                "-show_entries",
                "stream=width,height,nb_read_frames,duration:packet=stream_index,pts_time",
                "-of",
                "json",
                "-show_packets",
            ])
            .arg(path)
            .output()
            .await
            .unwrap(),
    );
    let data: Value = serde_json::from_slice(&result.stdout).unwrap();
    let streams = data["streams"].as_array().unwrap();
    let stream_index = streams
        .iter()
        .position(|stream| {
            stream["nb_read_frames"]
                .as_str()
                .and_then(|n| n.parse().ok())
                == Some(count)
        })
        .expect("missing animation track");
    assert_eq!(streams[stream_index]["width"], size.0);
    assert_eq!(streams[stream_index]["height"], size.1);
    let frame_times: Vec<_> = data["packets"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|packet| packet["stream_index"] == stream_index)
        .map(|packet| packet["pts_time"].as_str().unwrap().parse::<f64>().unwrap())
        .collect();
    assert_eq!(frame_times.len(), count as usize);
    for (index, pts) in frame_times.into_iter().enumerate() {
        let expected = (index + index / 3) as f64 * 0.1;
        assert!((pts - expected).abs() < 0.01, "{pts} != {expected}");
    }
}

#[tokio::test]
#[ignore = "Requires FFmpeg/FFprobe with animated WebP decoding, libwebp_anim and libaom-av1"]
async fn converts_all_formats_with_resize_and_preserves_animation() {
    let ffmpeg = which::which("ffmpeg")
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let scratch = crate::encoder::integration_tests::TestDir::new();
    let src = scratch.0.join("원본.mkv");
    let folder = scratch.0.join("출력");
    fs::create_dir(&folder).unwrap();
    check_output(
        encoder::command(&ffmpeg)
            .args([
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "testsrc2=size=160x120:rate=10:duration=1.2",
                "-vf",
                "settb=1/1000,setpts='(N+floor(N/3))*100'",
                "-fps_mode",
                "passthrough",
                "-enc_time_base",
                "1:1000",
                "-c:v",
                "ffv1",
            ])
            .arg(&src)
            .output()
            .await
            .unwrap(),
    );

    let cancel = Cancel::default();
    for (format, width, height, size) in [
        (AnimationFormat::Gif, "", "", (160, 120)),
        (AnimationFormat::Webp, "79", "", (79, 59)),
        (AnimationFormat::Avif, "", "61", (81, 61)),
    ] {
        let job = ConversionJob {
            source: src.clone(),
            output_folder: folder.clone(),
            format,
            resize: Resize::parse(width, height).unwrap(),
        };
        let path = job.run(&ffmpeg, &cancel, |_| {}).await.unwrap();
        assert_eq!(path, folder.join(format!("원본.{}", format.extension())));
        check_frames(&ffmpeg, &path, size, 12).await;
    }

    let avif = folder.join("원본.avif");
    let original = fs::read(&avif).unwrap();
    let job = ConversionJob {
        source: avif.clone(),
        output_folder: PathBuf::new(),
        format: AnimationFormat::Avif,
        resize: Resize::parse("65", "47").unwrap(),
    };
    let path = job.run(&ffmpeg, &cancel, |_| {}).await.unwrap();
    assert_eq!(path, folder.join("원본 (1).avif"));
    assert_eq!(fs::read(avif).unwrap(), original);
    check_frames(&ffmpeg, &path, (65, 47), 12).await;

    for entry in fs::read_dir(&folder).unwrap() {
        fs::remove_file(entry.unwrap().path()).unwrap();
    }
    fs::remove_dir(folder).unwrap();
}
