#[cfg(test)]
use crate::probe::probe_video;
use crate::{MainWindow, playback_range::PlaybackRange, seconds_to_hms};
use slint::ComponentHandle;
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Instant,
};
use tokio::{io::AsyncReadExt, process::Command};
static FFPLAY_WARNED: AtomicBool = AtomicBool::new(false);
/// 미리보기용 출력 해상도 (최대 너비 1280px, 짝수 크기 보장)
fn preview_size(vw: f32, vh: f32) -> (u32, u32) {
    if vw <= 0.0 || vh <= 0.0 {
        return (1280, 720);
    }
    let mut w = vw.min(1280.0).max(2.0);
    let mut h = (w * vh / vw).round().max(2.0);
    w = (w / 2.0).floor() * 2.0;
    h = (h / 2.0).floor() * 2.0;
    (w as u32, h as u32)
}

/// ffplay(오디오)가 오디오 장치를 초기화하고 소리를 내기 시작할 때까지의 대기 시간(초).
/// 이 시간만큼 영상 프레임 표시를 늦춰 재생 시작 시점의 A/V 싱크를 맞춘다.
const AUDIO_START_DELAY: f32 = 0.25;

/// ffplay(오디오 재생용) 실행 파일 경로를 찾는다.
/// ffmpeg.exe 와 같은 폴더의 ffplay.exe 를 우선 사용하고, 없으면 PATH 에서 찾는다.
fn resolve_ffplay(ffmpeg: &str) -> Option<String> {
    crate::probe::resolve_tool(ffmpeg, "ffplay").map(|p| p.to_string_lossy().into_owned())
}

