//! IMAPI2 burner (IDiscMaster2 / IDiscRecorder2 / IDiscFormat2Data).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};

use windows::core::{implement, Interface, Ref, BSTR, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{E_ABORT, E_NOTIMPL, S_OK, VARIANT_FALSE, VARIANT_TRUE};
use windows::Win32::Storage::Imapi::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, IConnectionPoint, IConnectionPointContainer, IDispatch,
    IDispatch_Impl, ISequentialStream_Impl, IStream, IStream_Impl, ITypeInfo, CLSCTX_ALL, COINIT_MULTITHREADED,
    DISPATCH_FLAGS, DISPPARAMS, EXCEPINFO, LOCKTYPE, SAFEARRAY, STATFLAG, STATSTG, STGC, STREAM_SEEK, STREAM_SEEK_CUR,
    STREAM_SEEK_END, STREAM_SEEK_SET,
};
use windows::Win32::System::Ole::{SafeArrayDestroy, SafeArrayGetElement, SafeArrayGetLBound, SafeArrayGetUBound};
use windows::Win32::System::Variant::{VariantClear, VARIANT, VT_BSTR, VT_DISPATCH, VT_I4};

use super::{
    verify_against_image, BurnError, BurnOptions, BurnPhase, BurnProgress, DriveInfo, MediaInfo, MediaType, OpticalBurner,
    WriteSpeed,
};

const CLIENT_NAME: &str = "convertx-rust";
/// E_IMAPI_RECORDER_MEDIA_NO_MEDIA
const NO_MEDIA: HRESULT = HRESULT(0xC0AA0202u32 as i32);
/// RPC_E_CHANGED_MODE: COM already initialised as STA on this thread (fine for IMAPI2).
const CHANGED_MODE: HRESULT = HRESULT(0x80010106u32 as i32);

fn backend(e: windows::core::Error) -> BurnError {
    if e.code() == NO_MEDIA {
        BurnError::NoMedia
    } else {
        BurnError::Backend(format!("IMAPI2: {} (0x{:08X})", e.message().trim(), e.code().0 as u32))
    }
}

/// COM for the current thread, released on drop. `true`: initialised here as MTA; `false`: the
/// thread already was an STA (no message loop of ours, so no COM events are delivered).
struct Com(bool);

impl Com {
    fn init() -> Result<Self, BurnError> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == CHANGED_MODE {
            return Ok(Com(false));
        }
        hr.ok().map_err(backend)?;
        Ok(Com(true))
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            unsafe { CoUninitialize() };
        }
    }
}

/// Elements of a SAFEARRAY of VARIANTs; the array is destroyed.
unsafe fn variants(sa: *mut SAFEARRAY) -> Vec<VARIANT> {
    let mut out = Vec::new();
    if sa.is_null() {
        return out;
    }
    if let (Ok(lo), Ok(hi)) = (SafeArrayGetLBound(sa, 1), SafeArrayGetUBound(sa, 1)) {
        for i in lo..=hi {
            let mut v = VARIANT::default();
            if SafeArrayGetElement(sa, &i, &mut v as *mut VARIANT as *mut _).is_ok() {
                out.push(v);
            }
        }
    }
    let _ = SafeArrayDestroy(sa);
    out
}

unsafe fn variant_i32(v: &VARIANT) -> Option<i32> {
    let inner = &v.Anonymous.Anonymous;
    (inner.vt == VT_I4).then(|| inner.Anonymous.lVal)
}

unsafe fn variant_string(v: &VARIANT) -> Option<String> {
    let inner = &v.Anonymous.Anonymous;
    (inner.vt == VT_BSTR).then(|| inner.Anonymous.bstrVal.to_string())
}

fn clear(mut vs: Vec<VARIANT>) {
    for v in &mut vs {
        unsafe {
            let _ = VariantClear(v);
        }
    }
}

fn recorder(id: &str) -> Result<IDiscRecorder2, BurnError> {
    unsafe {
        let r: IDiscRecorder2 = CoCreateInstance(&MsftDiscRecorder2, None, CLSCTX_ALL).map_err(backend)?;
        r.InitializeDiscRecorder(&BSTR::from(id)).map_err(backend)?;
        Ok(r)
    }
}

