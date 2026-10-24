//! Presence markers of an authoritative file store, and the platform's durable directory
//! mutations (#915; durable-records, Laws 2–5).
//!
//! **Presence law.** The marker of record name `n` is the empty file `n.p` beside it. A marker
//! is created only after the rename that made `n` a record was flushed, and removed before the
//! record, so in every state a crash leaves, `P ⊆ R` (markers name records). A restore reads
//! `P \ R` as records lost outside the store (missing), and repairs `R \ P` (a crash between a
//! rename and its marker) by marking those records. A marker is not a record: it is never
//! indexed, counted, or charged against the budget.
//!
//! ```text
//! put n (authoritative):  write n.tmp ; sync ; rename n.tmp → n ; sync dir ; [n ∉ P: mark n]
//! mark n:                 write n.p.tmp ; rename n.p.tmp → n.p ; sync dir
//! remove n:               remove n.p ; remove n ; sync dir
//! ```
//!
//! **Durable directory mutations.** On unix a rename or a removal is made durable by the
//! directory flush that follows it. Windows cannot open a directory for flushing through the
//! standard library, so there every rename of a durable store is write-through
//! (`MoveFileExW` with `MOVEFILE_WRITE_THROUGH`, which does not return until the move is on
//! disk), and a durable removal renames its file to a `.tmp` name write-through before deleting
//! it: the deletion need not be durable, since a `.tmp` file left by a crash is removed at the
//! next open and is never a record.

use std::path::Path;
use std::path::PathBuf;

/// The suffix of a presence marker's file name.
const MARKER_SUFFIX: &str = ".p";

/// The file name of the presence marker of record name `name` (pure).
pub(super) fn marker_name(name: &str) -> String {
    format!("{name}{MARKER_SUFFIX}")
}

/// The record name a presence marker's file name marks, or `None` for any other file name
/// (pure). Only the image of [`marker_name`] of a record name is a marker.
pub(super) fn marked_record_name(file_name: &str) -> Option<&str> {
    file_name
        .strip_suffix(MARKER_SUFFIX)
        .filter(|name| super::is_record_name(name))
}

/// The path of the temporary file a write of `path` goes through: `path` with `.tmp` appended,
/// which the open removes as an interrupted write's leftover.
fn temporary_of(path: &Path) -> PathBuf {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    PathBuf::from(temporary)
}

/// Create the presence marker of record name `name` in `root` through a temporary file and a
/// rename, write-through iff `durable` on Windows; the caller flushes the directory on unix.
pub(super) fn create_marker(root: &Path, name: &str, durable: bool) -> std::io::Result<()> {
    let marker = root.join(marker_name(name));
    let temporary = temporary_of(&marker);
    std::fs::File::create(&temporary)?;
    rename(&temporary, &marker, durable)
}

/// Rename `from` over `to`; iff `durable` on Windows, write-through (module docs).
pub(super) fn rename(from: &Path, to: &Path, durable: bool) -> std::io::Result<()> {
    if durable {
        return rename_write_through(from, to);
    }
    std::fs::rename(from, to)
}

/// Remove `path`, treating an already absent file as removed; iff `durable` on Windows, through
/// a write-through rename to its temporary name first (module docs).
pub(super) fn remove(path: &Path, durable: bool) -> std::io::Result<()> {
    let target = if durable && cfg!(windows) {
        let temporary = temporary_of(path);
        match rename(path, &temporary, durable) {
            Ok(()) => temporary,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        }
    } else {
        path.to_path_buf()
    };
    match std::fs::remove_file(target) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// A plain rename: on unix the directory flush that follows makes it durable.
#[cfg(not(windows))]
fn rename_write_through(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::rename(from, to)
}

/// `MoveFileExW(from, to, MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH)`: the rename does
/// not return until the move is on disk.
#[cfg(windows)]
fn rename_write_through(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING;
    use windows_sys::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH;

    let wide = |path: &Path| {
        path.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>()
    };
    let (from, to) = (wide(from), wide(to));
    // SAFETY: both arguments are NUL-terminated UTF-16 strings that outlive the call, and
    // `MoveFileExW` reads them only during it.
    let moved = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}
