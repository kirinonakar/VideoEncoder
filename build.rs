use std::path::Path;

fn main() {
    // 1. Process Icon for Windows Executable
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        let mut res = winres::WindowsResource::new();
        res.set_icon("app.ico");
        res.compile().unwrap();
    }

    // 2. Convert app.ico to app.png for Slint runtime (Window Titlebar/Taskbar)
    // Slint's built-in image decoder doesn't always handle multi-res .ico well.
    let ico_path = Path::new("app.ico");
    let png_dest = Path::new("src/app.png");
    
    if ico_path.exists() && !png_dest.exists() {
        if let Ok(img) = image::open(ico_path) {
            let _ = img.save(png_dest);
        }
    }

    slint_build::compile("src/ui.slint").unwrap();
}