fn format_for(rec: &IDiscRecorder2) -> Result<IDiscFormat2Data, BurnError> {
    unsafe {
        let f: IDiscFormat2Data = CoCreateInstance(&MsftDiscFormat2Data, None, CLSCTX_ALL).map_err(backend)?;
        f.SetRecorder(rec).map_err(backend)?;
        f.SetClientName(&BSTR::from(CLIENT_NAME)).map_err(backend)?;
        Ok(f)
    }
}

fn volume_paths(rec: &IDiscRecorder2) -> Vec<String> {
    unsafe {
        let Ok(sa) = rec.VolumePathNames() else { return vec![] };
        let vs = variants(sa);
        let out = vs.iter().filter_map(|v| variant_string(v)).collect();
        clear(vs);
        out
    }
}

fn physical_name(t: IMAPI_MEDIA_PHYSICAL_TYPE) -> &'static str {
    match t {
        IMAPI_MEDIA_TYPE_DVDPLUSR => "DVD+R",
        IMAPI_MEDIA_TYPE_DVDPLUSRW => "DVD+RW",
        IMAPI_MEDIA_TYPE_DVDPLUSR_DUALLAYER => "DVD+R DL",
        IMAPI_MEDIA_TYPE_DVDPLUSRW_DUALLAYER => "DVD+RW DL",
        IMAPI_MEDIA_TYPE_DVDDASHR => "DVD-R",
        IMAPI_MEDIA_TYPE_DVDDASHRW => "DVD-RW",
        IMAPI_MEDIA_TYPE_DVDDASHR_DUALLAYER => "DVD-R DL",
        IMAPI_MEDIA_TYPE_DVDRAM => "DVD-RAM",
        IMAPI_MEDIA_TYPE_DVDROM => "DVD-ROM",
        IMAPI_MEDIA_TYPE_CDR => "CD-R",
        IMAPI_MEDIA_TYPE_CDRW => "CD-RW",
        IMAPI_MEDIA_TYPE_CDROM => "CD-ROM",
        IMAPI_MEDIA_TYPE_BDR => "BD-R",
        IMAPI_MEDIA_TYPE_BDRE => "BD-RE",
        IMAPI_MEDIA_TYPE_BDROM => "BD-ROM",
        _ => "unbekannt",
    }
}

