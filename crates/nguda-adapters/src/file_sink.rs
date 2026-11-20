//! The file sink behind the [`FileSink`] port: whole-file writes that never leave a
//! partial file behind.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nguda_core::{FileSink, FileSinkError};

/// Writes to a sibling temp file, then renames it over the target, so the target
/// is either the old file or the whole new one. Blocking.
#[derive(Clone, Copy, Debug, Default)]
pub struct FsFileSink;

impl FileSink for FsFileSink {
    fn write(&self, path: &Path, contents: &[u8]) -> Result<(), FileSinkError> {
        let temp = temp_path(path)?;
        let result = write_new(&temp, contents).and_then(|()| fs::rename(&temp, path));
        result.map_err(|e| {
            let _ = fs::remove_file(&temp);
            FileSinkError::new(e.to_string())
        })
    }
}

/// Numbers each write's temp file, so concurrent writes never share one.
static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

/// `.<name>.<pid>.<n>.nguda-tmp` beside `path`, unique per write in this process.
fn temp_path(path: &Path) -> Result<PathBuf, FileSinkError> {
    let name = path
        .file_name()
        .ok_or_else(|| FileSinkError::new("the path has no file name"))?;
    let mut temp_name = std::ffi::OsString::from(".");
    temp_name.push(name);
    let n = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
    temp_name.push(format!(".{}.{n}.nguda-tmp", std::process::id()));
    Ok(path.with_file_name(temp_name))
}

fn write_new(temp: &Path, contents: &[u8]) -> std::io::Result<()> {
    let mut file = fs::File::create(temp)?;
    file.write_all(contents)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn entries(dir: &TempDir) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn each_write_gets_its_own_temp_file() {
        let path = Path::new("/tmp/out.csv");
        assert_ne!(temp_path(path).unwrap(), temp_path(path).unwrap());
    }

    #[test]
    fn writes_the_exact_bytes() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("out.csv");
        let bytes = b"\xEF\xBB\xBFdate\r\nCaf\xC3\xA9\r\n";
        FsFileSink.write(&path, bytes).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(entries(&dir), vec!["out.csv"], "no temp file left");
    }

    #[test]
    fn replaces_an_existing_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("out.csv");
        fs::write(&path, b"an older and much longer file").unwrap();
        FsFileSink.write(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        assert_eq!(entries(&dir), vec!["out.csv"]);
    }

    #[test]
    fn a_missing_parent_dir_errs_without_leaving_a_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("missing").join("out.csv");
        let err = FsFileSink.write(&path, b"secret contents").unwrap_err();
        assert!(!err.message.is_empty());
        assert!(!err.message.contains("secret contents"), "{err}");
        assert!(!path.exists());
        assert!(entries(&dir).is_empty());
    }

    #[test]
    fn a_failed_rename_removes_the_temp_file() {
        let dir = TempDir::new().unwrap();
        // A non-empty directory at the target makes the rename fail.
        let path = dir.path().join("out.csv");
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), b"x").unwrap();
        FsFileSink.write(&path, b"data").unwrap_err();
        assert_eq!(entries(&dir), vec!["out.csv"], "temp file removed");
    }
}
