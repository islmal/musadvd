//! MMC (T10 MMC-5 / SPC) command layer: CDB builders and response parsers (pure, unit-tested
//! byte by byte), a [`ScsiTransport`] with two Windows implementations (IMAPI `IDiscRecorder2Ex`,
//! the default and free of admin rights; SPTI `IOCTL_SCSI_PASS_THROUGH_DIRECT`), and the
//! read-only [`drive_info`] probe. Nothing here writes to a disc.

use serde::Serialize;

use super::BurnError;

// ---------------------------------------------------------------------------------------------
// CDB builders. All multi-byte fields are big-endian.

fn cdb<const N: usize>(op: u8) -> [u8; N] {
    let mut c = [0u8; N];
    c[0] = op;
    c
}

pub fn test_unit_ready() -> [u8; 6] {
    cdb(0x00)
}

pub fn inquiry(alloc: u8) -> [u8; 6] {
    let mut c = cdb::<6>(0x12);
    c[4] = alloc;
    c
}

/// FORMAT UNIT with format data (`0x11`: FmtData + format code 1); the parameter list is
/// [`format_list`].
pub fn format_unit() -> [u8; 6] {
    let mut c = cdb::<6>(0x04);
    c[1] = 0x11;
    c
}

/// FORMAT UNIT parameter list: 4-byte header + one 8-byte descriptor (`blocks`, format type).
pub fn format_list(format_type: u8, blocks: u32) -> [u8; 12] {
    let mut l = [0u8; 12];
    l[1] = 0x02; // immediate = 0, descriptor length 8
    l[3] = 8;
    l[4..8].copy_from_slice(&blocks.to_be_bytes());
    l[8] = format_type << 2;
    l
}

/// START STOP UNIT: `eject` = LoEj without Start (0x02), load = LoEj + Start (0x03).
pub fn start_stop_unit(eject: bool) -> [u8; 6] {
    let mut c = cdb::<6>(0x1B);
    c[4] = if eject { 0x02 } else { 0x03 };
    c
}

pub fn prevent_allow_medium_removal(prevent: bool) -> [u8; 6] {
    let mut c = cdb::<6>(0x1E);
    c[4] = prevent as u8;
    c
}

pub fn read_capacity() -> [u8; 10] {
    cdb(0x25)
}

pub fn read_format_capacities(alloc: u16) -> [u8; 10] {
    let mut c = cdb::<10>(0x23);
    c[7..9].copy_from_slice(&alloc.to_be_bytes());
    c
}

pub fn read10(lba: u32, blocks: u16) -> [u8; 10] {
    let mut c = cdb::<10>(0x28);
    c[2..6].copy_from_slice(&lba.to_be_bytes());
    c[7..9].copy_from_slice(&blocks.to_be_bytes());
    c
}

pub fn read12(lba: u32, blocks: u32) -> [u8; 12] {
    let mut c = cdb::<12>(0xA8);
    c[2..6].copy_from_slice(&lba.to_be_bytes());
    c[6..10].copy_from_slice(&blocks.to_be_bytes());
    c
}

pub fn write10(lba: u32, blocks: u16) -> [u8; 10] {
    let mut c = cdb::<10>(0x2A);
    c[2..6].copy_from_slice(&lba.to_be_bytes());
    c[7..9].copy_from_slice(&blocks.to_be_bytes());
    c
}

pub fn synchronize_cache(immediate: bool) -> [u8; 10] {
    let mut c = cdb::<10>(0x35);
    c[1] = (immediate as u8) << 1;
    c
}

/// GET CONFIGURATION; `rt`: 0 all features, 1 current, 2 one feature.
pub fn get_configuration(rt: u8, start_feature: u16, alloc: u16) -> [u8; 10] {
    let mut c = cdb::<10>(0x46);
    c[1] = rt & 3;
    c[2..4].copy_from_slice(&start_feature.to_be_bytes());
    c[7..9].copy_from_slice(&alloc.to_be_bytes());
    c
}

/// READ DISC INFORMATION, data type 0 (standard disc information).
pub fn read_disc_information(alloc: u16) -> [u8; 10] {
    let mut c = cdb::<10>(0x51);
    c[7..9].copy_from_slice(&alloc.to_be_bytes());
    c
}

/// READ TRACK INFORMATION; `address_type`: 0 LBA, 1 track number, 2 session number.
pub fn read_track_information(address_type: u8, address: u32, alloc: u16) -> [u8; 10] {
    let mut c = cdb::<10>(0x52);
    c[1] = address_type & 3;
    c[2..6].copy_from_slice(&address.to_be_bytes());
    c[7..9].copy_from_slice(&alloc.to_be_bytes());
    c
}

pub fn reserve_track(blocks: u32) -> [u8; 10] {
    let mut c = cdb::<10>(0x53);
    c[5..9].copy_from_slice(&blocks.to_be_bytes());
    c
}

pub fn send_opc_information(do_opc: bool) -> [u8; 10] {
    let mut c = cdb::<10>(0x54);
    c[1] = do_opc as u8;
    c
}

/// MODE SELECT(10) with the page-format bit.
pub fn mode_select10(len: u16) -> [u8; 10] {
    let mut c = cdb::<10>(0x55);
    c[1] = 0x10;
    c[7..9].copy_from_slice(&len.to_be_bytes());
    c
}

/// MODE SENSE(10) of `page` (0x05 write parameters, 0x01 error recovery, 0x2A capabilities),
/// current values.
pub fn mode_sense10(page: u8, alloc: u16) -> [u8; 10] {
    let mut c = cdb::<10>(0x5A);
    c[2] = page & 0x3F;
    c[7..9].copy_from_slice(&alloc.to_be_bytes());
    c
}

/// CLOSE TRACK/SESSION; `function`: 1 track, 2 session (finalise).
pub fn close_track_session(function: u8, track: u16, immediate: bool) -> [u8; 10] {
    let mut c = cdb::<10>(0x5B);
    c[1] = immediate as u8;
    c[2] = function;
    c[4..6].copy_from_slice(&track.to_be_bytes());
    c
}

pub fn read_buffer_capacity(alloc: u16) -> [u8; 10] {
    let mut c = cdb::<10>(0x5C);
    c[7..9].copy_from_slice(&alloc.to_be_bytes());
    c
}

/// GET PERFORMANCE, type 3 (write speed descriptors), "write" data set.
pub fn get_performance_write_speeds(max_descriptors: u16) -> [u8; 12] {
    let mut c = cdb::<12>(0xAC);
    c[1] = 0x04;
    c[8..10].copy_from_slice(&max_descriptors.to_be_bytes());
    c[10] = 0x03;
    c
}

/// READ DISC STRUCTURE; `bd`: media type 1 (BD) instead of 0 (DVD).
pub fn read_disc_structure(bd: bool, address: u32, layer: u8, format: u8, alloc: u16) -> [u8; 12] {
    let mut c = cdb::<12>(0xAD);
    c[1] = bd as u8;
    c[2..6].copy_from_slice(&address.to_be_bytes());
    c[6] = layer;
    c[7] = format;
    c[8..10].copy_from_slice(&alloc.to_be_bytes());
    c
}