fn probe(rec: &IDiscRecorder2) -> Result<(MediaInfo, IDiscFormat2Data), BurnError> {
    let format = format_for(rec)?;
    unsafe {
        let physical = match format.CurrentPhysicalMediaType() {
            Ok(t) => t,
            Err(e) if e.code() == NO_MEDIA => return Ok((MediaInfo::none(), format)),
            Err(e) => return Err(backend(e)),
        };
        let supported = format.IsCurrentMediaSupported(rec).map(|b| b.as_bool()).unwrap_or(false);
        let state = format.CurrentMediaStatus().map(|s| s.0).unwrap_or(0);
        let dvd_writable = matches!(
            physical,
            IMAPI_MEDIA_TYPE_DVDPLUSR
                | IMAPI_MEDIA_TYPE_DVDPLUSRW
                | IMAPI_MEDIA_TYPE_DVDPLUSR_DUALLAYER
                | IMAPI_MEDIA_TYPE_DVDPLUSRW_DUALLAYER
                | IMAPI_MEDIA_TYPE_DVDDASHR
                | IMAPI_MEDIA_TYPE_DVDDASHRW
                | IMAPI_MEDIA_TYPE_DVDDASHR_DUALLAYER
                | IMAPI_MEDIA_TYPE_DVDRAM
        );
        let bd_writable = matches!(physical, IMAPI_MEDIA_TYPE_BDR | IMAPI_MEDIA_TYPE_BDRE);
        let total_sectors = format.TotalSectorsOnMedia().unwrap_or(0).max(0) as u64;
        let dual = matches!(
            physical,
            IMAPI_MEDIA_TYPE_DVDPLUSR_DUALLAYER | IMAPI_MEDIA_TYPE_DVDDASHR_DUALLAYER | IMAPI_MEDIA_TYPE_DVDPLUSRW_DUALLAYER
        );
        let rewritable = matches!(
            physical,
            IMAPI_MEDIA_TYPE_DVDPLUSRW
                | IMAPI_MEDIA_TYPE_DVDDASHRW
                | IMAPI_MEDIA_TYPE_DVDPLUSRW_DUALLAYER
                | IMAPI_MEDIA_TYPE_DVDRAM
                | IMAPI_MEDIA_TYPE_BDRE
        );
        let unusable = state & IMAPI_FORMAT2_DATA_MEDIA_STATE_UNSUPPORTED_MASK.0 != 0;
        let blank = state & IMAPI_FORMAT2_DATA_MEDIA_STATE_BLANK.0 != 0;
        let mut speeds: Vec<u32> = match format.SupportedWriteSpeeds() {
            Ok(sa) => {
                let vs = variants(sa);
                let s = vs.iter().filter_map(|v| variant_i32(v)).filter(|&s| s > 0).map(|s| s as u32).collect();
                clear(vs);
                s
            }
            Err(_) => vec![],
        };
        speeds.sort_unstable_by(|a, b| b.cmp(a));
        speeds.dedup();
        let info = MediaInfo {
            present: true,
            media_type: if bd_writable {
                super::bd_media_type(total_sectors)
            } else if !dvd_writable && physical != IMAPI_MEDIA_TYPE_DVDROM {
                MediaType::Unknown
            } else if dual {
                MediaType::Dvd9
            } else {
                MediaType::Dvd5
            },
            physical: physical_name(physical).to_string(),
            blank,
            rewritable,
            writable: supported && (dvd_writable || bd_writable) && !unusable,
            free_sectors: format.FreeSectorsOnMedia().unwrap_or(0).max(0) as u64,
            total_sectors,
            speeds,
            layer_break_supported: physical == IMAPI_MEDIA_TYPE_DVDPLUSR_DUALLAYER,
        };
        Ok((info, format))
    }
}

/// Quick-erases a rewritable disc.
fn erase(rec: &IDiscRecorder2) -> Result<(), BurnError> {
    unsafe {
        let e: IDiscFormat2Erase = CoCreateInstance(&MsftDiscFormat2Erase, None, CLSCTX_ALL).map_err(backend)?;
        e.SetRecorder(rec).map_err(backend)?;
        e.SetClientName(&BSTR::from(CLIENT_NAME)).map_err(backend)?;
        e.SetFullErase(VARIANT_FALSE).map_err(backend)?;
        e.EraseMedia().map_err(backend)
    }
}

/// The DVD+R DL layer boundary (L0 data zone capacity) via SEND DVD STRUCTURE, format 20h.
fn set_layer_break(rec: &IDiscRecorder2, l0_sectors: u32) -> Result<(), BurnError> {
    unsafe {
        let ex: IDiscRecorder2Ex = rec.cast().map_err(backend)?;
        rec.AcquireExclusiveAccess(VARIANT_FALSE, &BSTR::from(CLIENT_NAME)).map_err(backend)?;
        let cdb = [0xBF, 0, 0, 0, 0, 0, 0, 0x20, 0, 12, 0, 0];
        let mut data = [0u8; 12];
        data[1] = 10; // data length (excluding the length field)
        data[8..12].copy_from_slice(&l0_sectors.to_be_bytes());
        let mut sense = [0u8; 18];
        let r = ex.SendCommandSendDataToDevice(&cdb, &mut sense, 30, &data);
        let _ = rec.ReleaseExclusiveAccess();
        r.map_err(|e| BurnError::Backend(format!("Layerwechsel konnte nicht gesetzt werden: {e} (Sense {:02X?})", &sense[..14])))
    }
}

