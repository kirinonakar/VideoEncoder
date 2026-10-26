//! Publish completed output without replacing any existing directory entry.
use std::{
    fs, io,
    path::{Path, PathBuf},
};

#[cfg(windows)]
fn publish_no_replace(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    // Rust's filesystem calls support extended Windows paths. Preserve that
    // support in this direct Win32 call, including for relative output folders.
    let source = fs::canonicalize(source)?;
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let target = fs::canonicalize(parent)?.join(
        target
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "출력 파일 이름 없음"))?,
    );
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // No MOVEFILE_REPLACE_EXISTING: atomically rename on NTFS/FAT/exFAT,
    // failing if another directory entry already has the target name.
    let result = unsafe {
        windows_sys::Win32::Storage::FileSystem::MoveFileExW(source.as_ptr(), target.as_ptr(), 0)
    };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn publish_no_replace(source: &Path, target: &Path) -> io::Result<()> {
    fs::hard_link(source, target)?;
    let _ = fs::remove_file(source);
    Ok(())
}

fn numbered(path: &Path, n: u32) -> PathBuf {
    if n == 0 {
        return path.to_owned();
    }
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    let mut name = format!("{stem} ({n})");
    if let Some(ext) = path.extension() {
        name.push('.');
        name.push_str(&ext.to_string_lossy());
    }
    path.with_file_name(name)
}

pub struct OutputFile {
    pub incomplete: PathBuf,
    requested: PathBuf,
}

impl OutputFile {
    pub fn reserve(requested: &Path) -> io::Result<Self> {
        let stem = requested.file_stem().unwrap_or_default().to_string_lossy();
        let mut name = format!("{stem}_incomplete");
        if let Some(ext) = requested.extension() {
            name.push('.');
            name.push_str(&ext.to_string_lossy());
        }
        let base = requested.with_file_name(name);
        for n in 0..10_000 {
            let incomplete = numbered(&base, n);
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&incomplete)
            {
                Ok(_) => {
                    return Ok(Self {
                        incomplete,
                        requested: requested.to_owned(),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "미완성 파일 이름을 확보할 수 없습니다",
        ))
    }

    pub fn finalize(&self) -> io::Result<PathBuf> {
        for n in 0..10_000 {
            let output = numbered(&self.requested, n);
            // The no-replace operation also protects against names created
            // concurrently after the encode started. Errors preserve the temp.
            match publish_no_replace(&self.incomplete, &output) {
                Ok(()) => return Ok(output),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "출력 파일 이름을 확보할 수 없습니다",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preserves_existing_outputs_temps_and_input_even_after_collision() {
        let dir = std::env::temp_dir().join(format!("videoencoder-output-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("원본.mp4");
        fs::write(&target, b"original").unwrap();
        let first = OutputFile::reserve(&target).unwrap();
        fs::write(&first.incomplete, b"first").unwrap();
        let second = OutputFile::reserve(&target).unwrap();
        fs::write(&second.incomplete, b"second").unwrap();
        assert_ne!(first.incomplete, second.incomplete);
        fs::write(numbered(&target, 1), b"concurrent-result").unwrap();
        let a = first.finalize().unwrap();
        let b = second.finalize().unwrap();
        assert_eq!(a, numbered(&target, 2));
        assert_eq!(b, numbered(&target, 3));
        assert_eq!(
            fs::read(numbered(&target, 1)).unwrap(),
            b"concurrent-result"
        );
        assert_eq!(fs::read(&target).unwrap(), b"original");
        assert_eq!(fs::read(a).unwrap(), b"first");
        assert_eq!(fs::read(b).unwrap(), b"second");
        assert!(first.finalize().is_err());
        for entry in fs::read_dir(&dir).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
        fs::remove_dir(&dir).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn finalization_failure_preserves_recoverable_file() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = std::env::temp_dir().join(format!("videoencoder-locked-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let target = dir.join("output.mp4");
        let output = OutputFile::reserve(&target).unwrap();
        fs::write(&output.incomplete, b"complete-data").unwrap();
        let lock = fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&output.incomplete)
            .unwrap();
        assert!(output.finalize().is_err());
        assert!(!target.exists());
        drop(lock);
        assert_eq!(fs::read(&output.incomplete).unwrap(), b"complete-data");
        fs::remove_file(&output.incomplete).unwrap();
        fs::remove_dir(dir).unwrap();
    }
}
