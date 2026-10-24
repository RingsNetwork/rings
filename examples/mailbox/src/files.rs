//! The one file effect the mailbox's state needs: replacing a file atomically.

use std::path::Path;

use tokio::io::AsyncWriteExt;

/// Replace the file `path` with `bytes`, atomically: written beside it and synced, then renamed
/// over it, so a crash leaves the old file or the new one, never a torn or empty one. On
/// failure, the path the failure concerns comes with it.
pub async fn write_atomic(
    path: &Path,
    bytes: &[u8],
) -> Result<(), (std::path::PathBuf, std::io::Error)> {
    let staging = path.with_extension("tmp");
    let failed = |error| (staging.clone(), error);
    let mut file = tokio::fs::File::create(&staging).await.map_err(failed)?;
    file.write_all(bytes).await.map_err(failed)?;
    file.sync_all().await.map_err(failed)?;
    drop(file);
    tokio::fs::rename(&staging, path)
        .await
        .map_err(|error| (path.to_path_buf(), error))
}
