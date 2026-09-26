//! The cell size classes of the onion data plane.
//!
//! A loop's class `b` is one of these buckets above 4 KiB (#834 D6), and a cell of the loop is
//! exactly `b` bytes: the class is the cell's length, visible to every hop, and the only size a
//! hop can observe.

use serde::Deserialize;
use serde::Serialize;

/// Public size classes of onion cells.
///
/// The class is intentionally visible while everything inside the cell is not. A small class
/// set bounds padding overhead without exposing a byte-accurate traffic fingerprint.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[repr(u8)]
pub enum OnionCellBucket {
    /// Four KiB; below every loop class, since the fixed header alone is 2919 bytes.
    KiB4,
    /// Sixteen KiB, the default loop class.
    KiB16,
    /// Sixty-four KiB.
    KiB64,
    /// 256 KiB.
    KiB256,
    /// One MiB.
    MiB1,
    /// Four MiB.
    MiB4,
    /// Twelve MiB. Admission charges the cell's full class, `b / 16 KiB` units.
    MiB12,
}

impl OnionCellBucket {
    /// Every bucket, in increasing size.
    pub(crate) const ALL: [Self; 7] = [
        Self::KiB4,
        Self::KiB16,
        Self::KiB64,
        Self::KiB256,
        Self::MiB1,
        Self::MiB4,
        Self::MiB12,
    ];

    /// Return the cell length `b` of this class.
    pub const fn cell_bytes(self) -> usize {
        match self {
            Self::KiB4 => 4 * 1024,
            Self::KiB16 => 16 * 1024,
            Self::KiB64 => 64 * 1024,
            Self::KiB256 => 256 * 1024,
            Self::MiB1 => 1024 * 1024,
            Self::MiB4 => 4 * 1024 * 1024,
            Self::MiB12 => 12 * 1024 * 1024,
        }
    }
}