/// SET STREAMING performance descriptor (type 0): the parameter list of 28 bytes limiting the
/// write speed to `kbytes_per_sec` over the whole medium (`last_lba`), one second granularity.
pub fn set_streaming() -> [u8; 12] {
    let mut c = cdb::<12>(0xB6);
    c[8] = 0; // type: performance descriptor
    c[9..11].copy_from_slice(&28u16.to_be_bytes());
    c
}

pub fn streaming_descriptor(last_lba: u32, read_kbps: u32, write_kbps: u32) -> [u8; 28] {
    let mut d = [0u8; 28];
    d[0] = 0x00; // WRC = 0, RDD = 0, Exact = 0, RA = 0
    d[8..12].copy_from_slice(&last_lba.to_be_bytes());
    d[12..16].copy_from_slice(&read_kbps.to_be_bytes());
    d[16..20].copy_from_slice(&1000u32.to_be_bytes()); // read time: 1 s
    d[20..24].copy_from_slice(&write_kbps.to_be_bytes());
    d[24..28].copy_from_slice(&1000u32.to_be_bytes()); // write time: 1 s
    d
}

/// SEND DISC STRUCTURE (layer break and friends: format 0x20–0x23).
pub fn send_disc_structure(format: u8, len: u16) -> [u8; 12] {
    let mut c = cdb::<12>(0xBF);
    c[7] = format;
    c[8..10].copy_from_slice(&len.to_be_bytes());
    c
}

// ---------------------------------------------------------------------------------------------
// Sense data.

/// Decoded sense data (fixed format 0x70/0x71 or descriptor format 0x72/0x73).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Sense {
    pub key: u8,
    pub asc: u8,
    pub ascq: u8,
}

impl Sense {
    /// `None` for an all-zero buffer or a response code that is not sense data.
    pub fn parse(raw: &[u8]) -> Option<Sense> {
        let code = *raw.first()? & 0x7F;
        match code {
            0x70 | 0x71 if raw.len() >= 14 => Some(Sense { key: raw[2] & 0x0F, asc: raw[12], ascq: raw[13] }),
            0x72 | 0x73 if raw.len() >= 4 => Some(Sense { key: raw[1] & 0x0F, asc: raw[2], ascq: raw[3] }),
            _ => None,
        }
    }

    /// Hardware-side trouble worth a retry (not ready, becoming ready, unit attention).
    pub fn recoverable(&self) -> bool {
        matches!(self.key, 0x02 | 0x06) && !matches!((self.asc, self.ascq), (0x3A, _))
    }

    /// German description of the common conditions.
    pub fn describe(&self) -> String {
        let text = match (self.key, self.asc, self.ascq) {
            (_, 0x3A, _) => "no disc in the drive",
            (2, 0x04, 0x01) => "Drive becoming ready (reading disc)",
            (2, 0x04, 0x04) => "Disc is being formatted",
            (2, 0x04, _) => "Drive not ready",
            (_, 0x30, 0x00..=0x07) => "incompatible or unknown disc",
            (_, 0x57, _) => "Disc table of contents unreadable",
            (_, 0x20, 0x00) => "invalid command",
            (_, 0x21, 0x00) => "address outside the disc",
            (_, 0x24, 0x00) => "invalid field in command",
            (_, 0x26, 0x00) => "invalid field in parameter list",
            (_, 0x28, 0x00) => "Disc changed",
            (_, 0x29, _) => "Drive was reset",
            (3, 0x0C, _) => "Write error",
            (3, 0x11, _) => "uncorrectable read error",
            (_, 0x64, 0x00) => "illegal mode for this track",
            (_, 0x72, _) => "Session error (disc full or not writable)",
            (_, 0x73, 0x03) => "Write laser calibration (OPC) failed",
            (_, 0x73, _) => "Drive write error",
            (5, _, _) => "illegal request",
            (7, _, _) => "Disc is write protected",
            _ => "Drive error",
        };
        format!("{text} (Sense {:X}/{:02X}/{:02X})", self.key, self.asc, self.ascq)
    }
}

// ---------------------------------------------------------------------------------------------
// Transport.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataDir {
    None,
    FromDevice,
    ToDevice,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScsiResult {
    /// SCSI status byte (0 good, 2 check condition).
    pub status: u8,
    pub sense: Option<Sense>,
    /// Bytes transferred (from-device commands).
    pub transferred: usize,
}

impl ScsiResult {
    pub const GOOD: ScsiResult = ScsiResult { status: 0, sense: None, transferred: 0 };

    /// The command succeeded (no check condition).
    pub fn ok(&self) -> bool {
        self.status == 0 && self.sense.map_or(true, |s| s.key == 0 || s.key == 1)
    }

    pub fn into_result(self, what: &str) -> Result<ScsiResult, BurnError> {
        if self.ok() {
            return Ok(self);
        }
        let why = self.sense.map(|s| s.describe()).unwrap_or_else(|| format!("SCSI-Status {:#04X}", self.status));
        Err(BurnError::Backend(format!("{what}: {why}")))
    }
}

/// Sends one CDB to the drive. `buf` is the data to send for [`DataDir::ToDevice`] and the receive
/// buffer for [`DataDir::FromDevice`]. A check condition is a successful call with `sense` set;
/// `Err` means the command could not be delivered at all.
pub trait ScsiTransport {
    fn exec(&self, cdb: &[u8], dir: DataDir, buf: &mut [u8], timeout_s: u32) -> Result<ScsiResult, BurnError>;
}

/// Convenience: command without data / with a receive buffer; fails on a check condition.
pub fn run(t: &dyn ScsiTransport, what: &str, cdb: &[u8]) -> Result<(), BurnError> {
    t.exec(cdb, DataDir::None, &mut [], 30)?.into_result(what).map(|_| ())
}

/// Receives up to `alloc` bytes; the vector is cut to what the drive returned.
pub fn query(t: &dyn ScsiTransport, what: &str, cdb: &[u8], alloc: usize) -> Result<Vec<u8>, BurnError> {
    let mut buf = vec![0u8; alloc];
    let r = t.exec(cdb, DataDir::FromDevice, &mut buf, 30)?.into_result(what)?;
    buf.truncate(if r.transferred == 0 { alloc } else { r.transferred.min(alloc) });
    Ok(buf)
}

#[cfg(windows)]
pub use win::{ImapiExTransport, SptiTransport};

