//! Keeping the machine awake while a disc is written, and tray / medium-removal control through
//! the documented storage IOCTLs (no raw START STOP UNIT). Windows only; on other systems the
//! guards do nothing and the drive operations report "nicht unterstützt".

use super::BurnError;

/// `F:\` or `F:` → `F`.
pub fn drive_letter(path: &str) -> Option<char> {
    let mut chars = path.trim().chars();
    let letter = chars.next().filter(char::is_ascii_alphabetic)?;
    (chars.next() == Some(':')).then(|| letter.to_ascii_uppercase())
}

#[cfg(windows)]
pub use win::{DriveHandle, StandbyGuard, TrayLock};

#[cfg(not(windows))]
pub use other::{DriveHandle, StandbyGuard, TrayLock};

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::IO::DeviceIoControl;
    use windows::Win32::System::Ioctl::{
        FSCTL_DISMOUNT_VOLUME, FSCTL_LOCK_VOLUME, FSCTL_UNLOCK_VOLUME, IOCTL_STORAGE_EJECT_MEDIA, IOCTL_STORAGE_LOAD_MEDIA,
        IOCTL_STORAGE_MEDIA_REMOVAL, PREVENT_MEDIA_REMOVAL,
    };
    use windows::Win32::System::Power::{SetThreadExecutionState, ES_CONTINUOUS, ES_DISPLAY_REQUIRED, ES_SYSTEM_REQUIRED};

    use super::BurnError;

    /// No standby or display-off while it lives. The execution state belongs to the calling
    /// thread, so create and drop the guard on the thread that runs the operation.
    pub struct StandbyGuard(());

    impl StandbyGuard {
        pub fn new() -> Self {
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED | ES_DISPLAY_REQUIRED);
            }
            StandbyGuard(())
        }
    }

    impl Default for StandbyGuard {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for StandbyGuard {
        fn drop(&mut self) {
            unsafe {
                SetThreadExecutionState(ES_CONTINUOUS);
            }
        }
    }

    /// An open handle on `\\.\X:`.
    pub struct DriveHandle {
        handle: HANDLE,
        letter: char,
    }

    impl DriveHandle {
        /// `write`: also request write access (needed to lock and dismount the volume).
        pub fn open(letter: char, write: bool) -> Result<Self, BurnError> {
            let path: Vec<u16> = format!(r"\\.\{}:", letter.to_ascii_uppercase()).encode_utf16().chain(Some(0)).collect();
            let access = GENERIC_READ.0 | if write { GENERIC_WRITE.0 } else { 0 };
            let handle = unsafe {
                CreateFileW(PCWSTR(path.as_ptr()), access, FILE_SHARE_READ | FILE_SHARE_WRITE, None, OPEN_EXISTING, FILE_ATTRIBUTE_NORMAL, None)
            }
            .map_err(|e| BurnError::Backend(format!("Drive {letter}: could not be opened: {e}")))?;
            Ok(DriveHandle { handle, letter })
        }

        fn ioctl(&self, code: u32, input: Option<&[u8]>) -> Result<(), BurnError> {
            let mut returned = 0u32;
            unsafe {
                DeviceIoControl(
                    self.handle,
                    code,
                    input.map(|i| i.as_ptr() as *const c_void),
                    input.map_or(0, |i| i.len() as u32),
                    None,
                    0,
                    Some(&mut returned),
                    None,
                )
            }
            .map_err(|e| BurnError::Backend(format!("Drive {}: {e}", self.letter)))
        }

        /// Locks the volume (fails while files are open) – an optional step before ejecting.
        pub fn lock_volume(&self) -> Result<(), BurnError> {
            self.ioctl(FSCTL_LOCK_VOLUME, None)
        }

        pub fn unlock_volume(&self) -> Result<(), BurnError> {
            self.ioctl(FSCTL_UNLOCK_VOLUME, None)
        }

        pub fn dismount_volume(&self) -> Result<(), BurnError> {
            self.ioctl(FSCTL_DISMOUNT_VOLUME, None)
        }

        /// Allows or prevents removal of the medium for as long as this handle stays open.
        pub fn prevent_removal(&self, prevent: bool) -> Result<(), BurnError> {
            let p = PREVENT_MEDIA_REMOVAL { PreventMediaRemoval: prevent };
            // SAFETY: plain one-byte struct.
            let bytes = unsafe { std::slice::from_raw_parts(&p as *const _ as *const u8, size_of::<PREVENT_MEDIA_REMOVAL>()) };
            self.ioctl(IOCTL_STORAGE_MEDIA_REMOVAL, Some(bytes))
        }

        /// Opens the tray / ejects the medium (locks and dismounts first, best effort).
        pub fn eject(&self) -> Result<(), BurnError> {
            let _ = self.lock_volume();
            let _ = self.dismount_volume();
            let _ = self.prevent_removal(false);
            self.ioctl(IOCTL_STORAGE_EJECT_MEDIA, None)
        }

        /// Closes the tray.
        pub fn load(&self) -> Result<(), BurnError> {
            self.ioctl(IOCTL_STORAGE_LOAD_MEDIA, None)
        }
    }

    impl Drop for DriveHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }

    /// The medium cannot be removed (eject button, software) while this lives: the lock is tied
    /// to the open handle, closing the handle releases it.
    pub struct TrayLock(DriveHandle);

    impl TrayLock {
        pub fn acquire(letter: char) -> Result<Self, BurnError> {
            let h = DriveHandle::open(letter, false)?;
            h.prevent_removal(true)?;
            Ok(TrayLock(h))
        }
    }

    impl Drop for TrayLock {
        fn drop(&mut self) {
            let _ = self.0.prevent_removal(false);
        }
    }
}

#[cfg(not(windows))]
mod other {
    use super::BurnError;

    pub struct StandbyGuard(());

    impl StandbyGuard {
        pub fn new() -> Self {
            StandbyGuard(())
        }
    }

    impl Default for StandbyGuard {
        fn default() -> Self {
            Self::new()
        }
    }

    fn unsupported<T>() -> Result<T, BurnError> {
        Err(BurnError::Unsupported("Drive control is only available on Windows".into()))
    }

    pub struct DriveHandle(());

    impl DriveHandle {
        pub fn open(_letter: char, _write: bool) -> Result<Self, BurnError> {
            unsupported()
        }
        pub fn eject(&self) -> Result<(), BurnError> {
            unsupported()
        }
        pub fn load(&self) -> Result<(), BurnError> {
            unsupported()
        }
        pub fn prevent_removal(&self, _prevent: bool) -> Result<(), BurnError> {
            unsupported()
        }
    }

    pub struct TrayLock(());

    impl TrayLock {
        pub fn acquire(_letter: char) -> Result<Self, BurnError> {
            unsupported()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_letters() {
        assert_eq!(drive_letter(r"F:\"), Some('F'));
        assert_eq!(drive_letter("f:"), Some('F'));
        assert_eq!(drive_letter(" d:\\ "), Some('D'));
        assert_eq!(drive_letter(""), None);
        assert_eq!(drive_letter("/dev/sr0"), None);
        assert_eq!(drive_letter("1:\\"), None);
        assert_eq!(drive_letter("FF:"), None);
    }

    #[test]
    fn standby_guard_nests() {
        let a = StandbyGuard::new();
        let b = StandbyGuard::default();
        drop(b);
        drop(a);
    }
}
