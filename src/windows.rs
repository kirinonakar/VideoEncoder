use crate::MainWindow;
use slint::ComponentHandle;
use std::sync::OnceLock;
// --- Windows-specific Drag & Drop Hooks ---
#[cfg(target_os = "windows")]
use windows_sys::Win32::Foundation::{GetLastError, HWND, LPARAM, LRESULT, WPARAM};
#[cfg(target_os = "windows")]
use windows_sys::Win32::System::Ole::RevokeDragDrop;
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
#[cfg(target_os = "windows")]
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, ChangeWindowMessageFilterEx, GWLP_WNDPROC, MSGFLT_ALLOW, SetWindowLongPtrW,
    WM_DROPFILES, WNDPROC,
};

static APP_WINDOW_HANDLE: OnceLock<slint::Weak<MainWindow>> = OnceLock::new();
#[cfg(target_os = "windows")]
static mut ORIGINAL_WNDPROC: WNDPROC = None;

/// Windows 작업표시줄의 앱 아이콘에 진행률을 표시한다.
///
/// 작업표시줄 COM 객체는 호출한 스레드의 COM 초기화 상태에 영향을 받으므로,
/// 각 업데이트에서 짧은 수명으로 생성하고 해제한다. 이렇게 하면 FFmpeg를
/// 감시하는 Tokio 작업 스레드에서도 안전하게 갱신할 수 있다.
#[cfg(target_os = "windows")]
mod taskbar_progress {
    use std::ffi::c_void;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicIsize, Ordering};

    use ::windows::Win32::Foundation::{HWND, RPC_E_CHANGED_MODE, S_FALSE, S_OK};
    use ::windows::Win32::System::Com::{
        CLSCTX_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
    };
    use ::windows::Win32::UI::Shell::{ITaskbarList3, TBPF_NOPROGRESS, TBPF_NORMAL, TaskbarList};

    #[derive(Clone)]
    pub struct TaskbarProgress {
        hwnd: Arc<AtomicIsize>,
    }

    impl TaskbarProgress {
        pub fn new() -> Self {
            Self {
                hwnd: Arc::new(AtomicIsize::new(0)),
            }
        }

        pub fn set_hwnd(&self, hwnd: isize) {
            self.hwnd.store(hwnd, Ordering::Release);
        }

        pub fn set_progress(&self, progress: f32) {
            let completed = (progress.clamp(0.0, 1.0) * 10_000.0).round() as u64;
            self.with_taskbar(|taskbar, hwnd| unsafe {
                let _ = taskbar.SetProgressState(hwnd, TBPF_NORMAL);
                let _ = taskbar.SetProgressValue(hwnd, completed, 10_000);
            });
        }

        pub fn clear(&self) {
            self.with_taskbar(|taskbar, hwnd| unsafe {
                let _ = taskbar.SetProgressState(hwnd, TBPF_NOPROGRESS);
            });
        }

        fn with_taskbar(&self, callback: impl FnOnce(&ITaskbarList3, HWND)) {
            let raw_hwnd = self.hwnd.load(Ordering::Acquire);
            if raw_hwnd == 0 {
                return;
            }

            unsafe {
                let com_init = CoInitializeEx(None, COINIT_MULTITHREADED);
                let com_is_ready =
                    com_init == S_OK || com_init == S_FALSE || com_init == RPC_E_CHANGED_MODE;
                if !com_is_ready {
                    return;
                }

                if let Ok(taskbar) =
                    CoCreateInstance::<_, ITaskbarList3>(&TaskbarList, None, CLSCTX_SERVER)
                {
                    if taskbar.HrInit().is_ok() {
                        callback(&taskbar, HWND(raw_hwnd as *mut c_void));
                    }
                }

                if com_init == S_OK || com_init == S_FALSE {
                    CoUninitialize();
                }
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod taskbar_progress {
    #[derive(Clone, Default)]
    pub struct TaskbarProgress;

    impl TaskbarProgress {
        pub fn new() -> Self {
            Self
        }

        pub fn set_hwnd(&self, _hwnd: isize) {}
        pub fn set_progress(&self, _progress: f32) {}
        pub fn clear(&self) {}
    }
}

pub use taskbar_progress::TaskbarProgress;

#[cfg(target_os = "windows")]
pub fn apply_titlebar_theme(ui: &MainWindow) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::{
        DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DwmSetWindowAttribute,
    };

    let window_handle = ui.window().window_handle();
    let Ok(handle) = window_handle.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let hwnd = handle.hwnd.get() as HWND;
    let colorref = |color: slint::Color| {
        u32::from(color.red()) | (u32::from(color.green()) << 8) | (u32::from(color.blue()) << 16)
    };
    let dark = i32::from(ui.get_dark_mode());
    let background = colorref(ui.get_titlebar_background());
    let foreground = colorref(ui.get_titlebar_foreground());
    // DWM applies these to the native caption. Derive both colors from the same
    // palette as the content so switching back to light restores matching colors.
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&dark as *const i32).cast(),
            std::mem::size_of_val(&dark) as u32,
        );
        for (attribute, color) in [
            (DWMWA_CAPTION_COLOR, background),
            (DWMWA_TEXT_COLOR, foreground),
        ] {
            let _ = DwmSetWindowAttribute(
                hwnd,
                attribute as u32,
                (&color as *const u32).cast(),
                std::mem::size_of_val(&color) as u32,
            );
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub fn apply_titlebar_theme(_ui: &MainWindow) {}

#[cfg(target_os = "windows")]
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DROPFILES => {
            let hdrop = wparam as HDROP;

            unsafe {
                let count = DragQueryFileW(hdrop, 0xFFFFFFFF, std::ptr::null_mut(), 0);
                let mut paths = Vec::new();
                for i in 0..count {
                    let len = DragQueryFileW(hdrop, i, std::ptr::null_mut(), 0);
                    let mut path_buf = vec![0u16; len as usize + 1];
                    let len = DragQueryFileW(hdrop, i, path_buf.as_mut_ptr(), len + 1);
                    if len > 0 {
                        paths.push(String::from_utf16_lossy(&path_buf[..len as usize]));
                    }
                }

                if !paths.is_empty() {
                    let paths_str = paths.join("|");
                    if let Some(weak) = APP_WINDOW_HANDLE.get() {
                        let weak_clone = weak.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak_clone.upgrade() {
                                ui.invoke_files_dropped(slint::SharedString::from(
                                    paths_str.as_str(),
                                ));
                            }
                        });
                    }
                }
                DragFinish(hdrop);
            }
            return 0;
        }
        _ => {}
    }

    unsafe {
        if let Some(orig) = ORIGINAL_WNDPROC {
            CallWindowProcW(Some(orig), hwnd, msg, wparam, lparam)
        } else {
            windows_sys::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}

pub fn setup(ui: &MainWindow, taskbar: &TaskbarProgress) {
    let _ = APP_WINDOW_HANDLE.set(ui.as_weak());
    // --- Windows-specific Hook Implementation ---
    #[cfg(target_os = "windows")]
    {
        let ui_handle_clone = ui.as_weak();
        let taskbar_clone = taskbar.clone();
        slint::Timer::single_shot(std::time::Duration::from_millis(300), move || {
            if let Some(ui) = ui_handle_clone.upgrade() {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};

                let window_handle = ui.window().window_handle();
                if let Ok(handle) = window_handle.window_handle() {
                    if let RawWindowHandle::Win32(h) = handle.as_raw() {
                        let hwnd = h.hwnd.get() as HWND;
                        apply_titlebar_theme(&ui);
                        taskbar_clone.set_hwnd(h.hwnd.get());
                        println!("Slint HWND success (deferred): {:?}", hwnd);

                        unsafe {
                            let hr = RevokeDragDrop(hwnd);
                            println!("RevokeDragDrop (S_OK=0): {}", hr);

                            ChangeWindowMessageFilterEx(
                                hwnd,
                                WM_DROPFILES,
                                MSGFLT_ALLOW,
                                std::ptr::null_mut(),
                            );
                            ChangeWindowMessageFilterEx(
                                hwnd,
                                0x0049,
                                MSGFLT_ALLOW,
                                std::ptr::null_mut(),
                            );
                            ChangeWindowMessageFilterEx(
                                hwnd,
                                0x004A,
                                MSGFLT_ALLOW,
                                std::ptr::null_mut(),
                            );

                            DragAcceptFiles(hwnd, 1);
                            println!("DragAcceptFiles enabled");

                            let prev_proc = SetWindowLongPtrW(
                                hwnd,
                                GWLP_WNDPROC,
                                wnd_proc as *const () as isize,
                            );

                            if prev_proc != 0 {
                                println!("WndProc hook success. Prev addr: 0x{:X}", prev_proc);
                                type WndProcFn =
                                    unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT;
                                ORIGINAL_WNDPROC =
                                    Some(core::mem::transmute::<isize, WndProcFn>(prev_proc));
                            } else {
                                println!("Warning: SetWindowLongPtrW failed: {}", GetLastError());
                            }
                        }
                    }
                }
            }
        });
    }
}