#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    use windows::core::{Interface, BSTR, PCWSTR};
    use windows::Win32::Foundation::{CloseHandle, HANDLE, GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::Storage::Imapi::{IDiscRecorder2, IDiscRecorder2Ex};
    use windows::Win32::Storage::IscsiDisc::{IOCTL_SCSI_PASS_THROUGH_DIRECT, SCSI_PASS_THROUGH_DIRECT};
    use windows::Win32::System::IO::DeviceIoControl;

    use super::super::windows::{backend, recorder, Com};
    use super::{BurnError, DataDir, ScsiResult, ScsiTransport, Sense};

    fn result(sense: [u8; 18], status: u8, transferred: usize) -> ScsiResult {
        ScsiResult { status, sense: Sense::parse(&sense).filter(|s| s.key != 0), transferred }
    }

    /// Commands through IMAPI's `IDiscRecorder2Ex` (works without administrator rights).
    pub struct ImapiExTransport {
        ex: IDiscRecorder2Ex,
        rec: IDiscRecorder2,
        _com: Com,
    }

    impl ImapiExTransport {
        pub fn open(drive_id: &str) -> Result<Self, BurnError> {
            let com = Com::init()?;
            let rec = recorder(drive_id)?;
            let ex: IDiscRecorder2Ex = rec.cast().map_err(backend)?;
            Ok(ImapiExTransport { ex, rec, _com: com })
        }

        /// Exclusive access (nobody else, including the file system driver, talks to the drive).
        pub fn acquire_exclusive(&self, client: &str) -> Result<(), BurnError> {
            unsafe {
                self.rec
                    .AcquireExclusiveAccess(windows::Win32::Foundation::VARIANT_FALSE, &BSTR::from(client))
                    .map_err(backend)
            }
        }

        pub fn release_exclusive(&self) {
            unsafe {
                let _ = self.rec.ReleaseExclusiveAccess();
            }
        }
    }

    impl ScsiTransport for ImapiExTransport {
        fn exec(&self, cdb: &[u8], dir: DataDir, buf: &mut [u8], timeout_s: u32) -> Result<ScsiResult, BurnError> {
            let mut sense = [0u8; 18];
            let mut fetched = 0u32;
            let r = unsafe {
                match dir {
                    DataDir::None => self.ex.SendCommandNoData(cdb, &mut sense, timeout_s),
                    DataDir::ToDevice => self.ex.SendCommandSendDataToDevice(cdb, &mut sense, timeout_s, buf),
                    DataDir::FromDevice => self.ex.SendCommandGetDataFromDevice(cdb, &mut sense, timeout_s, buf, &mut fetched),
                }
            };
            match r {
                Ok(()) => Ok(result(sense, 0, if dir == DataDir::FromDevice { fetched as usize } else { buf.len() })),
                // A check condition comes back as a failed HRESULT with the sense buffer filled.
                Err(_) if Sense::parse(&sense).is_some_and(|s| s.key != 0) => Ok(result(sense, 2, 0)),
                Err(e) => Err(backend(e)),
            }
        }
    }

    #[repr(C)]
    struct PassThrough {
        spt: SCSI_PASS_THROUGH_DIRECT,
        filler: u32,
        sense: [u8; 32],
    }

    /// `IOCTL_SCSI_PASS_THROUGH_DIRECT` on `\\.\X:`. Needs write access to the drive, which
    /// Windows grants to administrators only (CD-ROM class devices).
    pub struct SptiTransport {
        handle: HANDLE,
    }

    impl SptiTransport {
        /// `letter`: the drive letter, e.g. `F`.
        pub fn open(letter: char) -> Result<Self, BurnError> {
            let path: Vec<u16> = format!(r"\\.\{}:", letter.to_ascii_uppercase()).encode_utf16().chain(Some(0)).collect();
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(path.as_ptr()),
                    GENERIC_READ.0 | GENERIC_WRITE.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                )
            }
            .map_err(|e| {
                BurnError::Backend(if e.code().0 as u32 == 0x80070005 {
                    "SPTI needs administrator rights".to_string()
                } else {
                    format!("Drive {letter}: could not be opened: {e}")
                })
            })?;
            Ok(SptiTransport { handle })
        }
    }

    impl Drop for SptiTransport {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }

    impl ScsiTransport for SptiTransport {
        fn exec(&self, cdb: &[u8], dir: DataDir, buf: &mut [u8], timeout_s: u32) -> Result<ScsiResult, BurnError> {
            assert!(cdb.len() <= 16);
            // The driver wants the data buffer aligned to the adapter's mask; page alignment
            // satisfies every mask in practice.
            let mut backing = vec![0u8; buf.len() + 4096];
            let off = backing.as_ptr().align_offset(4096);
            let data = &mut backing[off..off + buf.len()];
            if dir == DataDir::ToDevice {
                data.copy_from_slice(buf);
            }
            let mut p: PassThrough = unsafe { std::mem::zeroed() };
            p.spt.Length = size_of::<SCSI_PASS_THROUGH_DIRECT>() as u16;
            p.spt.CdbLength = cdb.len() as u8;
            p.spt.SenseInfoLength = p.sense.len() as u8;
            p.spt.DataIn = match dir {
                DataDir::None => 2,       // SCSI_IOCTL_DATA_UNSPECIFIED
                DataDir::FromDevice => 1, // SCSI_IOCTL_DATA_IN
                DataDir::ToDevice => 0,   // SCSI_IOCTL_DATA_OUT
            };
            p.spt.DataTransferLength = buf.len() as u32;
            p.spt.TimeOutValue = timeout_s;
            p.spt.DataBuffer = if buf.is_empty() { std::ptr::null_mut() } else { data.as_mut_ptr() as *mut c_void };
            p.spt.SenseInfoOffset = std::mem::offset_of!(PassThrough, sense) as u32;
            p.spt.Cdb[..cdb.len()].copy_from_slice(cdb);
            let mut returned = 0u32;
            let size = size_of::<PassThrough>() as u32;
            unsafe {
                DeviceIoControl(
                    self.handle,
                    IOCTL_SCSI_PASS_THROUGH_DIRECT,
                    Some(&p as *const _ as *const c_void),
                    size,
                    Some(&mut p as *mut _ as *mut c_void),
                    size,
                    Some(&mut returned),
                    None,
                )
            }
            .map_err(|e| BurnError::Backend(format!("SPTI: {e}")))?;
            if dir == DataDir::FromDevice {
                buf.copy_from_slice(data);
            }
            let sense = Sense::parse(&p.sense).filter(|s| s.key != 0);
            Ok(ScsiResult { status: p.spt.ScsiStatus, sense, transferred: p.spt.DataTransferLength as usize })
        }
    }
}

#[cfg(target_os = "linux")]
pub use linux::SgIoTransport;

