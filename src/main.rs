#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
slint::include_modules!();
mod conversion;
mod editor;
mod encoder;
mod job_queue;
mod language;
mod output;
mod playback_range;
mod preview;
mod probe;
mod settings;
#[cfg(test)]
mod ui_tests;
mod windows;

use slint::ComponentHandle;
use std::sync::{Arc, Mutex};

use language::{Language, diagnostic};

fn seconds_to_hms(seconds: f32) -> String {
    let total = seconds.max(0.0) as u64;
    format!(
        "{:02}:{:02}:{:02}",
        total / 3600,
        (total % 3600) / 60,
        total % 60
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let ui = MainWindow::new()?;
    let settings = settings::setup(&ui, settings::settings_path());
    let taskbar = windows::TaskbarProgress::new();
    let weak = ui.as_weak();
    ui.on_native_theme_changed(move || {
        if let Some(ui) = weak.upgrade() {
            windows::apply_titlebar_theme(&ui);
        }
    });
    let batch_cancel = encoder::Cancel::default();
    let edit_cancel = encoder::Cancel::default();
    let conversion_cancel = encoder::Cancel::default();
    let probe_cancel = encoder::Cancel::default();
    let workers = Arc::new(Mutex::new(Vec::new()));
    let queue = job_queue::Queue::new(&ui, probe_cancel.clone(), workers.clone());
    conversion::setup(
        &ui,
        conversion_cancel.clone(),
        probe_cancel.clone(),
        workers.clone(),
        taskbar.clone(),
    );
    job_queue::setup(
        &ui,
        queue.clone(),
        batch_cancel.clone(),
        workers.clone(),
        taskbar.clone(),
    );
    let preview = editor::setup(
        &ui,
        queue,
        edit_cancel.clone(),
        workers.clone(),
        taskbar.clone(),
    );
    windows::setup(&ui, &taskbar);
    let result = ui.run();
    batch_cancel.stop();
    edit_cancel.stop();
    conversion_cancel.stop();
    probe_cancel.stop();
    preview.stop_playback();
    let handles = std::mem::take(&mut *workers.lock().unwrap());
    for handle in handles {
        let _ = handle.await;
    }
    if let Err(e) = settings.save(&ui) {
        let language = Language::current(&ui);
        let title = language.select("Could not save settings", "설정 저장 실패");
        let error = diagnostic(&e);
        let detail = language.select(error.english, error.korean);
        eprintln!("{title}: {detail}");
        rfd::MessageDialog::new()
            .set_title(title)
            .set_description(detail.as_str())
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
    result?;
    Ok(())
}
