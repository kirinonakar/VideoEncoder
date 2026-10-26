#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
slint::include_modules!();
mod editor;
mod encoder;
mod job_queue;
mod output;
mod playback_range;
mod preview;
mod probe;
mod settings;
mod windows;
#[cfg(test)]
mod ui_tests;

use slint::ComponentHandle;
use std::sync::{Arc, Mutex};

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
    settings::setup(&ui);
    let taskbar = windows::TaskbarProgress::new();
    let weak = ui.as_weak();
    ui.on_native_theme_changed(move || {
        if let Some(ui) = weak.upgrade() {
            windows::apply_titlebar_theme(&ui);
        }
    });
    let queue = job_queue::Queue::new(&ui);
    let batch_cancel = encoder::Cancel::default();
    let edit_cancel = encoder::Cancel::default();
    let workers = Arc::new(Mutex::new(Vec::new()));
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
    preview.stop_playback();
    let handles = std::mem::take(&mut *workers.lock().unwrap());
    for handle in handles {
        let _ = handle.await;
    }
    if let Err(e) = settings::Settings::capture(&ui).save(&settings::settings_path()) {
        eprintln!("설정 저장 실패: {e:#}");
        rfd::MessageDialog::new()
            .set_title("설정 저장 실패")
            .set_description(format!("{e:#}"))
            .set_level(rfd::MessageLevel::Error)
            .show();
    }
    result?;
    Ok(())
}
