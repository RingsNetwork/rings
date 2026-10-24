//! The one file effect the mailbox's state needs: replacing a file atomically.

use std::path::Path;

/// Replace the file `path` with `bytes`, atomically: written beside it, then renamed over it,
/// so a crash leaves the old file or the new one, never a torn one. On failure, the path the
/// failure concerns comes with it.
pub async fn write_atomic(
    path: &Path,
    bytes: &[u8],
) -> Result<(), (std::path::PathBuf, std::io::Error)> {
    let staging = path.with_extension("tmp");
    tokio::fs::write(&staging, bytes)
        .await
        .map_err(|error| (staging.clone(), error))?;
    tokio::fs::rename(&staging, path)
        .await
        .map_err(|error| (path.to_path_buf(), error))
}