/// IStream over the image file that reports progress and aborts on cancel.
#[implement(IStream)]
struct ImageStream {
    file: Mutex<File>,
    len: u64,
    pos: AtomicU64,
    reported: AtomicU64,
    cancel: *const AtomicBool,
    tx: Mutex<Sender<BurnProgress>>,
    /// Set once IMAPI's write events arrive; until then (or without them) progress is derived
    /// from the data handed to IMAPI, which runs ahead of the real write position (buffering).
    events_seen: Arc<AtomicBool>,
}

impl ImageStream {
    fn cancelled(&self) -> bool {
        // SAFETY: the flag outlives the stream (burn_iso waits for Write to return).
        unsafe { (*self.cancel).load(Ordering::Relaxed) }
    }

    fn send(&self, p: BurnProgress) {
        let _ = self.tx.lock().unwrap().send(p);
    }
}

impl ISequentialStream_Impl for ImageStream_Impl {
    fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if self.cancelled() {
            return E_ABORT;
        }
        let buf = unsafe { std::slice::from_raw_parts_mut(pv as *mut u8, cb as usize) };
        let mut file = self.file.lock().unwrap();
        let mut n = 0;
        while n < buf.len() {
            match file.read(&mut buf[n..]) {
                Ok(0) => break,
                Ok(r) => n += r,
                Err(_) => return E_ABORT,
            }
        }
        if !pcbread.is_null() {
            unsafe { *pcbread = n as u32 };
        }
        let pos = self.pos.fetch_add(n as u64, Ordering::Relaxed) + n as u64;
        // Report every ~8 MB and at the end.
        let last = self.reported.load(Ordering::Relaxed);
        if self.events_seen.load(Ordering::Relaxed) {
        } else if pos >= self.len {
            self.send(BurnProgress::new(BurnPhase::Finalizing, 0.0, "Disc wird abgeschlossen …"));
        } else if pos - last.min(pos) >= 8 << 20 || last == 0 {
            self.reported.store(pos, Ordering::Relaxed);
            self.send(BurnProgress::new(BurnPhase::Writing, pos as f64 / self.len as f64, "Daten werden geschrieben …"));
        }
        S_OK
    }

    fn Write(&self, _pv: *const core::ffi::c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        E_NOTIMPL
    }
}

impl IStream_Impl for ImageStream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> windows::core::Result<()> {
        let mut file = self.file.lock().unwrap();
        let target = match dworigin {
            STREAM_SEEK_SET => SeekFrom::Start(dlibmove as u64),
            STREAM_SEEK_CUR => SeekFrom::Current(dlibmove),
            STREAM_SEEK_END => SeekFrom::End(dlibmove),
            _ => return Err(E_NOTIMPL.into()),
        };
        let pos = file.seek(target).map_err(|_| windows::core::Error::from(E_ABORT))?;
        self.pos.store(pos, Ordering::Relaxed);
        if !plibnewposition.is_null() {
            unsafe { *plibnewposition = pos };
        }
        Ok(())
    }
    fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn CopyTo(&self, _pstm: windows::core::Ref<IStream>, _cb: u64, _r: *mut u64, _w: *mut u64) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
        Ok(())
    }
    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn LockRegion(&self, _o: u64, _cb: u64, _t: &LOCKTYPE) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn UnlockRegion(&self, _o: u64, _cb: u64, _t: u32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        if !pstatstg.is_null() {
            unsafe {
                *pstatstg = STATSTG { r#type: 2 /* STGTY_STREAM */, cbSize: self.len, ..Default::default() };
            }
        }
        Ok(())
    }
    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(E_NOTIMPL.into())
    }
}