fn preview_playback_command(ffmpeg: &str, path: &Path, t0: f32, pw: u32, ph: u32) -> Command {
    let mut cmd = crate::encoder::command(ffmpeg);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    cmd.args(["-hide_banner", "-loglevel", "error", "-ss"])
        .arg(format!("{:.3}", t0))
        .arg("-i")
        .arg(path)
        .args(["-an", "-sn", "-vf"])
        .arg(format!("scale={}:{},setsar=1", pw, ph))
        // FFmpeg 9에서는 -vsync가 제거되었다. 기존 -vsync 0과 같은 동작이다.
        .args([
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-y",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null());
    cmd
}

/// stderr를 계속 비워 디코더가 막히지 않게 하되, 오류 표시는 마지막 8 KiB로 제한한다.
async fn read_preview_errors(mut stderr: tokio::process::ChildStderr) -> String {
    let mut tail = Vec::new();
    let mut buf = [0u8; 4096];
    while let Ok(n) = stderr.read(&mut buf).await {
        if n == 0 {
            break;
        }
        tail.extend_from_slice(&buf[..n]);
        if tail.len() > 8192 {
            tail.drain(..tail.len() - 8192);
        }
    }
    String::from_utf8_lossy(&tail).trim().to_owned()
}

pub struct FrameRequester {
    state: std::sync::Mutex<FrameState>,
    stream: std::sync::Mutex<Option<tokio::process::Child>>,
    stream_id: AtomicU64,
    /// 오디오 디코더와 재생용 ffplay 프로세스
    audio: std::sync::Mutex<Option<AudioPlayback>>,
}

/// ffmpeg가 시작 위치부터 디코드한 WAV 오디오를 ffplay로 전달하는 파이프라인.
struct AudioPlayback {
    decoder: tokio::process::Child,
    player: tokio::process::Child,
}

struct AudioPipes {
    playback: AudioPlayback,
    decoder_stdout: tokio::process::ChildStdout,
    player_stdin: tokio::process::ChildStdin,
}

/// 오디오 파이프라인의 두 프로세스를 종료하고 핸들을 회수한다.
fn kill_audio_playback(mut playback: AudioPlayback) {
    let _ = playback.decoder.start_kill();
    let _ = playback.player.start_kill();
    tokio::spawn(async move {
        let _ = playback.decoder.wait().await;
        let _ = playback.player.wait().await;
    });
}

/// ffplay 자체의 -ss는 일부 파일에서 오디오 디코더를 파일 시작점에서
/// 열어 둔 채 seek 상태만 바꾸므로, 오디오도 ffmpeg에서 먼저 seek한 PCM으로
/// 만들어 ffplay의 stdin으로 전달한다. 이렇게 하면 오디오의 첫 샘플이 항상 t0이다.
fn spawn_audio_playback(
    ffmpeg: &str,
    ffplay: &str,
    path: &Path,
    t0: f32,
) -> anyhow::Result<AudioPipes> {
    let mut decoder_cmd = crate::encoder::command(ffmpeg);
    #[cfg(windows)]
    decoder_cmd.creation_flags(0x08000000);
    decoder_cmd
        .args(["-hide_banner", "-loglevel", "error", "-ss"])
        .arg(format!("{:.3}", t0))
        .arg("-i")
        .arg(path)
        .args([
            "-map",
            "0:a:0",
            "-vn",
            "-sn",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-c:a",
            "pcm_s16le",
            "-f",
            "wav",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null());
    let mut decoder = decoder_cmd.spawn()?;
    let decoder_stdout = decoder.stdout.take().ok_or_else(|| {
        let _ = decoder.start_kill();
        anyhow::anyhow!("오디오 디코더 출력 파이프를 열 수 없습니다")
    })?;

    let mut player_cmd = crate::encoder::command(ffplay);
    #[cfg(windows)]
    player_cmd.creation_flags(0x08000000);
    player_cmd
        .args([
            "-nodisp",
            "-vn",
            "-autoexit",
            "-loglevel",
            "quiet",
            "-i",
            "pipe:0",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut player = match player_cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            let _ = decoder.start_kill();
            return Err(e.into());
        }
    };
    let player_stdin = match player.stdin.take() {
        Some(stdin) => stdin,
        None => {
            let _ = decoder.start_kill();
            let _ = player.start_kill();
            return Err(anyhow::anyhow!("ffplay 입력 파이프를 열 수 없습니다"));
        }
    };

    Ok(AudioPipes {
        playback: AudioPlayback { decoder, player },
        decoder_stdout,
        player_stdin,
    })
}

/// 프레임 요청 상태. busy/pending 을 하나의 락으로 관리해
/// 마지막 요청이 유실되는 경쟁 조건(race)을 제거한다.
struct FrameState {
    busy: bool,
    pending: Option<f32>,
}

impl FrameRequester {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: std::sync::Mutex::new(FrameState {
                busy: false,
                pending: None,
            }),
            stream: std::sync::Mutex::new(None),
            stream_id: AtomicU64::new(0),
            audio: std::sync::Mutex::new(None),
        })
    }

    /// 단일 프레임 추출 요청 (시크/시작점·끝점 이동 시 사용).
    /// 진행 중인 재생 스트림은 먼저 중단한다.
    pub fn request(
        self: &Arc<Self>,
        weak: slint::Weak<MainWindow>,
        ffmpeg: String,
        path: PathBuf,
        t: f32,
        vw: f32,
        vh: f32,
    ) {
        let (pw, ph) = preview_size(vw, vh);
        self.stop_playback();
        let should_spawn = {
            let mut st = self.state.lock().unwrap();
            if st.busy {
                st.pending = Some(t);
                false
            } else {
                st.busy = true;
                true
            }
        };
        if !should_spawn {
            return;
        }
        let this = self.clone();
        let w = weak.clone();
        tokio::spawn(async move {
            let mut t = t;
            loop {
                let res = extract_frame(&ffmpeg, &path, t, pw, ph)
                    .await
                    .map(|bytes| (bytes, pw, ph));
                // pending 확인과 idle 전환을 같은 락 안에서 처리해
                // "마지막 시크 위치가 표시되지 않는" 문제를 없앤다.
                let next = {
                    let mut st = this.state.lock().unwrap();
                    let n = st.pending.take();
                    if n.is_none() {
                        st.busy = false;
                    }
                    n
                };
                let frame_t = t;
                let w2 = w.clone();
                let this2 = this.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w2.upgrade() {
                        // 재생 스트림이 살아 있으면 스트림 프레임이 우선한다.
                        if this2.stream.lock().unwrap().is_some() {
                            return;
                        }
                        // 마커/타임라인을 계속 움직여 최신 시크 위치와 다르다면
                        // 낡은 프레임을 표시하지 않는다.
                        if (ui.get_preview_time() - frame_t).abs() > 0.1 {
                            return;
                        }
                        match &res {
                            Ok((bytes, width, height)) => {
                                let buf =
                                    slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                                        bytes, *width, *height,
                                    );
                                ui.set_video_image(slint::Image::from_rgba8(buf));
                            }
                            Err(e) => {
                                ui.set_edit_status_text(format!("프레임 추출 실패: {}", e).into());
                            }
                        }
                    }
                });
                match next {
                    Some(n) => t = n,
                    None => break,
                }
            }
        });
    }

    /// 재생용 ffmpeg 프로세스를 종료한다.
    pub fn stop_playback(self: &Arc<Self>) {
        self.stream_id.fetch_add(1, Ordering::SeqCst);
        let child = self.stream.lock().unwrap().take();
        if let Some(mut child) = child {
            let _ = child.start_kill();
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
        }
        // 오디오(ffplay) 프로세스도 함께 종료한다.
        if let Some(audio) = self.audio.lock().unwrap().take() {
            kill_audio_playback(audio);
        }
    }

    /// ffmpeg 프로세스 하나로 프레임을 연속 수신하며 실시간 재생한다.
    pub fn start_playback(
        self: &Arc<Self>,
        weak: slint::Weak<MainWindow>,
        ffmpeg: String,
        path: PathBuf,
        t0: f32,
        dur: f32,
        fps: f32,
        vw: f32,
        vh: f32,
    ) {
        let (pw, ph) = preview_size(vw, vh);
        let frame_bytes = (pw as usize) * (ph as usize) * 4;
        self.stop_playback();
        let id = self.stream_id.fetch_add(1, Ordering::SeqCst) + 1;

        // ffplay(오디오 재생용) 경로를 미리 찾아둔다. 없으면 소리 없이 재생을 계속한다.
        let ffplay = match resolve_ffplay(&ffmpeg) {
            Some(p) => Some(p),
            None => {
                if !FFPLAY_WARNED.swap(true, Ordering::SeqCst) {
                    let w = weak.clone();
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = w.upgrade() {
                            ui.set_edit_status_text(
                                "ffplay 를 찾을 수 없어 소리 없이 재생합니다.".into(),
                            );
                        }
                    });
                }
                None
            }
        };

        let mut cmd = preview_playback_command(&ffmpeg, &path, t0, pw, ph);
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let w = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        ui.set_is_playing(false);
                        ui.set_edit_status_text(format!("재생 시작 실패: {}", e).into());
                    }
                });
                return;
            }
        };
        let mut stdout = match child.stdout.take() {
            Some(s) => s,
            None => {
                let _ = child.start_kill();
                let w = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = w.upgrade() {
                        ui.set_is_playing(false);
                    }
                });
                return;
            }
        };
        let errors = child
            .stderr
            .take()
            .map(|stderr| tokio::spawn(read_preview_errors(stderr)));
        *self.stream.lock().unwrap() = Some(child);
        let this = self.clone();
        let w = weak.clone();
        tokio::spawn(async move {
            let fps = if fps > 0.5 { fps } else { 30.0 };
            // 1) 첫 프레임이 준비될 때까지 기다린다. ffmpeg 는 시크 지점(t0) 이전
            //    키프레임부터 t0 까지 디코드해야 하므로, 재생 중간부터 시작하면 이
            //    대기 시간이 길어진다. 이 시간 동안 오디오를 먼저 켜면 소리가 영상보다
            //    앞서가므로, 영상 준비가 끝난 뒤에 오디오를 시작한다.
            let mut first_frame: Option<Vec<u8>> = None;
            let mut decoder_eof = false;
            let mut playback_error = None;
            {
                let mut buf = vec![0u8; frame_bytes];
                match read_exact_or_eof(&mut stdout, &mut buf).await {
                    Ok(true) => first_frame = Some(buf),
                    Ok(false) => decoder_eof = true,
                    Err(e) => playback_error = Some(e.to_string()),
                }
            }

            // 2) 영상이 준비된 시점에 t0부터 디코드한 오디오를 시작해 같은 시각에서 출발한다.
            let mut audio_started = false;
            if first_frame.is_some() {
                if this.stream_id.load(Ordering::SeqCst) != id {
                    return;
                }
                if let Some(ffplay) = ffplay {
                    if let Ok(mut pipes) = spawn_audio_playback(&ffmpeg, &ffplay, &path, t0) {
                        // stop_playback()와 오디오 생성이 겹쳐도 새 프로세스가 고아로
                        // 남지 않도록 먼저 슬롯에 넣고, 직후 세대 번호를 다시 확인한다.
                        let stale_before_install = this.stream_id.load(Ordering::SeqCst) != id;
                        if stale_before_install {
                            kill_audio_playback(pipes.playback);
                            return;
                        }
                        *this.audio.lock().unwrap() = Some(pipes.playback);
                        if this.stream_id.load(Ordering::SeqCst) != id {
                            if let Some(audio) = this.audio.lock().unwrap().take() {
                                kill_audio_playback(audio);
                            }
                            return;
                        }
                        tokio::spawn(async move {
                            let _ =
                                tokio::io::copy(&mut pipes.decoder_stdout, &mut pipes.player_stdin)
                                    .await;
                        });
                        audio_started = true;
                    }
                }
                // 3) 오디오 장치 초기화 시간만큼 기다렸다가 영상 표시를 시작한다 (A/V 싱크).
                if audio_started {
                    tokio::time::sleep(std::time::Duration::from_secs_f32(AUDIO_START_DELAY)).await;
                }
                if this.stream_id.load(Ordering::SeqCst) != id {
                    return;
                }
            }

            let start_wall = Instant::now();
            let mut idx: u32 = 0;
            let mut pending = first_frame;
            while !decoder_eof && playback_error.is_none() {
                if this.stream_id.load(Ordering::SeqCst) != id {
                    break;
                }
                let buf = match pending.take() {
                    Some(b) => b,
                    None => {
                        let mut b = vec![0u8; frame_bytes];
                        match read_exact_or_eof(&mut stdout, &mut b).await {
                            Ok(true) => b,
                            Ok(false) => {
                                decoder_eof = true;
                                break;
                            }
                            Err(e) => {
                                playback_error = Some(e.to_string());
                                break;
                            }
                        }
                    }
                };
                let rel = idx as f32 / fps;
                idx += 1;
                let media_t = t0 + rel;
                if media_t > dur {
                    break;
                }
                // 실제 재생 속도에 맞춰 프레임 표시 시점을 조절한다 (실시간 재생).
                let elapsed = start_wall.elapsed().as_secs_f32();
                if rel > elapsed {
                    tokio::time::sleep(std::time::Duration::from_secs_f32(rel - elapsed)).await;
                }
                if this.stream_id.load(Ordering::SeqCst) != id {
                    break;
                }
                let w2 = w.clone();
                let this2 = this.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if this2.stream_id.load(Ordering::SeqCst) != id {
                        return;
                    }
                    if let Some(ui) = w2.upgrade() {
                        let range = playback_range(&ui);
                        if range.finished(media_t) {
                            ui.set_preview_time(range.end);
                            ui.set_time_preview_text(seconds_to_hms(range.end).into());
                            ui.set_is_playing(false);
                            this2.stop_playback();
                            return;
                        }
                        if media_t < range.start {
                            move_preview(&ui, &this2, range.start, true);
                            return;
                        }
                        let img = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            &buf, pw, ph,
                        );
                        ui.set_video_image(slint::Image::from_rgba8(img));
                        ui.set_preview_time(media_t);
                        ui.set_time_preview_text(seconds_to_hms(media_t).into());
                    }
                });
            }
            // EOF와 디코더 실패를 구분하고, 취소된 이전 재생은 현재 UI를 바꾸지 않는다.
            if this.stream_id.load(Ordering::SeqCst) == id {
                let child = this.stream.lock().unwrap().take();
                if let Some(mut child) = child {
                    if !decoder_eof {
                        let _ = child.start_kill();
                    }
                    match child.wait().await {
                        Ok(status) if decoder_eof && !status.success() => {
                            playback_error = Some(format!("FFmpeg {}", status));
                        }
                        Err(e) => playback_error = Some(e.to_string()),
                        _ => {}
                    }
                }
                let detail = match errors {
                    Some(errors) => errors.await.unwrap_or_default(),
                    None => String::new(),
                };
                if playback_error.is_some() && !detail.is_empty() {
                    playback_error = Some(detail);
                }
                if this.stream_id.load(Ordering::SeqCst) != id {
                    return;
                }
                // 오디오(ffplay)도 함께 종료한다.
                if let Some(audio) = this.audio.lock().unwrap().take() {
                    kill_audio_playback(audio);
                }
                let w2 = w.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if this.stream_id.load(Ordering::SeqCst) != id {
                        return;
                    }
                    if let Some(ui) = w2.upgrade() {
                        ui.set_is_playing(false);
                        if let Some(error) = playback_error {
                            ui.set_edit_status_text(format!("재생 실패: {}", error).into());
                        }
                    }
                });
            }
        });
    }
}

