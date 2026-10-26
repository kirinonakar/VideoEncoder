use crate::{
    MainWindow, editor,
    encoder::Cancel,
    job_queue::{self, Queue},
    windows::TaskbarProgress,
};
use slint::{
    ComponentHandle, Model,
    platform::{
        Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use std::{
    rc::Rc,
    sync::{Arc, Mutex},
};

struct HeadlessPlatform(Rc<MinimalSoftwareWindow>);

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }
}

#[test]
fn dropped_files_render_in_queue() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    slint::platform::set_platform(Box::new(HeadlessPlatform(window.clone()))).unwrap();
    let ui = MainWindow::new().unwrap();
    let queue = Queue::new(&ui);
    let workers = Arc::new(Mutex::new(Vec::new()));
    let taskbar = TaskbarProgress::new();
    job_queue::setup(
        &ui,
        queue.clone(),
        Cancel::default(),
        workers.clone(),
        taskbar.clone(),
    );
    let preview = editor::setup(&ui, queue.clone(), Cancel::default(), workers, taskbar);
    ui.set_ffmpeg_path("".into());
    window.set_size(slint::PhysicalSize::new(900, 800));
    ui.show().unwrap();
    let mut pixels = vec![slint::Rgb8Pixel::default(); 900 * 800];
    let mut render = || {
        slint::platform::update_timers_and_animations();
        window.request_redraw();
        window.draw_if_needed(|renderer| {
            renderer.render(&mut pixels, 900);
        });
    };
    render();
    let directory =
        std::env::temp_dir().join(format!("videoencoder-ui-drop-{}", std::process::id()));
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!(
        "긴 파일명_日本語_{}.mp4",
        "long_filename_".repeat(10)
    ));
    std::fs::write(&path, []).unwrap();
    ui.invoke_files_dropped(path.to_string_lossy().as_ref().into());
    assert_eq!(queue.files.row_count(), 1);
    assert_eq!(ui.get_job_list().row_count(), 1);
    render();
    ui.invoke_files_dropped(directory.to_string_lossy().as_ref().into());
    assert_eq!(queue.files.row_count(), 2);
    render();
    ui.invoke_move_job(0, 1);
    render();
    ui.invoke_remove_job(0);
    assert_eq!(ui.get_job_list().row_count(), 1);
    render();
    window.set_size(slint::PhysicalSize::new(800, 760));
    render();
    ui.invoke_clear_list();
    assert_eq!(ui.get_job_list().row_count(), 0);
    render();
    ui.hide().unwrap();
    preview.stop_playback();
    std::fs::remove_file(path).unwrap();
    std::fs::remove_dir(directory).unwrap();
}
