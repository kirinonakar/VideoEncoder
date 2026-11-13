use crate::{
    MainWindow,
    encoder::{EncodeOptions, Tracks},
    language::{Language, diagnostic, message},
};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use slint::{ComponentHandle, Model, VecModel};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
};

#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    #[serde(flatten)]
    options: Options,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<Language>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    presets: Vec<NamedPreset>,
}

#[derive(Clone, Serialize, Deserialize)]
struct NamedPreset {
    name: String,
    options: Options,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Options {
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
    #[serde(skip_serializing_if = "Option::is_none")]
    conversion_format: Option<i32>,
}

impl Default for Options {
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
            conversion_format: None,
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
    fn read(path: &Path) -> Result<Self> {
        serde_json::from_slice(&std::fs::read(path)?).context(message!(
            "Could not read settings JSON",
            "설정 JSON 읽기 실패"
        ))
    }
    fn capture(ui: &MainWindow, presets: Vec<NamedPreset>) -> Self {
        Self {
            options: Options::capture(ui),
            language: Some(Language::current(ui)),
            presets,
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(temp, path).context(message!(
            "Could not write settings file",
            "설정 파일 저장 실패"
        ))
    }
}

impl Options {
    fn capture(ui: &MainWindow) -> Self {
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
            conversion_format: Some(ui.get_conversion_format()),
        }
    }
    fn apply(&self, ui: &MainWindow) {
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
        if let Some(format @ 0..=2) = self.conversion_format {
            ui.set_conversion_format(format);
        }
    }
}

pub struct SettingsFile {
    path: PathBuf,
    presets: RefCell<Vec<NamedPreset>>,
}

impl SettingsFile {
    pub fn save(&self, ui: &MainWindow) -> Result<()> {
        Settings::capture(ui, self.presets.borrow().clone()).save(&self.path)
    }

    fn save_preset(&self, ui: &MainWindow, name: &str) -> Result<usize> {
        let mut settings = Settings::capture(ui, self.presets.borrow().clone());
        let preset = NamedPreset {
            name: name.to_owned(),
            options: settings.options.clone(),
        };
        let index = settings
            .presets
            .iter()
            .position(|preset| preset.name == name);
        let index = if let Some(index) = index {
            settings.presets[index] = preset;
            index
        } else {
            settings.presets.push(preset);
            settings.presets.len() - 1
        };
        settings.save(&self.path)?;
        *self.presets.borrow_mut() = settings.presets;
        Ok(index)
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
pub fn setup(ui: &MainWindow, path: PathBuf) -> Rc<SettingsFile> {
    ui.set_korean(Language::system() == Language::Ko);
    let settings = Settings::read(&path).unwrap_or_else(|error| {
        if path.exists() {
            ui.set_current_file_text(diagnostic(
                &error.context(message!("Could not load settings", "설정 불러오기 실패")),
            ));
        }
        Settings::default()
    });
    settings.options.apply(ui);
    if let Some(language) = settings.language {
        ui.set_korean(language == Language::Ko);
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
                    ui.set_current_file_text(message!(
                        "FFmpeg path selected automatically.",
                        "FFmpeg 경로를 자동 선택했습니다."
                    ));
                }
                None => ui.set_current_file_text(message!(
                    "FFmpeg was not found. Check the application folder or PATH.",
                    "FFmpeg를 찾지 못했습니다. 프로그램 폴더 또는 PATH를 확인하세요.",
                )),
            }
        }
    });
    let names = Rc::new(VecModel::from(
        std::iter::once("—".into())
            .chain(
                settings
                    .presets
                    .iter()
                    .map(|preset| preset.name.as_str().into()),
            )
            .collect::<Vec<_>>(),
    ));
    ui.set_preset_names(names.clone().into());
    let file = Rc::new(SettingsFile {
        path,
        presets: RefCell::new(settings.presets),
    });

    let weak = ui.as_weak();
    let settings = file.clone();
    ui.on_save_settings(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_current_file_text(match settings.save(&ui) {
                Ok(()) => message!(
                    "Settings saved: {}",
                    "설정 저장: {}",
                    settings.path.display()
                ),
                Err(error) => diagnostic(
                    &error.context(message!("Could not save settings", "설정 저장 실패")),
                ),
            });
        }
    });

    let weak = ui.as_weak();
    let settings = file.clone();
    ui.on_save_preset(move || {
        if let Some(ui) = weak.upgrade() {
            if !ui.get_preset_options_enabled() {
                return;
            }
            let input = ui.get_preset_name();
            let name = input.trim();
            if name.is_empty() {
                ui.set_current_file_text(message!(
                    "Enter a preset name.",
                    "프리셋 이름을 입력하세요."
                ));
                return;
            }
            ui.set_current_file_text(match settings.save_preset(&ui, name) {
                Ok(index) => {
                    let row = index + 1;
                    if row == names.row_count() {
                        names.push(name.into());
                    }
                    ui.set_preset_index(row as i32);
                    ui.set_preset_name(name.into());
                    message!("Preset saved: {}", "프리셋 저장 완료: {}", name)
                }
                Err(error) => diagnostic(
                    &error.context(message!("Could not save preset", "프리셋 저장 실패")),
                ),
            });
        }
    });

    let weak = ui.as_weak();
    let settings = file.clone();
    ui.on_select_preset(move |row| {
        if let Some(ui) = weak.upgrade() {
            if !ui.get_preset_options_enabled() {
                return;
            }
            if row <= 0 {
                ui.set_preset_name("".into());
                return;
            }
            if let Some(preset) = settings.presets.borrow().get(row as usize - 1) {
                preset.options.apply(&ui);
                ui.set_preset_name(preset.name.as_str().into());
                ui.set_current_file_text(message!(
                    "Preset loaded: {}",
                    "프리셋 불러오기 완료: {}",
                    preset.name
                ));
            }
        }
    });
    file
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn settings_roundtrip_and_older_partial_settings() {
        let settings: Settings =
            serde_json::from_str(r#"{"encoder":0,"crf":23,"all_audio":false}"#).unwrap();
        assert_eq!(settings.options.encoder, 0);
        assert_eq!(settings.options.crf, 23.0);
        assert!(!settings.options.all_audio);
        assert_eq!(settings.options.suffix, "_h265");
        assert_eq!(settings.language, None);
        assert!(settings.presets.is_empty());
        let korean: Settings = serde_json::from_str(r#"{"language":"KO"}"#).unwrap();
        assert_eq!(korean.language, Some(Language::Ko));
        let english = Settings {
            language: Some(Language::En),
            ..Settings::default()
        };
        assert_eq!(serde_json::to_value(english).unwrap()["language"], "EN");
        let decoded: Settings =
            serde_json::from_slice(&serde_json::to_vec(&settings).unwrap()).unwrap();
        assert_eq!(decoded.options.crf, settings.options.crf);
        assert!(serde_json::from_str::<Settings>("invalid").is_err());
        let path =
            std::env::temp_dir().join(format!("videoencoder-settings-{}.json", std::process::id()));
        settings.save(&path).unwrap();
        Settings::default().save(&path).unwrap();
        assert_eq!(Settings::read(&path).unwrap().options.encoder, 1);
        assert!(!path.with_extension("json.tmp").exists());
        std::fs::remove_file(path).unwrap();
    }
}
