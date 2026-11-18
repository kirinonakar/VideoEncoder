use crate::{
    encoder::{self, Cancel, Progress},
    language::message,
    output, probe,
};
use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::process::Command;

const ANIMATION_SOURCE_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "mpg", "mpeg", "webm", "m4v", "gif", "webp", "avif",
    "apng", "png", "jxl",
];
const IMAGE_SOURCE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "bmp", "tif", "tiff", "gif", "webp", "avif", "jxl", "apng",
];

#[derive(Clone, Copy)]
enum ConversionFormat {
    Animation(AnimationFormat),
    Image(ImageOptions),
}

impl ConversionFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Animation(format) => format.extension(),
            Self::Image(options) => options.format.extension(),
        }
    }
}

#[derive(Clone, Copy)]
enum AnimationFormat {
    Gif,
    Webp,
    Avif,
    Jxl,
}

impl AnimationFormat {
    fn from_index(index: i32) -> Result<Self> {
        match index {
            0 => Ok(Self::Gif),
            1 => Ok(Self::Webp),
            2 => Ok(Self::Avif),
            3 => Ok(Self::Jxl),
            _ => anyhow::bail!(message!(
                "Choose an output format.",
                "출력 형식을 선택하세요."
            )),
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::Jxl => "jxl",
        }
    }
}

#[derive(Clone, Copy)]
enum ImageFormat {
    Png,
    Jpeg,
    Bmp,
    Tiff,
    Gif,
    Webp,
    Avif,
    Jxl,
}

impl ImageFormat {
    fn from_index(index: i32) -> Result<Self> {
        match index {
            0 => Ok(Self::Png),
            1 => Ok(Self::Jpeg),
            2 => Ok(Self::Bmp),
            3 => Ok(Self::Tiff),
            4 => Ok(Self::Gif),
            5 => Ok(Self::Webp),
            6 => Ok(Self::Avif),
            7 => Ok(Self::Jxl),
            _ => anyhow::bail!(message!(
                "Choose an output format.",
                "출력 형식을 선택하세요."
            )),
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Bmp => "bmp",
            Self::Tiff => "tiff",
            Self::Gif => "gif",
            Self::Webp => "webp",
            Self::Avif => "avif",
            Self::Jxl => "jxl",
        }
    }

    fn encoder(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "mjpeg",
            Self::Bmp => "bmp",
            Self::Tiff => "tiff",
            Self::Gif => "gif",
            Self::Webp => "libwebp",
            Self::Avif => "libaom-av1",
            Self::Jxl => "libjxl",
        }
    }
}

#[derive(Clone, Copy)]
struct ImageOptions {
    format: ImageFormat,
    lossless: bool,
    quality: i32,
    compression: usize,
}

impl ImageOptions {
    fn configure(
        self,
        cmd: &mut Command,
        media: &probe::Media,
        video: &probe::Stream,
        filter: &str,
    ) {
        let avif_alpha = probe::still_avif_alpha(media, video);
        let input = if let Some(alpha) = avif_alpha {
            format!("[0:{}][0:{}]alphamerge,{filter}", video.index, alpha.index)
        } else {
            format!("[0:{}]{filter}", video.index)
        };
        let compression = self.compression;
        let quality_drop = 100 - self.quality;
        cmd.args(["-frames:v", "1", "-c:v", self.format.encoder()]);
        match self.format {
            ImageFormat::Gif => {
                gif_filter(cmd, &input);
                cmd.args(["-loop", "-1", "-f", "gif"]);
                return;
            }
            ImageFormat::Avif => {
                self.configure_avif(cmd, &input, avif_alpha.is_some() || !known_opaque(video));
                return;
            }
            _ => {
                cmd.arg("-filter_complex").arg(format!("{input}[image]"));
                cmd.args(["-map", "[image]"]);
            }
        }
        match self.format {
            ImageFormat::Png => cmd.args(["-compression_level", ["1", "6", "9"][compression]]),
            ImageFormat::Jpeg => cmd
                .arg("-q:v")
                .arg((2 + quality_drop * 29 / 99).to_string())
                .args(["-pix_fmt", "yuv444p", "-color_range", "pc"]),
            ImageFormat::Tiff => cmd.args([
                "-compression_algo",
                ["packbits", "lzw", "deflate"][compression],
            ]),
            ImageFormat::Webp => cmd
                .args([
                    "-lossless",
                    if self.lossless { "1" } else { "0" },
                    "-compression_level",
                    ["1", "4", "6"][compression],
                    "-quality",
                ])
                .arg(if self.lossless { 100 } else { self.quality }.to_string()),
            ImageFormat::Jxl => cmd
                .args(["-effort", ["3", "7", "9"][compression], "-distance"])
                .arg(if self.lossless {
                    "0".into()
                } else {
                    format!("{:.2}", 0.1 + quality_drop as f32 * 0.08)
                }),
            _ => cmd,
        };
        // Write one literal path, including filenames containing '%'.
        cmd.args(["-f", "image2", "-update", "1"]);
    }

