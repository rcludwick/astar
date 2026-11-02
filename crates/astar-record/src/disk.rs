// astar — Copyright (c) 2026 Rob Ludwick.
// SPDX-License-Identifier: AGPL-3.0-only
// Licensed under the GNU Affero General Public License v3.0 only. See LICENSE.

//! The free-space guard: recording refuses to start, or stops, when the
//! disk under its folder has less than 500 MB free.

use std::path::Path;

use crate::RecordError;

/// Below this many free bytes, recording stops (500 MB).
pub const MIN_FREE_BYTES: u64 = 500_000_000;

/// Bytes free to an unprivileged writer on the volume holding `path`
/// (`f_bavail * f_frsize`). `None` when the path does not exist, the call
/// fails, or the platform is not Unix.
#[cfg(unix)]
#[must_use]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: `c` is a NUL-terminated path and `st` points to writable
    // storage the size of `statvfs`; it is read only after a zero return.
    let rc = unsafe { libc::statvfs(c.as_ptr(), st.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    // SAFETY: statvfs returned 0, so it filled the struct.
    let st = unsafe { st.assume_init() };
    #[allow(clippy::useless_conversion)] // the field widths differ by platform
    let (avail, frsize) = (u64::from(st.f_bavail), u64::from(st.f_frsize));
    Some(avail.saturating_mul(frsize))
}

/// Not measured off Unix.
#[cfg(not(unix))]
#[must_use]
pub fn free_bytes(_path: &Path) -> Option<u64> {
    None
}

/// `Err(DiskLow)` when the volume holding `path` has less than
/// [`MIN_FREE_BYTES`] free. A volume that cannot be measured passes.
pub fn check_free(path: &Path) -> Result<(), RecordError> {
    check_free_with(free_bytes, path)
}

/// [`check_free`] with the measurement swapped out (a test seam).
pub(crate) fn check_free_with(
    free_space: fn(&Path) -> Option<u64>,
    path: &Path,
) -> Result<(), RecordError> {
    match free_space(path) {
        Some(free_bytes) if free_bytes < MIN_FREE_BYTES => Err(RecordError::DiskLow { free_bytes }),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disk_free_bytes_answers_for_a_real_folder_and_not_a_missing_one() {
        let here = std::env::temp_dir();
        if cfg!(unix) {
            assert!(free_bytes(&here).is_some_and(|b| b > 0));
        } else {
            assert_eq!(free_bytes(&here), None);
        }
        assert_eq!(free_bytes(&here.join("astar-disk-no-such-folder/x")), None);
    }

    #[test]
    fn disk_check_refuses_below_500_mb() {
        assert_eq!(MIN_FREE_BYTES, 500_000_000);
        let p = Path::new("/");
        assert!(check_free_with(|_| Some(MIN_FREE_BYTES), p).is_ok());
        assert!(check_free_with(|_| None, p).is_ok());
        let e = check_free_with(|_| Some(MIN_FREE_BYTES - 1), p).unwrap_err();
        assert!(
            matches!(e, RecordError::DiskLow { free_bytes } if free_bytes == MIN_FREE_BYTES - 1)
        );
        assert_eq!(
            e.to_string(),
            "disk has only 499 MB free; recording needs 500 MB"
        );
    }
}