pub fn playback_range(ui: &MainWindow) -> PlaybackRange {
    PlaybackRange::new(
        ui.get_video_duration(),
        ui.get_trim_start(),
        ui.get_trim_end(),
        ui.get_playback_limited(),
    )
}

/// Seek or rewind using the same limits. Rewind and live range corrections can
/// resume playback; ordinary timeline scrubbing continues to pause it.
pub fn move_preview(
    ui: &MainWindow,
    requester: &Arc<FrameRequester>,
    time: f32,
    keep_playing: bool,
) {
    let range = playback_range(ui);
    let time = range.clamp(time);
    let resume = keep_playing && ui.get_is_playing() && !range.finished(time);
    ui.set_preview_time(time);
    ui.set_time_preview_text(seconds_to_hms(time).into());
    if !ui.get_video_loaded() {
        return;
    }
    let ffmpeg = ui.get_ffmpeg_path().to_string();
    if ffmpeg.is_empty() {
        requester.stop_playback();
        ui.set_is_playing(false);
        return;
    }
    let path = PathBuf::from(ui.get_current_video_path().to_string());
    ui.set_is_playing(resume);
    if resume {
        // Read live selection bounds on each UI frame so changing the end or
        // disabling the limit during playback takes effect immediately.
        requester.start_playback(
            ui.as_weak(),
            ffmpeg,
            path,
            time,
            ui.get_video_duration(),
            ui.get_video_fps(),
            ui.get_video_width(),
            ui.get_video_height(),
        );
    } else {
        // EOF has no frame; keep the selected position and decode just before it.
        let frame_time = time.min((ui.get_video_duration() - 0.05).max(0.0));
        requester.request(
            ui.as_weak(),
            ffmpeg,
            path,
            frame_time,
            ui.get_video_width(),
            ui.get_video_height(),
        );
    }
}