    fn configure_avif(self, cmd: &mut Command, input: &str, has_alpha: bool) {
        if has_alpha {
            cmd.arg("-filter_complex").arg(format!(
                "{input},format=rgba,split[color][alpha-src];[alpha-src]alphaextract[alpha]"
            ));
            cmd.args(["-map", "[color]", "-map", "[alpha]"]);
            // libaom requires a non-identity matrix for the monochrome alpha plane.
            cmd.args([
                "-pix_fmt:v:1",
                "gray",
                "-color_range:v:1",
                "pc",
                "-crf:v:1",
                "0",
                "-aom-params:v:1",
                "lossless=1:matrix-coefficients=bt709",
            ]);
        } else {
            cmd.arg("-filter_complex").arg(format!("{input}[color]"));
            cmd.args(["-map", "[color]"]);
        }
        cmd.args(["-b:v", "0", "-cpu-used", ["8", "6", "3"][self.compression]]);
        if self.lossless {
            cmd.args([
                "-pix_fmt:v:0",
                "gbrp",
                "-colorspace:v:0",
                "rgb",
                "-color_range:v:0",
                "pc",
                "-crf:v:0",
                "0",
                "-aom-params:v:0",
                "lossless=1",
            ]);
        } else {
            cmd.args(["-pix_fmt:v:0", "yuv420p", "-crf:v:0"])
                .arg((1 + (100 - self.quality) * 62 / 99).to_string());
        }
        cmd.args(["-f", "avif"]);
    }
}

#[derive(Clone, Copy)]
struct Resize {
    width: Option<i32>,
    height: Option<i32>,
}

