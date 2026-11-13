use crate::{
    MainWindow, conversion, editor,
    encoder::Cancel,
    job_queue::{self, Queue},
    windows::TaskbarProgress,
};
use slint::{
    ComponentHandle, Model,
    platform::{
        EventLoopProxy, Platform, WindowAdapter,
        software_renderer::{MinimalSoftwareWindow, RepaintBufferType},
    },
};
use std::{
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
};

type Event = Box<dyn FnOnce() + Send>;

struct HeadlessPlatform(Rc<MinimalSoftwareWindow>, mpsc::Sender<Event>);

impl Platform for HeadlessPlatform {
    fn create_window_adapter(&self) -> Result<Rc<dyn WindowAdapter>, slint::PlatformError> {
        Ok(self.0.clone())
    }

    fn new_event_loop_proxy(&self) -> Option<Box<dyn EventLoopProxy>> {
        Some(Box::new(HeadlessProxy(self.1.clone())))
    }
}

struct HeadlessProxy(mpsc::Sender<Event>);

impl EventLoopProxy for HeadlessProxy {
    fn quit_event_loop(&self) -> Result<(), slint::EventLoopError> {
        Ok(())
    }

    fn invoke_from_event_loop(&self, event: Event) -> Result<(), slint::EventLoopError> {
        self.0
            .send(event)
            .map_err(|_| slint::EventLoopError::EventLoopTerminated)
    }
}

#[test]
fn dropped_files_render_in_queue() {
    std::thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(render_dropped_files)
        .unwrap()
        .join()
        .unwrap();
}

fn render_dropped_files() {
    let window = MinimalSoftwareWindow::new(RepaintBufferType::NewBuffer);
    let (send, events) = mpsc::channel();
    slint::platform::set_platform(Box::new(HeadlessPlatform(window.clone(), send))).unwrap();
    let ui = MainWindow::new().unwrap();
    let queue = Queue::new(&ui);
    let workers = Arc::new(Mutex::new(Vec::new()));
    let taskbar = TaskbarProgress::new();
    conversion::setup(
        &ui,
        Cancel::default(),
        Cancel::default(),
        workers.clone(),
        taskbar.clone(),
    );
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
    ui.set_active_tab(2);
    assert!(ui.get_conversion_tab_active());
    let other = directory.join("second.gif");
    std::fs::write(&other, []).unwrap();
    ui.invoke_files_dropped(format!("{}|{}", path.display(), other.display()).into());
    assert_eq!(ui.get_conversion_jobs().row_count(), 2);
    assert_eq!(
        ui.get_conversion_jobs().row_data(0).unwrap().path.as_str(),
        path.to_string_lossy()
    );
    assert_eq!(queue.files.row_count(), 0);
    ui.invoke_move_conversion_job(0, 1);
    assert_eq!(
        ui.get_conversion_jobs().row_data(0).unwrap().path.as_str(),
        other.to_string_lossy()
    );
    ui.set_ffmpeg_path("ffmpeg".into());
    ui.set_conversion_height("0".into());
    ui.invoke_start_conversion(false);
    assert!(!ui.get_conversion_busy());
    assert!(ui.get_conversion_status().contains("세로"));
    render();
    ui.invoke_remove_conversion_job(0);
    assert_eq!(ui.get_conversion_jobs().row_count(), 1);
    ui.invoke_clear_conversion_list();
    assert_eq!(ui.get_conversion_jobs().row_count(), 0);
    if std::env::var_os("VIDEOENCODER_TEST_BATCH").is_some() {
        check_conversion_batch(&ui, &events);
        render();
    }
    ui.hide().unwrap();
    preview.stop_playback();
    std::fs::remove_file(path).unwrap();
    std::fs::remove_file(other).unwrap();
    std::fs::remove_dir(directory).unwrap();
}

fn check_conversion_batch(ui: &MainWindow, events: &mpsc::Receiver<Event>) {
    let runtime = tokio::runtime::Runtime::new().unwrap();
    let _runtime_guard = runtime.enter();
    let scratch = crate::encoder::integration_tests::TestDir::new();
    let sources = [scratch.0.join("first.mkv"), scratch.0.join("second.mkv")];
    for (source, size) in sources.iter().zip(["96x64", "64x48"]) {
        let fixture = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={size}:rate=10:duration=0.4"))
            .args(["-c:v", "ffv1"])
            .arg(source)
            .output()
            .unwrap();
        assert!(
            fixture.status.success(),
            "{}",
            String::from_utf8_lossy(&fixture.stderr)
        );
    }
    let invalid = scratch.0.join("invalid.mkv");
    std::fs::write(&invalid, []).unwrap();
    ui.set_conversion_height("".into());
    ui.set_conversion_width("48".into());
    ui.set_output_folder(scratch.0.to_string_lossy().as_ref().into());
    ui.invoke_files_dropped(
        format!(
            "{}|{}|{}",
            sources[0].display(),
            invalid.display(),
            sources[1].display()
        )
        .into(),
    );
    let jobs = ui.get_conversion_jobs();
    assert_eq!(jobs.row_count(), 3);
    ui.invoke_move_conversion_job(2, -1);
    for _ in 0..3 {
        events
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()();
    }
    assert_eq!(
        jobs.row_data(0).unwrap().source_info,
        "96 × 64 px · 00:00:00.400"
    );
    assert_eq!(
        jobs.row_data(1).unwrap().source_info,
        "64 × 48 px · 00:00:00.400"
    );
    ui.invoke_move_conversion_job(2, -1);
    ui.invoke_start_conversion(false);
    assert!(ui.get_conversion_busy());
    while ui.get_conversion_busy() {
        events
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap()();
    }
    assert_eq!(jobs.row_data(0).unwrap().status, "완료");
    assert_eq!(jobs.row_data(1).unwrap().status, "실패");
    assert_eq!(jobs.row_data(2).unwrap().status, "완료");
    assert!(scratch.0.join("first.gif").is_file());
    assert!(scratch.0.join("second.gif").is_file());
}