pub fn enforce_playback_range(ui: &MainWindow, requester: &Arc<FrameRequester>) {
    if !ui.get_playback_limited() || !ui.get_video_loaded() {
        return;
    }
    let range = playback_range(ui);
    let time = ui.get_preview_time();
    let clamped = range.clamp(time);
    if time != clamped || (ui.get_is_playing() && range.finished(clamped)) {
        move_preview(ui, requester, clamped, true);
    }
}

async fn extract_frame(
    ffmpeg: &str,
    path: &Path,
    t: f32,
    pw: u32,
    ph: u32,
) -> anyhow::Result<Vec<u8>> {
    let mut cmd = crate::encoder::command(ffmpeg);
    #[cfg(windows)]
    cmd.creation_flags(0x08000000);
    let out = cmd
        .args(["-hide_banner", "-loglevel", "error", "-ss"])
        .arg(format!("{:.3}", t))
        .arg("-i")
        .arg(path)
        // 입력 -ss(정확한 시크)만 사용한다. 입력+출력 이중 -ss 는 ffmpeg 8.x 에서
        // 출력 -ss 가 입력 시크 지점을 기준으로 적용되어 출력이 비어 추출이 실패한다.
        .args(["-vf"])
        .arg(format!("scale={}:{},setsar=1", pw, ph))
        .args([
            "-frames:v",
            "1",
            "-an",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgba",
            "-y",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .stdin(Stdio::null())
        .output()
        .await?;
    anyhow::ensure!(
        !out.stdout.is_empty(),
        "프레임 추출 실패 (시간이 영상 끝을 벗어났을 수 있습니다)"
    );
    Ok(out.stdout)
}

/// 버퍼를 끝까지 채울 때까지 읽는다. EOF 이면 false 를 반환한다.
async fn read_exact_or_eof(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    buf: &mut [u8],
) -> std::io::Result<bool> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]).await {
            Ok(0) => return Ok(false),
            Ok(n) => filled += n,
            Err(e) => return Err(e),
        }
    }
    Ok(true)
}