/// Progress for one IMAPI write update (`DDiscFormat2DataEvents::Update`): the current action and
/// the sector range being written. `None` when there is nothing to report.
fn write_update(action: IMAPI_FORMAT2_DATA_WRITE_ACTION, start_lba: i32, sector_count: i32, last_written_lba: i32) -> Option<BurnProgress> {
    match action {
        IMAPI_FORMAT2_DATA_WRITE_ACTION_VALIDATING_MEDIA
        | IMAPI_FORMAT2_DATA_WRITE_ACTION_FORMATTING_MEDIA
        | IMAPI_FORMAT2_DATA_WRITE_ACTION_INITIALIZING_HARDWARE
        | IMAPI_FORMAT2_DATA_WRITE_ACTION_CALIBRATING_POWER => {
            Some(BurnProgress::new(BurnPhase::Formatting, 0.0, "Disc wird vorbereitet …"))
        }
        IMAPI_FORMAT2_DATA_WRITE_ACTION_WRITING_DATA if sector_count > 0 => {
            let done = (last_written_lba as i64 - start_lba as i64).max(0) as f64;
            Some(BurnProgress::new(BurnPhase::Writing, done / sector_count as f64, "Daten werden geschrieben …"))
        }
        // IMAPI's own verification (if a drive does it) counts as part of closing the disc.
        IMAPI_FORMAT2_DATA_WRITE_ACTION_FINALIZATION | IMAPI_FORMAT2_DATA_WRITE_ACTION_VERIFYING => {
            Some(BurnProgress::new(BurnPhase::Finalizing, 0.0, "Disc wird abgeschlossen …"))
        }
        _ => None,
    }
}

/// Sink for IMAPI's write events: the real write position (instead of the data handed to IMAPI)
/// and the finalizing phase. Cancels the write from inside the event when asked to.
#[implement(DDiscFormat2DataEvents)]
struct WriteEvents {
    format: IDiscFormat2Data,
    cancel: *const AtomicBool,
    tx: Mutex<Sender<BurnProgress>>,
    /// Last report (phase, per mille) to skip repeats.
    last: Mutex<Option<(BurnPhase, u32)>>,
    /// Tells the image stream to stop reporting.
    seen: Arc<AtomicBool>,
}

impl WriteEvents {
    fn update(&self, progress: &IDispatch) {
        // SAFETY: the flag outlives the sink (burn_iso unadvises it before returning).
        if unsafe { (*self.cancel).load(Ordering::Relaxed) } {
            unsafe {
                let _ = self.format.CancelWrite();
            }
            return;
        }
        let Ok(args) = progress.cast::<IDiscFormat2DataEventArgs>() else { return };
        self.seen.store(true, Ordering::Relaxed);
        let p = unsafe {
            let (Ok(action), Ok(start), Ok(count), Ok(last)) =
                (args.CurrentAction(), args.StartLba(), args.SectorCount(), args.LastWrittenLba())
            else {
                return;
            };
            write_update(action, start, count, last)
        };
        let Some(p) = p else { return };
        let key = (p.phase, (p.fraction * 1000.0) as u32);
        let mut last = self.last.lock().unwrap();
        if *last != Some(key) {
            *last = Some(key);
            let _ = self.tx.lock().unwrap().send(p);
        }
    }
}

impl DDiscFormat2DataEvents_Impl for WriteEvents_Impl {
    fn Update(&self, _object: Ref<IDispatch>, progress: Ref<IDispatch>) -> windows::core::Result<()> {
        if let Ok(progress) = progress.ok() {
            WriteEvents::update(self, progress);
        }
        Ok(())
    }
}

/// IMAPI raises the events through `IDispatch::Invoke` (DISPID 0x200, arguments in reverse
/// order: progress, object).
impl IDispatch_Impl for WriteEvents_Impl {
    fn GetTypeInfoCount(&self) -> windows::core::Result<u32> {
        Ok(0)
    }
    fn GetTypeInfo(&self, _itinfo: u32, _lcid: u32) -> windows::core::Result<ITypeInfo> {
        Err(E_NOTIMPL.into())
    }
    fn GetIDsOfNames(&self, _riid: *const GUID, _names: *const PCWSTR, _count: u32, _lcid: u32, _ids: *mut i32) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn Invoke(
        &self,
        dispid: i32,
        _riid: *const GUID,
        _lcid: u32,
        _flags: DISPATCH_FLAGS,
        params: *const DISPPARAMS,
        _result: *mut VARIANT,
        _excepinfo: *mut EXCEPINFO,
        _argerr: *mut u32,
    ) -> windows::core::Result<()> {
        if dispid as u32 != DISPID_DDISCFORMAT2DATAEVENTS_UPDATE || params.is_null() {
            return Ok(());
        }
        unsafe {
            let params = &*params;
            if params.cArgs < 1 || params.rgvarg.is_null() {
                return Ok(());
            }
            let inner = &(*params.rgvarg).Anonymous.Anonymous;
            if inner.vt == VT_DISPATCH {
                if let Some(progress) = (*inner.Anonymous.pdispVal).as_ref() {
                    WriteEvents::update(self, progress);
                }
            }
        }
        Ok(())
    }
}

