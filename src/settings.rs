use crate::{
    MainWindow,
    encoder::{EncodeOptions, Tracks},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use slint::ComponentHandle;
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    ffmpeg: String,
    suffix: String,
    encoder: i32,
    crf: f32,
    cq: f32,
    output_folder: String,
    all_audio: bool,
    subtitles: bool,
    exact_cut: bool,
    dark: bool,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            ffmpeg: String::new(),
            suffix: "_h265".into(),
            encoder: 1,
            crf: 19.0,
            cq: 20.0,
            output_folder: String::new(),
            all_audio: true,
            subtitles: false,
            exact_cut: false,
            dark: false,
        }
    }
}
pub fn settings_path() -> PathBuf {
    std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from))
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir)
                .join(".config")
        })
        .join("VideoEncoder")
        .join("settings.json")
}
impl Settings {
    pub fn read(path: &Path) -> Result<Self> {
        serde_json::from_slice(&std::fs::read(path)?).context("설정 JSON 읽기 실패")
    }
    pub fn capture(ui: &MainWindow) -> Self {
        Self {
            ffmpeg: ui.get_ffmpeg_path().into(),
            suffix: ui.get_output_suffix().into(),
            encoder: ui.get_encoder_index(),
            crf: ui.get_crf_value(),
            cq: ui.get_cq_value(),
            output_folder: ui.get_output_folder().into(),
            all_audio: ui.get_all_audio(),
            subtitles: ui.get_preserve_subtitles(),
            exact_cut: ui.get_cut_mode() == 1,
            dark: ui.get_dark_mode(),
        }
    }
    pub fn apply(&self, ui: &MainWindow) {
        ui.set_ffmpeg_path(self.ffmpeg.clone().into());
        ui.set_output_suffix(self.suffix.clone().into());
        ui.set_encoder_index(self.encoder.clamp(0, 1));
        ui.set_crf_value(if self.crf.is_finite() {
            self.crf.clamp(0.0, 51.0)
        } else {
            19.0
        });
        ui.set_cq_value(if self.cq.is_finite() {
            self.cq.clamp(0.0, 51.0)
        } else {
            20.0
        });
        ui.set_output_folder(self.output_folder.clone().into());
        ui.set_all_audio(self.all_audio);
        ui.set_preserve_subtitles(self.subtitles);
        ui.set_cut_mode(i32::from(self.exact_cut));
        ui.set_dark_mode(self.dark);
    }
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temp, path).context("설정 파일 저장 실패")
    }
}
pub fn tracks(ui: &MainWindow) -> Tracks {
    Tracks {
        all_audio: ui.get_all_audio(),
        subtitles: ui.get_preserve_subtitles(),
    }
}
pub fn encode_options(ui: &MainWindow) -> EncodeOptions {
    let encoder = ui.get_encoder_index();
    EncodeOptions {
        encoder,
        quality: if encoder == 1 {
            ui.get_cq_value()
        } else {
            ui.get_crf_value()
        } as i32,
        tracks: tracks(ui),
    }
}
fn discover_ffmpeg() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(name)))
        .filter(|p| p.is_file())
        .or_else(|| which::which(name).ok())
}
pub fn setup(ui: &MainWindow) {
    match Settings::read(&settings_path()) {
        Ok(settings) => settings.apply(ui),
        Err(e) if settings_path().exists() => {
            ui.set_current_file_text(format!("설정 불러오기 실패: {e:#}").into())
        }
        _ => Settings::default().apply(ui),
    }
    if ui.get_ffmpeg_path().is_empty() {
        if let Some(path) = discover_ffmpeg() {
            ui.set_ffmpeg_path(path.to_string_lossy().into_owned().into());
        }
    }
    let weak = ui.as_weak();
    ui.on_refresh_ffmpeg_path(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_is_encoding() || ui.get_edit_busy() {
                return;
            }
            match discover_ffmpeg() {
                Some(path) => {
                    ui.set_ffmpeg_path(path.to_string_lossy().into_owned().into());
                    ui.set_current_file_text("FFmpeg 경로를 자동 선택했습니다.".into());
                }
                None => ui.set_current_file_text(
                    "FFmpeg를 찾지 못했습니다. 프로그램 폴더 또는 PATH를 확인하세요.".into(),
                ),
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_save_settings(move || {
        if let Some(ui) = weak.upgrade() {
            let result = Settings::capture(&ui).save(&settings_path());
            ui.set_current_file_text(
                match result {
                    Ok(()) => format!("설정 저장: {}", settings_path().display()),
                    Err(e) => format!("설정 저장 실패: {e:#}"),
                }
                .into(),
            );
        }
    });
    let weak = ui.as_weak();
    ui.on_export_preset(move || {
        if let Some(ui) = weak.upgrade() {
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .set_file_name("preset.json")
                .save_file()
            {
                let result = Settings::capture(&ui).save(&path);
                ui.set_current_file_text(
                    match result {
                        Ok(()) => "프리셋 저장 완료".into(),
                        Err(e) => format!("프리셋 저장 실패: {e:#}"),
                    }
                    .into(),
                );
            }
        }
    });
    let weak = ui.as_weak();
    ui.on_import_preset(move || {
        if let Some(ui) = weak.upgrade() {
            if ui.get_is_encoding() || ui.get_edit_busy() {
                return;
            }
            if let Some(path) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
            {
                match Settings::read(&path) {
                    Ok(settings) => {
                        settings.apply(&ui);
                        ui.set_current_file_text("프리셋 불러오기 완료".into());
                    }
                    Err(e) => {
                        ui.set_current_file_text(format!("프리셋 불러오기 실패: {e:#}").into())
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_roundtrip_and_older_partial_presets() {
        let settings: Settings =
            serde_json::from_str(r#"{"encoder":0,"crf":23,"all_audio":false}"#).unwrap();
        assert_eq!(settings.encoder, 0);
        assert_eq!(settings.crf, 23.0);
        assert!(!settings.all_audio);
        assert_eq!(settings.suffix, "_h265");
        let decoded: Settings =
            serde_json::from_slice(&serde_json::to_vec(&settings).unwrap()).unwrap();
        assert_eq!(decoded.crf, settings.crf);
        assert!(serde_json::from_str::<Settings>("invalid").is_err());
        let path =
            std::env::temp_dir().join(format!("videoencoder-settings-{}.json", std::process::id()));
        settings.save(&path).unwrap();
        Settings::default().save(&path).unwrap();
        assert_eq!(Settings::read(&path).unwrap().encoder, 1);
        assert!(!path.with_extension("json.tmp").exists());
        std::fs::remove_file(path).unwrap();
    }
}