#[cfg(test)]
mod preview_tests {
    use super::*;

    #[tokio::test]
    async fn frame_reader_distinguishes_complete_frames_from_eof() {
        let mut input = &b"abcdefghij"[..];
        let mut frame = [0u8; 4];
        assert!(read_exact_or_eof(&mut input, &mut frame).await.unwrap());
        assert_eq!(&frame, b"abcd");
        assert!(read_exact_or_eof(&mut input, &mut frame).await.unwrap());
        assert_eq!(&frame, b"efgh");
        assert!(!read_exact_or_eof(&mut input, &mut frame).await.unwrap());
    }

    #[tokio::test]
    #[ignore = "Requires FFmpeg and VIDEOENCODER_TEST_VIDEO pointing to heart_ani.mp4"]
    async fn heart_animation_decodes_to_eof_and_after_seek() {
        let ffmpeg = which::which("ffmpeg")
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let path = PathBuf::from(std::env::var_os("VIDEOENCODER_TEST_VIDEO").unwrap());
        let (dur, w, h, fps) = probe_video(&ffmpeg, &path).await.unwrap();
        assert!((dur - 18.133333).abs() < 0.01);
        assert_eq!((w, h, fps), (680.0, 1440.0, 30.0));
        let (pw, ph) = preview_size(w, h);
        for (t0, expected_frames) in [(0.0, 544), (9.0, 274), (17.8, 10)] {
            let mut child = preview_playback_command(&ffmpeg, &path, t0, pw, ph)
                .spawn()
                .unwrap();
            let mut stdout = child.stdout.take().unwrap();
            let errors = tokio::spawn(read_preview_errors(child.stderr.take().unwrap()));
            let mut buf = vec![0; (pw * ph * 4) as usize];
            let mut frames = 0;
            while read_exact_or_eof(&mut stdout, &mut buf).await.unwrap() {
                frames += 1;
            }
            let status = child.wait().await.unwrap();
            let detail = errors.await.unwrap();
            assert!(status.success(), "seek {t0}: {detail}");
            assert_eq!(frames, expected_frames, "seek {t0}: {detail}");
            println!("seek {t0:.1}s: {frames} complete RGBA frames, {pw}x{ph}");
        }
        // 단일 프레임 시크도 같은 파일에서 정상 작동해야 한다.
        let frame = extract_frame(&ffmpeg, &path, 9.0, pw, ph).await.unwrap();
        assert_eq!(frame.len(), (pw * ph * 4) as usize);
    }

    #[tokio::test]
    #[ignore = "Requires FFmpeg"]
    async fn decoder_failure_preserves_ffmpeg_error() {
        let ffmpeg = which::which("ffmpeg")
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let path =
            std::env::temp_dir().join(format!("videoencoder-missing-{}.mp4", std::process::id()));
        assert!(!path.exists());
        let mut child = preview_playback_command(&ffmpeg, &path, 0.0, 2, 2)
            .spawn()
            .unwrap();
        let errors = tokio::spawn(read_preview_errors(child.stderr.take().unwrap()));
        let mut frame = [0u8; 16];
        assert!(
            !read_exact_or_eof(&mut child.stdout.take().unwrap(), &mut frame)
                .await
                .unwrap()
        );
        assert!(!child.wait().await.unwrap().success());
        let detail = errors.await.unwrap();
        assert!(detail.contains("Error opening input"), "{detail}");
        assert!(detail.contains("videoencoder-missing-"), "{detail}");
    }
}