/// An advised [`WriteEvents`] sink; unadvised on drop.
struct EventConnection {
    point: IConnectionPoint,
    cookie: u32,
}

impl EventConnection {
    /// Connects a sink to `format`; `None` when IMAPI does not offer the events (the stream's
    /// progress is used then). Events need an MTA thread (they arrive on IMAPI's threads).
    fn advise(format: &IDiscFormat2Data, cancel: &AtomicBool, tx: Sender<BurnProgress>, seen: Arc<AtomicBool>) -> Option<Self> {
        unsafe {
            let container: IConnectionPointContainer = format.cast().ok()?;
            let point = container.FindConnectionPoint(&DDiscFormat2DataEvents::IID).ok()?;
            let sink: DDiscFormat2DataEvents = WriteEvents {
                format: format.clone(),
                cancel: cancel as *const AtomicBool,
                tx: Mutex::new(tx),
                last: Mutex::new(None),
                seen,
            }
            .into();
            let cookie = point.Advise(&sink).ok()?;
            Some(EventConnection { point, cookie })
        }
    }
}

impl Drop for EventConnection {
    fn drop(&mut self) {
        // Also breaks the reference cycle format → connection point → sink → format.
        unsafe {
            let _ = self.point.Unadvise(self.cookie);
        }
    }
}

/// Burner using the Windows Image Mastering API v2.
pub struct ImapiBurner;

impl OpticalBurner for ImapiBurner {
    fn list_drives(&self) -> Result<Vec<DriveInfo>, BurnError> {
        let _com = Com::init()?;
        unsafe {
            let master: IDiscMaster2 = CoCreateInstance(&MsftDiscMaster2, None, CLSCTX_ALL).map_err(backend)?;
            if !master.IsSupportedEnvironment().map(|b| b.as_bool()).unwrap_or(false) {
                return Ok(vec![]);
            }
            let mut drives = Vec::new();
            for i in 0..master.Count().map_err(backend)? {
                let id = master.get_Item(i).map_err(backend)?.to_string();
                let Ok(rec) = recorder(&id) else { continue };
                // Only drives IMAPI can write data discs with.
                let Ok(format) = format_for(&rec) else { continue };
                if !format.IsRecorderSupported(&rec).map(|b| b.as_bool()).unwrap_or(false) {
                    continue;
                }
                let vendor = rec.VendorId().map(|s| s.to_string()).unwrap_or_default();
                let product = rec.ProductId().map(|s| s.to_string()).unwrap_or_default();
                let path = volume_paths(&rec).into_iter().next().unwrap_or_default();
                let mut name = format!("{vendor} {product}").split_whitespace().collect::<Vec<_>>().join(" ");
                if !path.is_empty() {
                    name = format!("{name} ({})", path.trim_end_matches('\\'));
                }
                drives.push(DriveInfo { id, name, path });
            }
            Ok(drives)
        }
    }

    fn probe_media(&self, drive_id: &str) -> Result<MediaInfo, BurnError> {
        let _com = Com::init()?;
        let rec = recorder(drive_id)?;
        probe(&rec).map(|(info, _)| info)
    }

