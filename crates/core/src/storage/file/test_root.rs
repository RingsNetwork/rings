//! A temporary store root for tests, removed when dropped, so a failing assertion leaks nothing.

use std::ops::Deref;
use std::path::Path;
use std::path::PathBuf;

/// A fresh directory path under the system temporary directory, removed with its contents on
/// drop (best effort: a removal error is ignored, as the directory is scratch).
#[derive(Debug)]
pub(crate) struct TempRoot(PathBuf);

impl TempRoot {
    /// A fresh, not yet created root whose name carries `label`.
    pub(crate) fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!("rings-{label}-{}", uuid::Uuid::new_v4())))
    }
}

impl Deref for TempRoot {
    type Target = Path;

    fn deref(&self) -> &Path {
        self.0.as_path()
    }
}

impl AsRef<Path> for TempRoot {
    fn as_ref(&self) -> &Path {
        self.0.as_path()
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