impl Resize {
    fn parse(width: &str, height: &str) -> Result<Self> {
        fn dimension(value: &str, error: crate::LocalizedText) -> Result<Option<i32>> {
            let value = value.trim();
            if value.is_empty() {
                return Ok(None);
            }
            let size = value
                .parse::<i32>()
                .ok()
                .filter(|size| *size > 0)
                .context(error)?;
            Ok(Some(size))
        }
        Ok(Self {
            width: dimension(
                width,
                message!(
                    "Width must be a positive integer.",
                    "가로 크기는 1 이상의 정수로 입력하세요."
                ),
            )?,
            height: dimension(
                height,
                message!(
                    "Height must be a positive integer.",
                    "세로 크기는 1 이상의 정수로 입력하세요."
                ),
            )?,
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
    suffix: String,
    format: ConversionFormat,
    resize: Resize,
}

impl ConversionJob {
    fn output_path(&self) -> PathBuf {
        let mut name = self.source.file_stem().unwrap_or_default().to_os_string();
        name.push(&self.suffix);
        name.push(".");
        name.push(self.format.extension());
        output::directory(&self.source, &self.output_folder).join(name)
    }

    async fn run(
        &self,
        ffmpeg: &str,
        cancel: &Cancel,
        on_progress: impl Fn(Progress),
    ) -> Result<PathBuf> {
        let media = probe::analyze(ffmpeg, &self.source, cancel).await?;
        let video = probe::source_video(&media)?;
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
        cmd.args(probe::input_options(&self.source).await)
            .arg("-i")
            .arg(&self.source);
        let filter = self.resize.filter();
        let duration = match self.format {
            ConversionFormat::Image(options) => {
                options.configure(&mut cmd, &media, video, &filter);
                0.0
            }
            ConversionFormat::Animation(format) => {
                format.configure(&mut cmd, video, &filter);
                probe::source_duration(&media, video).unwrap_or_default() as f32
            }
        };
        encoder::run_to_output(cmd, &self.output_path(), duration, cancel, on_progress).await
    }
}

fn gif_filter(cmd: &mut Command, input: &str) {
    // Generate a palette per frame to avoid buffering the whole video.
    cmd.arg("-filter_complex").arg(format!(
        "{input},split[frames][colors];[colors]palettegen=stats_mode=single[palette];[frames][palette]paletteuse=new=1[gif]"
    ));
    cmd.args(["-map", "[gif]"]);
}

impl AnimationFormat {
    fn configure(self, cmd: &mut Command, video: &probe::Stream, filter: &str) {
        if matches!(self, Self::Gif) {
            gif_filter(cmd, &format!("[0:{}]{filter}", video.index));
        } else {
            cmd.arg("-map").arg(format!("0:{}", video.index));
            let filter = match self {
                Self::Webp => format!("format=bgra,{filter}"),
                // Let libjxl_anim derive frame durations from the presentation timestamps.
                Self::Jxl => format!("{filter},setpts=PTS:strip_fps=1"),
                _ => filter.to_owned(),
            };
            cmd.arg("-vf").arg(filter);
        }
        match self {
            Self::Gif => cmd.args(["-c:v", "gif"]),
            Self::Webp => cmd.args([
                "-c:v",
                webp_encoder(video),
                "-lossless",
                "0",
                "-quality",
                "90",
                "-compression_level",
                "4",
                "-pix_fmt",
                "bgra",
            ]),
            Self::Avif => cmd.args([
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
            Self::Jxl => cmd.args(["-c:v", "libjxl_anim", "-distance", "1"]),
        };
        cmd.args(["-fps_mode", "passthrough", "-enc_time_base", "1:1000"]);
        if matches!(self, Self::Jxl) {
            // The encoder emits one codestream in multiple packets.
            cmd.args(["-f", "image2pipe"]);
        } else {
            cmd.args(["-loop", "0", "-f", self.extension()]);
        }
    }
}

fn webp_encoder(video: &probe::Stream) -> &'static str {
    // For known opaque formats, let the WebP muxer assemble independent frames.
    // Alpha inputs need the animation encoder's frame replacement/blending flags.
    if known_opaque(video) {
        "libwebp"
    } else {
        "libwebp_anim"
    }
}

fn known_opaque(video: &probe::Stream) -> bool {
    matches!(
        video.pix_fmt.as_str(),
        "yuv420p"
            | "yuv422p"
            | "yuv444p"
            | "yuvj420p"
            | "yuvj422p"
            | "yuvj444p"
            | "yuv420p10le"
            | "yuv422p10le"
            | "yuv444p10le"
            | "nv12"
            | "p010le"
            | "gray"
            | "rgb24"
            | "bgr24"
            | "gbrp"
    ) && video
        .tags
        .alpha_mode
        .as_deref()
        .is_none_or(|mode| mode == "0")
}

#[path = "conversion_queue.rs"]
mod queue;
pub use queue::setup;

#[cfg(test)]
#[path = "conversion_tests.rs"]
mod tests;