/// Maps a finished `SG_IO` header to a result. A failed delivery (host adapter error, driver
/// timeout) is `Err`, like a failed `DeviceIoControl`; a check condition is `Ok` with sense data.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn sg_result(status: u8, host_status: u16, driver_status: u16, sense: &[u8], dxfer_len: u32, resid: i32) -> Result<ScsiResult, BurnError> {
    const DID_TIME_OUT: u16 = 0x03;
    const DRIVER_TIMEOUT: u16 = 0x06;
    if host_status == DID_TIME_OUT || driver_status & 0x0F == DRIVER_TIMEOUT {
        return Err(BurnError::Backend("SG_IO: the drive did not answer in time".into()));
    }
    if host_status != 0 {
        return Err(BurnError::Backend(format!("SG_IO: host adapter error {host_status:#04X}")));
    }
    let sense = Sense::parse(sense).filter(|s| s.key != 0);
    let transferred = (dxfer_len as i64 - resid.max(0) as i64).max(0) as usize;
    Ok(ScsiResult { status, sense, transferred })
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{File, OpenOptions};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;

    use super::{sg_result, BurnError, DataDir, ScsiResult, ScsiTransport};

    /// `struct sg_io_hdr` from `<scsi/sg.h>`.
    #[repr(C)]
    struct SgIoHdr {
        interface_id: libc::c_int,
        dxfer_direction: libc::c_int,
        cmd_len: u8,
        mx_sb_len: u8,
        iovec_count: u16,
        dxfer_len: u32,
        dxferp: *mut libc::c_void,
        cmdp: *const u8,
        sbp: *mut u8,
        timeout: u32,
        flags: u32,
        pack_id: libc::c_int,
        usr_ptr: *mut libc::c_void,
        status: u8,
        masked_status: u8,
        msg_status: u8,
        sb_len_wr: u8,
        host_status: u16,
        driver_status: u16,
        resid: libc::c_int,
        duration: u32,
        info: u32,
    }

    const SG_IO: libc::c_ulong = 0x2285;

    /// `SG_IO` on `/dev/srN` (or `/dev/sgN`). Needs read/write access to the node, which the
    /// `cdrom` / `optical` group grants on most distributions.
    pub struct SgIoTransport {
        file: File,
    }

    impl SgIoTransport {
        pub fn open(device: &str) -> Result<Self, BurnError> {
            // O_NONBLOCK: the sr driver refuses a plain open while the tray is empty or open.
            let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(device).map_err(|e| {
                BurnError::Backend(if e.kind() == std::io::ErrorKind::PermissionDenied {
                    format!("{device}: permission denied (add the user to the 'cdrom' or 'optical' group)")
                } else {
                    format!("{device} could not be opened: {e}")
                })
            })?;
            Ok(SgIoTransport { file })
        }
    }

    impl ScsiTransport for SgIoTransport {
        fn exec(&self, cdb: &[u8], dir: DataDir, buf: &mut [u8], timeout_s: u32) -> Result<ScsiResult, BurnError> {
            assert!(cdb.len() <= 16);
            let mut sense = [0u8; 32];
            let mut h = SgIoHdr {
                interface_id: b'S' as libc::c_int,
                dxfer_direction: match dir {
                    DataDir::None => -1,       // SG_DXFER_NONE
                    DataDir::ToDevice => -2,   // SG_DXFER_TO_DEV
                    DataDir::FromDevice => -3, // SG_DXFER_FROM_DEV
                },
                cmd_len: cdb.len() as u8,
                mx_sb_len: sense.len() as u8,
                iovec_count: 0,
                dxfer_len: if dir == DataDir::None { 0 } else { buf.len() as u32 },
                dxferp: if dir == DataDir::None || buf.is_empty() { std::ptr::null_mut() } else { buf.as_mut_ptr().cast() },
                cmdp: cdb.as_ptr(),
                sbp: sense.as_mut_ptr(),
                timeout: timeout_s.saturating_mul(1000),
                flags: 0,
                pack_id: 0,
                usr_ptr: std::ptr::null_mut(),
                status: 0,
                masked_status: 0,
                msg_status: 0,
                sb_len_wr: 0,
                host_status: 0,
                driver_status: 0,
                resid: 0,
                duration: 0,
                info: 0,
            };
            // SAFETY: every pointer in `h` refers to a live buffer of the stated length for the
            // duration of the (synchronous) call.
            if unsafe { libc::ioctl(self.file.as_raw_fd(), SG_IO as _, &mut h as *mut SgIoHdr) } < 0 {
                return Err(BurnError::Backend(format!("SG_IO: {}", std::io::Error::last_os_error())));
            }
            sg_result(h.status, h.host_status, h.driver_status, &sense[..h.sb_len_wr as usize], h.dxfer_len, h.resid)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Response parsers.

fn be16(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn be32(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string()
}

/// INQUIRY data: (vendor, product, firmware revision).
pub fn parse_inquiry(b: &[u8]) -> Option<(String, String, String)> {
    (b.len() >= 36).then(|| (text(&b[8..16]), text(&b[16..32]), text(&b[32..36])))
}

/// READ CAPACITY: (last LBA, block length).
pub fn parse_read_capacity(b: &[u8]) -> Option<(u32, u32)> {
    Some((be32(b, 0)?, be32(b, 4)?))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Profile {
    pub code: u16,
    pub name: &'static str,
    pub current: bool,
}

/// MMC profile names (MMC-5 table "Profile List").
pub fn profile_name(code: u16) -> &'static str {
    match code {
        0x0000 => "no disc",
        0x0001 => "hard disk",
        0x0002 => "removable medium",
        0x0008 => "CD-ROM",
        0x0009 => "CD-R",
        0x000A => "CD-RW",
        0x0010 => "DVD-ROM",
        0x0011 => "DVD-R",
        0x0012 => "DVD-RAM",
        0x0013 => "DVD-RW (Restricted Overwrite)",
        0x0014 => "DVD-RW",
        0x0015 => "DVD-R DL",
        0x0016 => "DVD-R DL (Layer Jump)",
        0x001A => "DVD+RW",
        0x001B => "DVD+R",
        0x002A => "DVD+RW DL",
        0x002B => "DVD+R DL",
        0x0040 => "BD-ROM",
        0x0041 => "BD-R (SRM)",
        0x0042 => "BD-R (RRM)",
        0x0043 => "BD-RE",
        0x0050 => "HD DVD-ROM",
        0x0051 => "HD DVD-R",
        0x0052 => "HD DVD-RAM",
        _ => "unknown",
    }
}

/// GET CONFIGURATION (all features): the current profile and the Profile List feature (0x0000).
pub fn parse_configuration(b: &[u8]) -> Option<(u16, Vec<Profile>)> {
    let current = be16(b, 6)?;
    let total = (be32(b, 0)? as usize + 4).min(b.len());
    let mut profiles = Vec::new();
    let mut at = 8;
    while at + 4 <= total {
        let code = be16(b, at)?;
        let len = *b.get(at + 3)? as usize;
        if code == 0 {
            for p in b.get(at + 4..(at + 4 + len).min(total))?.chunks_exact(4) {
                let c = u16::from_be_bytes([p[0], p[1]]);
                profiles.push(Profile { code: c, name: profile_name(c), current: p[2] & 1 != 0 || c == current });
            }
        }
        at += 4 + len;
    }
    Some((current, profiles))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct DiscInformation {
    /// 0 empty, 1 incomplete (appendable), 2 finalised, 3 other.
    pub status: u8,
    /// State of the last session: 0 empty, 1 incomplete, 3 complete.
    pub last_session: u8,
    pub erasable: bool,
}

impl DiscInformation {
    pub fn status_text(&self) -> &'static str {
        match self.status {
            0 => "blank",
            1 => "incomplete (appendable)",
            2 => "finalized",
            _ => "other state",
        }
    }
}

pub fn parse_disc_information(b: &[u8]) -> Option<DiscInformation> {
    let b2 = *b.get(2)?;
    Some(DiscInformation { status: b2 & 3, last_session: (b2 >> 2) & 3, erasable: b2 & 0x10 != 0 })
}

/// Book type nibble of the DVD physical format information (READ DISC STRUCTURE 0x00).
pub fn book_type_name(nibble: u8) -> &'static str {
    match nibble {
        0x0 => "DVD-ROM",
        0x1 => "DVD-RAM",
        0x2 => "DVD-R",
        0x3 => "DVD-RW",
        0x9 => "DVD+RW",
        0xA => "DVD+R",
        0xD => "DVD+RW DL",
        0xE => "DVD+R DL",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DvdPhysical {
    /// High nibble of byte 0 after the 4-byte header.
    pub book_type: u8,
    pub part_version: u8,
    pub layers: u8,
    /// Opposite track path (layer 1 runs backwards).
    pub opposite_track_path: bool,
    pub start_psn: u32,
    pub end_psn: u32,
    pub end_l0_psn: u32,
    /// Sectors of layer 0 when the disc is dual layer and the break is known (DVD+R DL after
    /// the layer break was set, DVD-R DL pre-formatted).
    pub layer_break: Option<u32>,
}

/// READ DISC STRUCTURE format 0x00 on DVD media (physical format information).
pub fn parse_dvd_physical(b: &[u8]) -> Option<DvdPhysical> {
    let d = b.get(4..)?;
    if d.len() < 16 {
        return None;
    }
    let psn = |at: usize| u32::from_be_bytes([0, d[at], d[at + 1], d[at + 2]]);
    let layers = ((d[2] >> 5) & 3) + 1;
    let (start, end, end_l0) = (psn(5), psn(9), psn(13));
    Some(DvdPhysical {
        book_type: d[0] >> 4,
        part_version: d[0] & 0x0F,
        layers,
        opposite_track_path: d[2] & 0x10 != 0,
        start_psn: start,
        end_psn: end,
        end_l0_psn: end_l0,
        layer_break: (layers == 2 && end_l0 > start).then(|| end_l0 - start + 1),
    })
}

/// "CMC MAG-M01-00": manufacturer, media type ID and revision of DVD+R/RW from the ADIP
/// (READ DISC STRUCTURE 0x11; offsets 19 / 27 / 30 after the header as dvd+rw-tools reads them).
/// shortcut: DVD-R/RW (format 0x0E) and the revision's display form are unverified on real media.
pub fn parse_adip_media_id(b: &[u8]) -> Option<String> {
    let d = b.get(4..)?;
    let mfg = text(d.get(19..27)?);
    let kind = text(d.get(27..30)?);
    let rev = *d.get(30)?;
    (!mfg.is_empty()).then(|| format!("{mfg}-{kind}-{rev:02X}"))
}

/// BD disc information (READ DISC STRUCTURE media type 1, format 0x00): the DI unit starts
/// with `DI`; type "BDR"/"BDW" at 8, manufacturer ID at 100 (8), media type ID at 108 (3),
/// product revision at 113. Unverified on real media.
pub fn parse_bd_media_id(b: &[u8]) -> Option<String> {
    let d = b.get(4..)?;
    if d.get(0..2)? != b"DI" {
        return None;
    }
    let mfg = text(d.get(100..108)?);
    let kind = text(d.get(108..111)?);
    let rev = *d.get(113)?;
    (!mfg.is_empty()).then(|| format!("{mfg}-{kind}-{rev:02X}"))
}

/// READ BUFFER CAPACITY (data mode): (buffer length, free bytes).
pub fn parse_buffer_capacity(b: &[u8]) -> Option<(u32, u32)> {
    Some((be32(b, 4)?, be32(b, 8)?))
}

/// GET PERFORMANCE type 3: write speeds in kB/s (descriptor bytes 12–15), fastest first.
pub fn parse_write_speeds(b: &[u8]) -> Vec<u32> {
    let total = be32(b, 0).map_or(0, |l| l as usize + 4).min(b.len());
    let mut speeds: Vec<u32> = (8..total.saturating_sub(15)).step_by(16).filter_map(|at| be32(b, at + 12)).filter(|&s| s > 0).collect();
    speeds.sort_unstable_by(|a, b| b.cmp(a));
    speeds.dedup();
    speeds
}

// ---------------------------------------------------------------------------------------------
// Drive information.

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct DriveDetails {
    pub vendor: String,
    pub model: String,
    pub firmware: String,
    pub profiles: Vec<Profile>,
    pub current_profile: Option<u16>,
    pub current_profile_name: String,
    pub ready: bool,
    /// kB/s as the drive reports them.
    pub write_speeds_kbps: Vec<u32>,
    /// Book type of the inserted DVD (`DVD+R DL`, …).
    pub book_type: Option<String>,
    pub media_id: Option<String>,
    /// Layer 0 sectors of a dual-layer DVD whose break is set.
    pub layer_break: Option<u32>,
    pub disc_status: Option<String>,
    pub erasable: Option<bool>,
    pub capacity_sectors: Option<u64>,
    /// What could not be read (command failed) – informational.
    pub notes: Vec<String>,
}

/// Read-only probe: INQUIRY, TEST UNIT READY, GET CONFIGURATION, READ CAPACITY, READ DISC
/// INFORMATION, READ DISC STRUCTURE (physical format, media ID), GET PERFORMANCE. Every failing
/// query only adds a note.
pub fn drive_info(t: &dyn ScsiTransport) -> DriveDetails {
    let mut d = DriveDetails::default();
    match query(t, "INQUIRY", &inquiry(36), 36).map(|b| parse_inquiry(&b)) {
        Ok(Some((v, m, f))) => (d.vendor, d.model, d.firmware) = (v, m, f),
        Ok(None) => d.notes.push("INQUIRY: answer too short".into()),
        Err(e) => d.notes.push(e.to_string()),
    }
    match query(t, "GET CONFIGURATION", &get_configuration(0, 0, 1024), 1024).map(|b| parse_configuration(&b)) {
        Ok(Some((cur, profiles))) => {
            d.current_profile = Some(cur);
            d.current_profile_name = profile_name(cur).to_string();
            d.profiles = profiles;
        }
        Ok(None) => d.notes.push("GET CONFIGURATION: answer too short".into()),
        Err(e) => d.notes.push(e.to_string()),
    }
    d.ready = t.exec(&test_unit_ready(), DataDir::None, &mut [], 10).map(|r| r.ok()).unwrap_or(false);
    let profile = d.current_profile.unwrap_or(0);
    if !d.ready || profile == 0 {
        return d; // no disc: nothing more to ask
    }
    if let Ok(Some((last, _))) = query(t, "READ CAPACITY", &read_capacity(), 8).map(|b| parse_read_capacity(&b)) {
        d.capacity_sectors = Some(last as u64 + 1);
    }
    if let Ok(Some(info)) = query(t, "READ DISC INFORMATION", &read_disc_information(34), 34).map(|b| parse_disc_information(&b)) {
        d.disc_status = Some(info.status_text().to_string());
        d.erasable = Some(info.erasable);
    }
    let bd = (0x40..=0x43).contains(&profile);
    let dvd = (0x10..=0x2B).contains(&profile);
    if dvd {
        match query(t, "READ DISC STRUCTURE", &read_disc_structure(false, 0, 0, 0x00, 2052 + 4), 2056).map(|b| parse_dvd_physical(&b)) {
            Ok(Some(p)) => {
                d.book_type = Some(format!("{} (part {})", book_type_name(p.book_type), p.part_version));
                d.layer_break = p.layer_break;
            }
            Ok(None) => d.notes.push("Physical format: answer too short".into()),
            Err(e) => d.notes.push(e.to_string()),
        }
        if matches!(profile, 0x1A | 0x1B | 0x2A | 0x2B) {
            d.media_id = query(t, "ADIP", &read_disc_structure(false, 0, 0, 0x11, 256 + 4), 260).ok().and_then(|b| parse_adip_media_id(&b));
        }
    } else if bd {
        d.media_id = query(t, "BD disc information", &read_disc_structure(true, 0, 0, 0x00, 4 + 112), 116).ok().and_then(|b| parse_bd_media_id(&b));
    }
    if let Ok(b) = query(t, "GET PERFORMANCE", &get_performance_write_speeds(32), 8 + 32 * 16) {
        d.write_speeds_kbps = parse_write_speeds(&b);
    }
    d
}

/// READ DISC STRUCTURE format 0x01 (Copyright Information): returns true when the disc is CSS/CPPM protected.
pub fn parse_copyright_info(b: &[u8]) -> Option<bool> {
    (b.len() >= 5).then(|| b[4] != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    #[test]
    fn cdbs_byte_by_byte() {
        assert_eq!(test_unit_ready(), [0; 6]);
        assert_eq!(inquiry(36), [0x12, 0, 0, 0, 36, 0]);
        assert_eq!(format_unit(), [0x04, 0x11, 0, 0, 0, 0]);
        assert_eq!(format_list(0x26, 0x1234), [0, 2, 0, 8, 0, 0, 0x12, 0x34, 0x26 << 2, 0, 0, 0]);
        assert_eq!(start_stop_unit(true), [0x1B, 0, 0, 0, 0x02, 0]);
        assert_eq!(start_stop_unit(false), [0x1B, 0, 0, 0, 0x03, 0]);
        assert_eq!(prevent_allow_medium_removal(true), [0x1E, 0, 0, 0, 1, 0]);
        assert_eq!(prevent_allow_medium_removal(false), [0x1E, 0, 0, 0, 0, 0]);
        assert_eq!(read_capacity(), [0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(read_format_capacities(0x0100), [0x23, 0, 0, 0, 0, 0, 0, 1, 0, 0]);
        assert_eq!(read10(0x01020304, 0x0506), [0x28, 0, 1, 2, 3, 4, 0, 5, 6, 0]);
        assert_eq!(read12(0x01020304, 0x05060708), [0xA8, 0, 1, 2, 3, 4, 5, 6, 7, 8, 0, 0]);
        assert_eq!(write10(0x01020304, 0x0506), [0x2A, 0, 1, 2, 3, 4, 0, 5, 6, 0]);
        assert_eq!(synchronize_cache(true), [0x35, 2, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(synchronize_cache(false), [0x35, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(get_configuration(1, 0x002B, 0x0200), [0x46, 1, 0, 0x2B, 0, 0, 0, 2, 0, 0]);
        assert_eq!(read_disc_information(34), [0x51, 0, 0, 0, 0, 0, 0, 0, 34, 0]);
        assert_eq!(read_track_information(1, 2, 48), [0x52, 1, 0, 0, 0, 2, 0, 0, 48, 0]);
        assert_eq!(reserve_track(0x01020304), [0x53, 0, 0, 0, 0, 1, 2, 3, 4, 0]);
        assert_eq!(send_opc_information(true), [0x54, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(mode_select10(0x32), [0x55, 0x10, 0, 0, 0, 0, 0, 0, 0x32, 0]);
        assert_eq!(mode_sense10(0x05, 0x40), [0x5A, 0, 5, 0, 0, 0, 0, 0, 0x40, 0]);
        assert_eq!(close_track_session(2, 0x0102, true), [0x5B, 1, 2, 0, 1, 2, 0, 0, 0, 0]);
        assert_eq!(read_buffer_capacity(12), [0x5C, 0, 0, 0, 0, 0, 0, 0, 12, 0]);
        assert_eq!(get_performance_write_speeds(32), [0xAC, 4, 0, 0, 0, 0, 0, 0, 0, 32, 3, 0]);
        assert_eq!(read_disc_structure(true, 0x01020304, 1, 0x11, 0x0204), [0xAD, 1, 1, 2, 3, 4, 1, 0x11, 2, 4, 0, 0]);
        assert_eq!(set_streaming(), [0xB6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 28, 0]);
        assert_eq!(send_disc_structure(0x20, 12), [0xBF, 0, 0, 0, 0, 0, 0, 0x20, 0, 12, 0, 0]);
    }

    #[test]
    fn streaming_descriptor_layout() {
        let d = streaming_descriptor(0x0022_0000, 8000, 11080);
        assert_eq!(&d[8..12], &[0, 0x22, 0, 0]);
        assert_eq!(u32::from_be_bytes(d[20..24].try_into().unwrap()), 11080);
        assert_eq!(u32::from_be_bytes(d[24..28].try_into().unwrap()), 1000);
        assert_eq!(d[0], 0);
    }

    #[test]
    fn sense_formats() {
        let mut fixed = [0u8; 18];
        fixed[0] = 0x70;
        fixed[2] = 0x02;
        fixed[12] = 0x3A;
        let s = Sense::parse(&fixed).unwrap();
        assert_eq!(s, Sense { key: 2, asc: 0x3A, ascq: 0 });
        assert!(s.describe().starts_with("no disc"), "{}", s.describe());
        assert!(!s.recoverable(), "no medium is not worth a retry");
        let desc = [0x72, 0x05, 0x24, 0x00, 0, 0, 0, 0];
        let s = Sense::parse(&desc).unwrap();
        assert_eq!((s.key, s.asc, s.ascq), (5, 0x24, 0));
        assert!(s.describe().contains("5/24/00"));
        assert!(Sense::parse(&[0u8; 18]).is_none());
        assert!(Sense::parse(&[0x70, 0, 0]).is_none(), "truncated");
        assert!(Sense { key: 2, asc: 4, ascq: 1 }.recoverable());
        assert!(Sense { key: 6, asc: 0x28, ascq: 0 }.recoverable());
        assert!(!Sense { key: 3, asc: 0x0C, ascq: 0 }.recoverable());
        assert!(Sense { key: 3, asc: 0x0C, ascq: 0 }.describe().starts_with("Write error"));
    }

    #[test]
    fn sg_io_header_results() {
        let mut fixed = [0u8; 18];
        fixed[0] = 0x70;
        fixed[2] = 0x02;
        fixed[12] = 0x3A;
        let r = sg_result(0, 0, 0, &[], 2048, 48).unwrap();
        assert_eq!((r.ok(), r.transferred), (true, 2000), "resid is subtracted");
        let r = sg_result(2, 0, 0x08, &fixed, 8, 8).unwrap();
        assert_eq!(r.sense, Some(Sense { key: 2, asc: 0x3A, ascq: 0 }), "check condition is a delivered answer");
        assert!(r.into_result("TUR").unwrap_err().to_string().contains("no disc"));
        assert!(sg_result(0, 0x03, 0, &[], 0, 0).is_err(), "DID_TIME_OUT");
        assert!(sg_result(0, 0, 0x06, &[], 0, 0).is_err(), "DRIVER_TIMEOUT");
        assert!(sg_result(0, 0x01, 0, &[], 0, 0).is_err(), "DID_NO_CONNECT");
        assert_eq!(sg_result(0, 0, 0, &[], 8, -4).unwrap().transferred, 8, "negative resid ignored");
    }

    #[test]
    fn result_checks() {
        assert!(ScsiResult::GOOD.ok());
        let bad = ScsiResult { status: 2, sense: Some(Sense { key: 5, asc: 0x20, ascq: 0 }), transferred: 0 };
        assert!(!bad.ok());
        let e = bad.into_result("TEST").unwrap_err().to_string();
        assert!(e.starts_with("TEST: invalid command"), "{e}");
    }

    #[test]
    fn inquiry_and_capacity() {
        let mut b = vec![0u8; 36];
        b[0] = 5;
        b[8..16].copy_from_slice(b"PIONEER ");
        b[16..32].copy_from_slice(b"BD-RW   BDR-209M");
        b[32..36].copy_from_slice(b"1.33");
        assert_eq!(parse_inquiry(&b), Some(("PIONEER".into(), "BD-RW   BDR-209M".into(), "1.33".into())));
        assert!(parse_inquiry(&b[..20]).is_none());
        assert_eq!(parse_read_capacity(&[0, 0x22, 0x3F, 0xFF, 0, 0, 8, 0]), Some((0x223FFF, 2048)));
    }

    /// GET CONFIGURATION answer: current profile DVD+R DL, profile list with 3 profiles, plus an
    /// unrelated feature after it.
    fn config_response() -> Vec<u8> {
        let mut b = vec![0u8; 8];
        b[6..8].copy_from_slice(&0x002Bu16.to_be_bytes());
        b.extend([0, 0, 3, 12]); // feature 0, version/current, 12 bytes of profiles
        b.extend([0x00, 0x1B, 0, 0, 0x00, 0x2B, 1, 0, 0x00, 0x43, 0, 0]);
        b.extend([0, 0x1D, 0, 4, 1, 2, 3, 4]); // feature 0x1D, skipped
        let len = (b.len() - 4) as u32;
        b[0..4].copy_from_slice(&len.to_be_bytes());
        b
    }

    #[test]
    fn configuration_profiles() {
        let (cur, profiles) = parse_configuration(&config_response()).unwrap();
        assert_eq!(cur, 0x2B);
        let names: Vec<_> = profiles.iter().map(|p| (p.name, p.current)).collect();
        assert_eq!(names, [("DVD+R", false), ("DVD+R DL", true), ("BD-RE", false)]);
        assert!(parse_configuration(&[0; 4]).is_none());
        assert_eq!(profile_name(0x41), "BD-R (SRM)");
        assert_eq!(profile_name(0x7777), "unknown");
    }

    #[test]
    fn disc_information() {
        let mut b = vec![0u8; 34];
        b[2] = 0b0001_0000 | 0b1100 | 0b01; // erasable, last session complete, incomplete disc
        let i = parse_disc_information(&b).unwrap();
        assert_eq!((i.status, i.last_session, i.erasable), (1, 3, true));
        assert_eq!(i.status_text(), "incomplete (appendable)");
        assert_eq!(parse_disc_information(&[0, 0, 0]).unwrap().status_text(), "blank");
    }

    fn dvd_physical(book: u8, layers: u8, start: u32, end: u32, end_l0: u32) -> Vec<u8> {
        let mut b = vec![0u8; 4 + 2048];
        b[4] = book << 4 | 1;
        b[5] = 0x0F;
        b[6] = layers << 5 | 1;
        let put = |b: &mut Vec<u8>, at: usize, v: u32| b[4 + at..4 + at + 3].copy_from_slice(&v.to_be_bytes()[1..]);
        put(&mut b, 5, start);
        put(&mut b, 9, end);
        put(&mut b, 13, end_l0);
        b
    }

    #[test]
    fn dvd_physical_format() {
        let p = parse_dvd_physical(&dvd_physical(0xE, 1, 0x30000, 0x30000 + 4_000_000, 0x30000 + 1_800_000 - 1)).unwrap();
        assert_eq!((p.book_type, p.part_version, p.layers), (0xE, 1, 2));
        assert_eq!(book_type_name(p.book_type), "DVD+R DL");
        assert_eq!(p.layer_break, Some(1_800_000));
        // Single layer: no layer break even when the field is filled.
        let p = parse_dvd_physical(&dvd_physical(0xA, 0, 0x30000, 0x30000 + 2_295_103, 0)).unwrap();
        assert_eq!((p.layers, p.layer_break), (1, None));
        assert_eq!(book_type_name(0xA), "DVD+R");
        assert_eq!(book_type_name(0x9), "DVD+RW");
        assert_eq!(book_type_name(0x0), "DVD-ROM");
        assert_eq!(book_type_name(0x2), "DVD-R");
        assert!(parse_dvd_physical(&[0; 10]).is_none());
    }

    #[test]
    fn media_ids() {
        let mut adip = vec![0u8; 4 + 256];
        adip[4 + 19..4 + 27].copy_from_slice(b"CMC MAG ");
        adip[4 + 27..4 + 30].copy_from_slice(b"M01");
        adip[4 + 30] = 0;
        assert_eq!(parse_adip_media_id(&adip).as_deref(), Some("CMC MAG-M01-00"));
        assert!(parse_adip_media_id(&[0u8; 8]).is_none());
        let mut bd = vec![0u8; 4 + 112];
        bd[4..6].copy_from_slice(b"DI");
        bd[4 + 100..4 + 108].copy_from_slice(b"VERBAT  ");
        bd[4 + 108..4 + 111].copy_from_slice(b"IMA");
        assert_eq!(parse_bd_media_id(&bd), None, "too short for the revision byte");
        bd.push(0);
        bd.push(1);
        assert_eq!(parse_bd_media_id(&bd).as_deref(), Some("VERBAT-IMA-01"));
        bd[4] = b'X';
        assert!(parse_bd_media_id(&bd).is_none(), "not a DI structure");
    }

    #[test]
    fn buffer_and_speeds() {
        assert_eq!(parse_buffer_capacity(&[0, 10, 0, 0, 0, 0x10, 0, 0, 0, 0x0F, 0, 0]), Some((0x100000, 0xF0000)));
        let mut b = vec![0u8; 8];
        for kbps in [5540u32, 11080, 11080, 22160, 0] {
            let mut d = [0u8; 16];
            d[12..16].copy_from_slice(&kbps.to_be_bytes());
            b.extend(d);
        }
        let len = (b.len() - 4) as u32;
        b[0..4].copy_from_slice(&len.to_be_bytes());
        assert_eq!(parse_write_speeds(&b), [22160, 11080, 5540], "fastest first, zero and duplicates dropped");
        assert!(parse_write_speeds(&[0; 4]).is_empty());
    }

    /// Scripted drive: answers by opcode (and READ DISC STRUCTURE by format), records the CDBs.
    struct Fake {
        profile: u16,
        sent: RefCell<Vec<Vec<u8>>>,
        ready: bool,
    }

    impl ScsiTransport for Fake {
        fn exec(&self, cdb: &[u8], _dir: DataDir, buf: &mut [u8], _t: u32) -> Result<ScsiResult, BurnError> {
            self.sent.borrow_mut().push(cdb.to_vec());
            let data: Vec<u8> = match cdb[0] {
                0x00 if !self.ready => {
                    return Ok(ScsiResult { status: 2, sense: Some(Sense { key: 2, asc: 0x3A, ascq: 0 }), transferred: 0 })
                }
                0x00 => vec![],
                0x12 => {
                    let mut b = vec![0u8; 36];
                    b[8..16].copy_from_slice(b"PIONEER ");
                    b[16..32].copy_from_slice(b"BD-RW   BDR-209M");
                    b[32..36].copy_from_slice(b"1.33");
                    b
                }
                0x46 => {
                    let mut b = config_response();
                    b[6..8].copy_from_slice(&self.profile.to_be_bytes());
                    b
                }
                0x25 => vec![0, 0x22, 0x3F, 0xFF, 0, 0, 8, 0],
                0x51 => vec![0, 32, 0b0000_0001, 1, 1, 1, 1],
                0xAD => match cdb[7] {
                    0x00 => dvd_physical(0xE, 1, 0x30000, 0x30000 + 4_000_000, 0x30000 + 1_800_000 - 1),
                    0x11 => {
                        let mut a = vec![0u8; 260];
                        a[23..31].copy_from_slice(b"RITEK   ");
                        a[31..34].copy_from_slice(b"S04");
                        a[34] = 0x66;
                        a
                    }
                    _ => return Ok(ScsiResult { status: 2, sense: Some(Sense { key: 5, asc: 0x24, ascq: 0 }), transferred: 0 }),
                },
                0xAC => {
                    let mut b = vec![0u8; 8];
                    let mut d = [0u8; 16];
                    d[12..16].copy_from_slice(&11080u32.to_be_bytes());
                    b.extend(d);
                    let len = (b.len() - 4) as u32;
                    b[0..4].copy_from_slice(&len.to_be_bytes());
                    b
                }
                op => panic!("unexpected opcode {op:#x}"),
            };
            let n = data.len().min(buf.len());
            buf[..n].copy_from_slice(&data[..n]);
            Ok(ScsiResult { status: 0, sense: None, transferred: n })
        }
    }

    #[test]
    fn drive_info_with_a_dvd_plus_r_dl() {
        let t = Fake { profile: 0x2B, sent: RefCell::new(vec![]), ready: true };
        let d = drive_info(&t);
        assert_eq!((d.vendor.as_str(), d.model.as_str(), d.firmware.as_str()), ("PIONEER", "BD-RW   BDR-209M", "1.33"));
        assert_eq!((d.current_profile, d.current_profile_name.as_str()), (Some(0x2B), "DVD+R DL"));
        assert_eq!(d.profiles.len(), 3);
        assert_eq!(d.capacity_sectors, Some(0x224000));
        assert_eq!(d.disc_status.as_deref(), Some("incomplete (appendable)"));
        assert_eq!(d.book_type.as_deref(), Some("DVD+R DL (part 1)"));
        assert_eq!(d.layer_break, Some(1_800_000));
        assert_eq!(d.media_id.as_deref(), Some("RITEK-S04-66"));
        assert_eq!(d.write_speeds_kbps, [11080]);
        assert!(d.notes.is_empty(), "{:?}", d.notes);
        // Only reading commands were sent.
        let ops: Vec<u8> = t.sent.borrow().iter().map(|c| c[0]).collect();
        assert!(ops.iter().all(|op| [0x00, 0x12, 0x25, 0x46, 0x51, 0xAC, 0xAD].contains(op)), "{ops:02X?}");
    }

    #[test]
    fn drive_info_without_a_disc_stops_early() {
        let t = Fake { profile: 0, sent: RefCell::new(vec![]), ready: false };
        let d = drive_info(&t);
        assert!(!d.ready);
        assert_eq!(d.current_profile_name, "no disc");
        assert!(d.capacity_sectors.is_none() && d.media_id.is_none());
        assert_eq!(t.sent.borrow().len(), 3, "INQUIRY, GET CONFIGURATION, TEST UNIT READY");
    }

    /// Read-only probe of the real drives (skipped without drives or with
    /// `MUSADVD_SKIP_DRIVE_TESTS`): INQUIRY / configuration / disc structures through IMAPI, and
    /// whether SPTI opens without elevation. Never writes.
    #[cfg(windows)]
    #[test]
    fn host_probe_reads_only() {
        use crate::burner::{windows::ImapiBurner, OpticalBurner};
        if std::env::var_os("MUSADVD_SKIP_DRIVE_TESTS").is_some() {
            return;
        }
        let drives = ImapiBurner.list_drives().unwrap_or_default();
        if drives.is_empty() {
            eprintln!("host probe: no drive");
        }
        for d in drives {
            let t = ImapiExTransport::open(&d.id).expect("IMAPI transport");
            let info = drive_info(&t);
            eprintln!("host probe {}: {info:#?}", d.name);
            assert!(!info.model.is_empty(), "INQUIRY answered: {:?}", info.notes);
            if let Some(letter) = crate::burner::power::drive_letter(&d.path) {
                match SptiTransport::open(letter) {
                    Ok(spti) => {
                        let again = drive_info(&spti);
                        eprintln!("SPTI works here (elevated?): {} {}", again.vendor, again.model);
                        assert_eq!(again.model, info.model);
                    }
                    Err(e) => eprintln!("SPTI not available: {e}"),
                }
            }
        }
    }

    #[test]
    fn parse_copyright_info_detects_protection() {
        assert_eq!(parse_copyright_info(&[]), None);
        assert_eq!(parse_copyright_info(&[0, 2, 0, 0]), None);
        // byte 4 = 0 -> not protected
        assert_eq!(parse_copyright_info(&[0, 4, 0, 0, 0, 0]), Some(false));
        // byte 4 != 0 (e.g. 1 = CSS/CPPM) -> protected
        assert_eq!(parse_copyright_info(&[0, 4, 0, 0, 1, 0]), Some(true));
    }
}