    fn burn_iso(
        &self,
        drive_id: &str,
        iso_path: &Path,
        speed: WriteSpeed,
        options: &BurnOptions,
        cancel: &AtomicBool,
        progress_tx: Sender<BurnProgress>,
    ) -> Result<(), BurnError> {
        let com = Com::init()?;
        let _ = progress_tx.send(BurnProgress::new(BurnPhase::Preparing, 0.0, "Laufwerk wird vorbereitet …"));
        let rec = recorder(drive_id)?;
        let (media, format) = probe(&rec)?;
        let file = File::open(iso_path)?;
        let len = file.metadata()?.len();
        let sectors = len.div_ceil(2048);
        if !media.present {
            return Err(BurnError::NoMedia);
        }
        if !media.writable {
            return Err(BurnError::Unsupported(format!("{} kann in diesem Laufwerk nicht beschrieben werden", media.physical)));
        }
        if !media.blank && !media.rewritable {
            return Err(BurnError::Unsupported(format!("die {} ist nicht leer", media.physical)));
        }
        if !media.blank && !options.erase {
            return Err(BurnError::Unsupported(format!("die {} enthält Daten und darf nicht gelöscht werden", media.physical)));
        }
        let (media, format) = if !media.blank && media.physical == "DVD-RW" {
            // Sequential DVD-RW has to be blanked before it can take a new disc-at-once image.
            let _ = progress_tx.send(BurnProgress::new(BurnPhase::Formatting, 0.0, "DVD-RW wird gelöscht …"));
            erase(&rec)?;
            probe(&rec)?
        } else {
            (media, format)
        };
        let free = if media.blank { media.free_sectors } else { media.total_sectors };
        if free < sectors {
            return Err(BurnError::NotEnoughSpace { needed: sectors, free });
        }
        unsafe {
            // DVD+R DL: forcing the close fails at the end of the write on some drives
            // (IMAPI_E_FEATURE_NOT_CURRENT, seen at ~94 %), so IMAPI's default close is used.
            if media.media_type != MediaType::Dvd9 {
                format.SetForceMediaToBeClosed(VARIANT_TRUE).map_err(backend)?;
            }
            if !media.blank {
                // Rewritable disc with old content: overwrite from the start.
                format.SetForceOverwrite(VARIANT_TRUE).map_err(backend)?;
            }
            let sps = match speed {
                WriteSpeed::Max => -1,
                WriteSpeed::Specific(s) => s as i32,
            };
            format.SetWriteSpeed(sps, VARIANT_FALSE).map_err(backend)?;
        }
        if let Some(l0) = options.layer_break.filter(|_| media.media_type == MediaType::Dvd9) {
            if media.layer_break_supported {
                set_layer_break(&rec, l0)?;
            } else {
                let _ = progress_tx.send(BurnProgress::new(
                    BurnPhase::Preparing,
                    0.0,
                    format!("{}: Layerwechsel wird vom Laufwerk festgelegt", media.physical),
                ));
            }
        }
        let _ = progress_tx.send(BurnProgress::new(BurnPhase::Formatting, 0.0, "Disc wird vorbereitet …"));
        // IMAPI's write events report the real position; the image stream reports until the
        // first event arrives (or throughout, without events).
        let events_seen = Arc::new(AtomicBool::new(false));
        let events = if com.0 { EventConnection::advise(&format, cancel, progress_tx.clone(), events_seen.clone()) } else { None };
        let stream: IStream = ImageStream {
            file: Mutex::new(file),
            len,
            pos: AtomicU64::new(0),
            reported: AtomicU64::new(0),
            cancel: cancel as *const AtomicBool,
            tx: Mutex::new(progress_tx.clone()),
            events_seen,
        }
        .into();
        let written = unsafe { format.Write(&stream) };
        drop(events);
        drop(stream);
        if let Err(e) = written {
            return Err(if cancel.load(Ordering::Relaxed) { BurnError::Cancelled } else { backend(e) });
        }

        if options.verify {
            let letter = volume_paths(&rec).into_iter().next().unwrap_or_default();
            let letter = letter.trim_end_matches('\\');
            if letter.is_empty() {
                return Err(BurnError::Backend("Überprüfung nicht möglich: Laufwerk hat keinen Laufwerksbuchstaben".into()));
            }
            let _ = progress_tx.send(BurnProgress::new(BurnPhase::Verifying, 0.0, "Disc wird überprüft …"));
            let mut device = File::open(format!(r"\\.\{letter}"))?;
            let tx = progress_tx.clone();
            verify_against_image(&mut device, iso_path, cancel, &mut |f| {
                let _ = tx.send(BurnProgress::new(BurnPhase::Verifying, f, "Disc wird überprüft …"));
            })?;
        }
        if options.eject {
            unsafe {
                let _ = rec.EjectMedia();
            }
        }
        let _ = progress_tx.send(BurnProgress::new(BurnPhase::Done, 1.0, "Fertig"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_updates_map_to_phases() {
        let prep = write_update(IMAPI_FORMAT2_DATA_WRITE_ACTION_CALIBRATING_POWER, 0, 0, 0).unwrap();
        assert_eq!((prep.phase, prep.fraction), (BurnPhase::Formatting, 0.0));
        assert_eq!(prep.message, "Disc wird vorbereitet …", "same text as before the write starts");
        for a in [IMAPI_FORMAT2_DATA_WRITE_ACTION_VALIDATING_MEDIA, IMAPI_FORMAT2_DATA_WRITE_ACTION_FORMATTING_MEDIA, IMAPI_FORMAT2_DATA_WRITE_ACTION_INITIALIZING_HARDWARE] {
            assert_eq!(write_update(a, 0, 0, 0).unwrap().phase, BurnPhase::Formatting);
        }
        // Written fraction from the LBA range.
        let w = write_update(IMAPI_FORMAT2_DATA_WRITE_ACTION_WRITING_DATA, 0, 2_000_000, 500_000).unwrap();
        assert_eq!(w.phase, BurnPhase::Writing);
        assert!((w.fraction - 0.25).abs() < 1e-9);
        assert_eq!(w.message, "Daten werden geschrieben …");
        let w = write_update(IMAPI_FORMAT2_DATA_WRITE_ACTION_WRITING_DATA, 1000, 2000, 2000).unwrap();
        assert!((w.fraction - 0.5).abs() < 1e-9, "relative to the start LBA");
        assert_eq!(write_update(IMAPI_FORMAT2_DATA_WRITE_ACTION_WRITING_DATA, 1000, 2000, 500).unwrap().fraction, 0.0);
        assert_eq!(write_update(IMAPI_FORMAT2_DATA_WRITE_ACTION_WRITING_DATA, 0, 100, 250).unwrap().fraction, 1.0, "clamped");
        assert!(write_update(IMAPI_FORMAT2_DATA_WRITE_ACTION_WRITING_DATA, 0, 0, 10).is_none(), "no sector count yet");
        for a in [IMAPI_FORMAT2_DATA_WRITE_ACTION_FINALIZATION, IMAPI_FORMAT2_DATA_WRITE_ACTION_VERIFYING] {
            let f = write_update(a, 0, 100, 100).unwrap();
            assert_eq!((f.phase, f.message.as_str()), (BurnPhase::Finalizing, "Disc wird abgeschlossen …"));
        }
        assert!(write_update(IMAPI_FORMAT2_DATA_WRITE_ACTION_COMPLETED, 0, 100, 100).is_none());
        assert!(write_update(IMAPI_FORMAT2_DATA_WRITE_ACTION(99), 0, 100, 100).is_none());
    }

    /// The sink can be connected to a real IMAPI format object (no disc access, no write).
    #[test]
    fn write_events_can_be_advised() {
        let _com = Com::init().unwrap();
        let format: IDiscFormat2Data = unsafe { CoCreateInstance(&MsftDiscFormat2Data, None, CLSCTX_ALL) }.expect("IMAPI2 available");
        let (tx, _rx) = std::sync::mpsc::channel();
        let cancel = AtomicBool::new(false);
        let connection = EventConnection::advise(&format, &cancel, tx, Arc::new(AtomicBool::new(false)))
            .expect("IMAPI offers DDiscFormat2DataEvents");
        drop(connection);
    }

    /// Enumerates the real drives (no disc access beyond IMAPI queries).
    #[test]
    fn enumerate_drives() {
        let drives = ImapiBurner.list_drives().expect("IMAPI2 available");
        for d in &drives {
            eprintln!("drive: {d:?}");
            let media = ImapiBurner.probe_media(&d.id).expect("probe");
            eprintln!("  media: {media:?}");
        }
    }
}
