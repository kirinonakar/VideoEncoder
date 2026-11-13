use super::*;
use crate::probe::source_info;
use serde_json::Value;
use std::{fs, path::Path, process::Output};

fn check_output(result: Output) -> Output {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(result.stderr.is_empty(), "{:?}", result.stderr);
    result
}

fn webp_chunks(mut data: &[u8]) -> impl Iterator<Item = (&[u8], &[u8])> {
    std::iter::from_fn(move || {
        let header = data.get(..8)?;
        let size = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
        let payload = &data[8..8 + size];
        data = &data[8 + size + (size & 1)..];
        Some((&header[..4], payload))
    })
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
#[ignore = "Requires FFmpeg/FFprobe with animated WebP decoding, libwebp and libaom-av1"]
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
    assert_eq!(
        source_info(&ffmpeg, &src, &cancel).await.unwrap().english,
        "160 × 120 px · 00:00:01.500"
    );
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
        let info = source_info(&ffmpeg, &path, &cancel).await.unwrap().english;
        assert!(
            info.starts_with(&format!("{} × {} px · 00:00:01.", size.0, size.1)),
            "{info}"
        );
        if matches!(format, AnimationFormat::Webp) {
            let bytes = fs::read(&path).unwrap();
            let frames: Vec<_> = webp_chunks(&bytes[12..])
                .filter_map(|(tag, frame)| (tag == b"ANMF").then_some(frame))
                .collect();
            assert_eq!(frames.len(), 12);
            for frame in frames {
                assert_eq!(&frame[..12], &[0, 0, 0, 0, 0, 0, 78, 0, 0, 58, 0, 0]);
                let tags: Vec<_> = webp_chunks(&frame[16..]).map(|(tag, _)| tag).collect();
                assert_eq!(tags, [b"VP8 "]);
            }
        }
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

#[tokio::test]
#[ignore = "Requires FFmpeg/FFprobe with animated WebP decoding and libwebp_anim"]
async fn webp_preserves_transparency_across_moving_frames() {
    let ffmpeg = which::which("ffmpeg")
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let scratch = crate::encoder::integration_tests::TestDir::new();
    let source = scratch.0.join("alpha.mkv");
    check_output(
        encoder::command(&ffmpeg)
            .args(["-v", "error", "-f", "lavfi", "-i",
                "nullsrc=size=65x47:rate=10:duration=0.5,format=gbrap,geq=r=240:g=60:b=30:a='if(between(X,8+8*N,15+8*N)*between(Y,10,30),255,0)'",
                "-c:v", "ffv1"])
            .arg(&source)
            .output()
            .await
            .unwrap(),
    );
    let output = ConversionJob {
        source: source.clone(),
        output_folder: PathBuf::new(),
        format: AnimationFormat::Webp,
        resize: Resize::parse("", "").unwrap(),
    }
    .run(&ffmpeg, &Cancel::default(), |_| {})
    .await
    .unwrap();

    let mut alpha_planes = Vec::new();
    for path in [source, output] {
        let decoded = check_output(
            encoder::command(&ffmpeg)
                .args(["-v", "error", "-i"])
                .arg(path)
                .args(["-vf", "alphaextract", "-f", "rawvideo", "-"])
                .output()
                .await
                .unwrap(),
        );
        assert_eq!(decoded.stdout.len(), 65 * 47 * 5);
        alpha_planes.push(decoded.stdout);
    }
    assert!(alpha_planes[0] == alpha_planes[1], "decoded alpha changed");
}
