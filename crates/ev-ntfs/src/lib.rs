//! Windows file-system access for everything-rs.
//!
//! Two indexing strategies, mirroring Everything:
//!
//! * **NTFS volumes** - `FSCTL_ENUM_USN_DATA` walks the master file table in
//!   bulk (no per-file syscalls at all), then `FSCTL_READ_USN_JOURNAL` streams
//!   incremental changes.
//! * **Anything else** (network shares, FAT sticks, or NTFS without
//!   privileges) - an ordinary directory walk, optionally rescanned.
//!
//! Both paths produce [`ev_core::RawEntry`] values that feed the same index.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ev_core::index::{Index, IndexBuilder, RawEntry, VolumeData};
use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_HANDLE_EOF, ERROR_MORE_DATA, GENERIC_READ, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetDiskFreeSpaceExW, GetDriveTypeW, GetLogicalDriveStringsW, GetVolumeInformationW,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

const FSCTL_ENUM_USN_DATA: u32 = 0x0009_00B3;
const FSCTL_READ_USN_JOURNAL: u32 = 0x0009_00BB;
const FSCTL_QUERY_USN_JOURNAL: u32 = 0x0009_00F4;
const FSCTL_CREATE_USN_JOURNAL: u32 = 0x0009_00E7;

const DRIVE_FIXED: u32 = 3;
const DRIVE_REMOTE: u32 = 4;
const DRIVE_REMOVABLE: u32 = 2;

const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x10;

// USN reason bits
const USN_REASON_DATA_OVERWRITE: u32 = 0x0000_0001;
const USN_REASON_DATA_EXTEND: u32 = 0x0000_0002;
const USN_REASON_DATA_TRUNCATION: u32 = 0x0000_0004;
const USN_REASON_FILE_CREATE: u32 = 0x0000_0100;
const USN_REASON_FILE_DELETE: u32 = 0x0000_0200;
const USN_REASON_RENAME_NEW_NAME: u32 = 0x0000_2000;
const USN_REASON_BASIC_INFO_CHANGE: u32 = 0x0000_8000;

#[derive(Debug, Clone)]
pub struct VolumeInfo {
    pub letter: char,
    pub root: String,
    pub label: String,
    pub fs: String,
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub kind: &'static str,
    pub can_use_usn: bool,
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error() -> std::io::Error {
    std::io::Error::last_os_error()
}

/// Every drive that can hold an index (fixed, removable and mapped network).
pub fn list_volumes() -> Vec<VolumeInfo> {
    let mut out = Vec::new();
    unsafe {
        let len = GetLogicalDriveStringsW(0, std::ptr::null_mut());
        if len == 0 {
            return out;
        }
        let mut buf = vec![0u16; len as usize + 2];
        let written = GetLogicalDriveStringsW(buf.len() as u32, buf.as_mut_ptr());
        if written == 0 {
            return out;
        }
        let mut start = 0usize;
        for i in 0..written as usize {
            if buf[i] == 0 {
                if i == start {
                    break;
                }
                let root = String::from_utf16_lossy(&buf[start..i]);
                start = i + 1;
                let wroot = wide(&root);
                let dtype = GetDriveTypeW(wroot.as_ptr());
                if dtype != DRIVE_FIXED && dtype != DRIVE_REMOTE && dtype != DRIVE_REMOVABLE {
                    continue;
                }
                let mut label = vec![0u16; 256];
                let mut fs = vec![0u16; 64];
                let mut serial = 0u32;
                let mut maxcomp = 0u32;
                let mut flags = 0u32;
                let ok = GetVolumeInformationW(
                    wroot.as_ptr(),
                    label.as_mut_ptr(),
                    label.len() as u32,
                    &mut serial,
                    &mut maxcomp,
                    &mut flags,
                    fs.as_mut_ptr(),
                    fs.len() as u32,
                );
                let fs_name = if ok != 0 {
                    String::from_utf16_lossy(
                        &fs[..fs.iter().position(|&c| c == 0).unwrap_or(0)],
                    )
                } else {
                    String::new()
                };
                let label_name = if ok != 0 {
                    String::from_utf16_lossy(
                        &label[..label.iter().position(|&c| c == 0).unwrap_or(0)],
                    )
                } else {
                    String::new()
                };
                let mut free_avail = 0u64;
                let mut total = 0u64;
                let mut free_total = 0u64;
                let _ = GetDiskFreeSpaceExW(
                    wroot.as_ptr(),
                    &mut free_avail,
                    &mut total,
                    &mut free_total,
                );
                out.push(VolumeInfo {
                    letter: root.chars().next().unwrap_or('?'),
                    root,
                    label: label_name,
                    fs: fs_name.clone(),
                    total_bytes: total,
                    free_bytes: free_total,
                    kind: match dtype {
                        DRIVE_FIXED => "fixed",
                        DRIVE_REMOTE => "network",
                        _ => "removable",
                    },
                    can_use_usn: fs_name.eq_ignore_ascii_case("NTFS"),
                });
            }
        }
    }
    out
}

// --------------------------------------------------------------- MFT / USN

#[repr(C)]
#[derive(Clone, Copy)]
struct MftEnumData {
    start_frn: u64,
    low_usn: i64,
    high_usn: i64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct UsnJournalData {
    journal_id: u64,
    first_usn: i64,
    next_usn: i64,
    max_usn: i64,
    maximum_size: u64,
    allocation_delta: u64,
    min_major: u16,
    max_major: u16,
    min_minor: u16,
    max_minor: u16,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct ReadUsnJournalData {
    start_usn: i64,
    reason_mask: u32,
    return_only_on_close: u32,
    timeout: u64,
    bytes_to_wait_for: u32,
    journal_id: u64,
}

struct VolumeHandle(HANDLE);

impl Drop for VolumeHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn open_volume(letter: char) -> std::io::Result<VolumeHandle> {
    let path = wide(&format!("\\\\.\\{letter}:"));
    let share = windows_sys::Win32::Storage::FileSystem::FILE_SHARE_READ
        | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_WRITE
        | windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
    let h = unsafe {
        CreateFileW(
            path.as_ptr(),
            GENERIC_READ,
            share,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if h == INVALID_HANDLE_VALUE || h.is_null() {
        Err(last_error())
    } else {
        Ok(VolumeHandle(h))
    }
}

unsafe fn device_io<T, U>(
    h: HANDLE,
    code: u32,
    input: &T,
    output: &mut U,
) -> std::io::Result<u32> {
    let mut got: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            h,
            code,
            input as *const T as *const c_void,
            std::mem::size_of::<T>() as u32,
            output as *mut U as *mut c_void,
            std::mem::size_of::<U>() as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        Err(last_error())
    } else {
        Ok(got)
    }
}

/// Enumerate every MFT record. This is the fast path: one ioctl stream, no
/// per-file calls. Requires a volume handle opened for read (administrator or
/// the Everything-style service).
pub fn enumerate_mft(
    letter: char,
    counter: &AtomicU64,
) -> std::io::Result<Vec<RawEntry>> {
    let h = open_volume(letter)?;
    let mut out = Vec::with_capacity(1 << 16);
    let mut data = MftEnumData {
        start_frn: 0,
        low_usn: 0,
        high_usn: i64::MAX,
    };
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let mut got: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                h.0,
                FSCTL_ENUM_USN_DATA,
                &data as *const MftEnumData as *const c_void,
                std::mem::size_of::<MftEnumData>() as u32,
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                &mut got,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let err = last_error();
            let code = err.raw_os_error().unwrap_or(0) as u32;
            if code == ERROR_HANDLE_EOF {
                break;
            }
            if code != ERROR_MORE_DATA {
                return Err(err);
            }
        }
        if got as usize <= 8 {
            break;
        }
        let next = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let before = out.len();
        parse_records(&buf[8..got as usize], &mut out);
        counter.fetch_add((out.len() - before) as u64, Ordering::Relaxed);
        if next == data.start_frn {
            break;
        }
        data.start_frn = next;
    }
    Ok(out)
}

fn parse_records(buf: &[u8], out: &mut Vec<RawEntry>) {
    let mut off = 0usize;
    while off + 60 <= buf.len() {
        let len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        if len < 60 || off + len > buf.len() {
            break;
        }
        let rec = &buf[off..off + len];
        off += (len + 7) & !7;
        let major = u16::from_le_bytes(rec[4..6].try_into().unwrap());
        if major != 2 {
            // USN_RECORD_V3 uses 128 bit references; not handled here.
            continue;
        }
        let frn = u64::from_le_bytes(rec[8..16].try_into().unwrap());
        let parent = u64::from_le_bytes(rec[16..24].try_into().unwrap());
        let attrs = u32::from_le_bytes(rec[52..56].try_into().unwrap());
        let name_len = u16::from_le_bytes(rec[56..58].try_into().unwrap()) as usize;
        let name_off = u16::from_le_bytes(rec[58..60].try_into().unwrap()) as usize;
        if name_len == 0 || name_off + name_len > rec.len() {
            continue;
        }
        // Skip NTFS metadata records ($MFT, $LogFile, ...).
        if frn < 16 {
            continue;
        }
        let units: Vec<u16> = rec[name_off..name_off + name_len]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let name = String::from_utf16_lossy(&units);
        if name == "." || name == ".." || name.is_empty() {
            continue;
        }
        out.push(RawEntry::new(frn, parent, name, attrs));
    }
}

fn query_journal(h: HANDLE) -> std::io::Result<UsnJournalData> {
    let input = 0u64;
    let mut data = UsnJournalData::default();
    unsafe { device_io(h, FSCTL_QUERY_USN_JOURNAL, &input, &mut data)? };
    Ok(data)
}

/// Create a USN journal on a volume that has none (some freshly formatted or
/// cloned volumes lack one).
pub fn ensure_journal(letter: char) -> std::io::Result<(u64, i64)> {
    let h = open_volume(letter)?;
    if let Ok(j) = query_journal(h.0) {
        return Ok((j.journal_id, j.next_usn));
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct CreateUsnJournalData {
        maximum_size: u64,
        allocation_delta: u64,
        journal_id: u64,
    }
    let mut out = 0u64;
    let create = CreateUsnJournalData {
        maximum_size: 0x400_0000,
        allocation_delta: 0x20_0000,
        journal_id: 0,
    };
    unsafe { device_io(h.0, FSCTL_CREATE_USN_JOURNAL, &create, &mut out)? };
    let j = query_journal(h.0)?;
    Ok((j.journal_id, j.next_usn))
}

#[derive(Debug, Default, Clone)]
pub struct JournalDelta {
    pub upserts: Vec<RawEntry>,
    pub removes: Vec<u64>,
}

struct JournalCursor {
    letter: char,
    handle: VolumeHandle,
    journal_id: u64,
    next_usn: i64,
}

/// Streams incremental changes from every NTFS USN journal.
pub struct JournalWatcher {
    cursors: Vec<JournalCursor>,
    pub errors: Vec<String>,
}

impl JournalWatcher {
    pub fn start(volumes: &[char]) -> Self {
        let mut cursors = Vec::new();
        let mut errors = Vec::new();
        for &letter in volumes {
            match open_volume(letter).and_then(|h| {
                let j = query_journal(h.0)?;
                Ok((h, j))
            }) {
                Ok((h, j)) => cursors.push(JournalCursor {
                    letter,
                    handle: h,
                    journal_id: j.journal_id,
                    next_usn: j.next_usn,
                }),
                Err(e) => errors.push(format!("{letter}: {e}")),
            }
        }
        Self { cursors, errors }
    }

    pub fn is_empty(&self) -> bool {
        self.cursors.is_empty()
    }

    /// Non-blocking read of everything that changed since the last poll.
    pub fn poll(&mut self) -> Vec<(char, JournalDelta)> {
        let mut out = Vec::new();
        for c in &mut self.cursors {
            match read_changes(c.handle.0, c.journal_id, c.next_usn) {
                Ok((next, delta)) => {
                    c.next_usn = next;
                    if !delta.upserts.is_empty() || !delta.removes.is_empty() {
                        out.push((c.letter, delta));
                    }
                }
                Err(_) => {}
            }
        }
        out
    }
}

fn read_changes(
    h: HANDLE,
    journal_id: u64,
    start_usn: i64,
) -> std::io::Result<(i64, JournalDelta)> {
    let input = ReadUsnJournalData {
        start_usn,
        reason_mask: 0xFFFF_FFFF,
        return_only_on_close: 0,
        timeout: 0,
        bytes_to_wait_for: 0,
        journal_id,
    };
    let mut buf = vec![0u8; 1 << 20];
    let mut got: u32 = 0;
    let ok = unsafe {
        DeviceIoControl(
            h,
            FSCTL_READ_USN_JOURNAL,
            &input as *const ReadUsnJournalData as *const c_void,
            std::mem::size_of::<ReadUsnJournalData>() as u32,
            buf.as_mut_ptr() as *mut c_void,
            buf.len() as u32,
            &mut got,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 && got < 8 {
        return Err(last_error());
    }
    if got < 8 {
        return Ok((start_usn, JournalDelta::default()));
    }
    let next = i64::from_le_bytes(buf[0..8].try_into().unwrap());

    // Net effect per FRN: a create/rename wins over a delete, so a rename that
    // emits RENAME_OLD_NAME + RENAME_NEW_NAME ends up as a single upsert.
    let mut changes: Vec<(u64, u8, Option<RawEntry>)> = Vec::new();
    let mut pos = std::collections::HashMap::new();
    let mut off = 8usize;
    while off + 60 <= got as usize {
        let len = u32::from_le_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
        if len < 60 || off + len > got as usize {
            break;
        }
        let rec = &buf[off..off + len];
        off += (len + 7) & !7;
        let major = u16::from_le_bytes(rec[4..6].try_into().unwrap());
        if major != 2 {
            continue;
        }
        let frn = u64::from_le_bytes(rec[8..16].try_into().unwrap());
        let parent = u64::from_le_bytes(rec[16..24].try_into().unwrap());
        let reason = u32::from_le_bytes(rec[40..44].try_into().unwrap());
        let attrs = u32::from_le_bytes(rec[52..56].try_into().unwrap());
        let name_len = u16::from_le_bytes(rec[56..58].try_into().unwrap()) as usize;
        let name_off = u16::from_le_bytes(rec[58..60].try_into().unwrap()) as usize;
        if frn < 16 {
            continue;
        }
        let structural = reason
            & (USN_REASON_FILE_CREATE
                | USN_REASON_RENAME_NEW_NAME
                | USN_REASON_FILE_DELETE
                | USN_REASON_BASIC_INFO_CHANGE
                | USN_REASON_DATA_OVERWRITE
                | USN_REASON_DATA_EXTEND
                | USN_REASON_DATA_TRUNCATION)
            != 0;
        if !structural {
            continue;
        }
        let name = if name_off + name_len <= rec.len() && name_len > 0 {
            let units: Vec<u16> = rec[name_off..name_off + name_len]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            String::new()
        };
        let is_delete = reason & USN_REASON_FILE_DELETE != 0
            && reason & (USN_REASON_FILE_CREATE | USN_REASON_RENAME_NEW_NAME) == 0;
        let key = frn;
        let idx = *pos.entry(key).or_insert_with(|| {
            changes.push((frn, 0, None));
            changes.len() - 1
        });
        if is_delete && name.is_empty() {
            changes[idx].1 = 2;
            changes[idx].2 = None;
        } else if !name.is_empty() && !name.starts_with('$') {
            changes[idx].1 = 1;
            changes[idx].2 = Some(RawEntry::new(frn, parent, name, attrs));
        }
    }

    let mut delta = JournalDelta::default();
    for (frn, kind, entry) in changes {
        match kind {
            1 => {
                if let Some(e) = entry {
                    delta.upserts.push(e);
                }
            }
            2 => delta.removes.push(frn),
            _ => {}
        }
    }
    Ok((next, delta))
}

// ------------------------------------------------------------- folder walk

/// Recursively scan a directory tree, assigning synthetic FRNs. Used for
/// network shares, non-NTFS volumes and unprivileged NTFS access.
///
/// `FindFirstFileW`/`FindNextFileW` return attributes, size and all three
/// timestamps for a whole directory in one call, so a folder scan costs one
/// syscall per directory instead of one per file.
pub fn walk_folder(root: &str, counter: &AtomicU64) -> (Vec<RawEntry>, u64) {
    use windows_sys::Win32::Storage::FileSystem::{
        FindClose, FindFirstFileW, FindNextFileW, WIN32_FIND_DATAW,
    };
    let mut out = Vec::new();
    let mut next_frn: u64 = 1 << 48;
    let mut stack: Vec<(String, u64)> = vec![(root.to_string(), 5)];
    let mut fd: WIN32_FIND_DATAW = unsafe { std::mem::zeroed() };
    while let Some((dir, dir_frn)) = stack.pop() {
        let pattern = if dir.ends_with('\\') {
            format!("{dir}*")
        } else {
            format!("{dir}\\*")
        };
        let wpattern = wide(&pattern);
        let handle = unsafe { FindFirstFileW(wpattern.as_ptr(), &mut fd) };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            continue;
        }
        loop {
            let name_len = fd
                .cFileName
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(fd.cFileName.len());
            let name = String::from_utf16_lossy(&fd.cFileName[..name_len]);
            if name != "." && name != ".." {
                let attrs = fd.dwFileAttributes;
                let is_dir = attrs & FILE_ATTRIBUTE_DIRECTORY != 0;
                let is_reparse = attrs & 0x400 != 0;
                next_frn += 1;
                let frn = next_frn;
                let size = if is_dir {
                    0
                } else {
                    ((fd.nFileSizeHigh as u64) << 32) | fd.nFileSizeLow as u64
                };
                let ticks = |f: windows_sys::Win32::Foundation::FILETIME| -> i64 {
                    ((f.dwHighDateTime as i64) << 32) | f.dwLowDateTime as i64
                };
                out.push(
                    RawEntry::new(frn, dir_frn, name.clone(), attrs).with_stat(
                        size,
                        ticks(fd.ftLastWriteTime),
                        ticks(fd.ftCreationTime),
                        ticks(fd.ftLastAccessTime),
                    ),
                );
                if is_dir && !is_reparse {
                    let child = if dir.ends_with('\\') {
                        format!("{dir}{name}")
                    } else {
                        format!("{dir}\\{name}")
                    };
                    stack.push((child, frn));
                }
                counter.fetch_add(1, Ordering::Relaxed);
            }
            if unsafe { FindNextFileW(handle, &mut fd) } == 0 {
                break;
            }
        }
        unsafe {
            FindClose(handle);
        }
    }
    (out, next_frn)
}

// ------------------------------------------------------------------ scan

/// Capture the current journal position of each volume, to be stored with a
/// saved index so changes made while the app was closed can be replayed.
pub fn journal_marks(volumes: &[char]) -> Vec<(char, u64, i64)> {
    let mut out = Vec::new();
    for &letter in volumes {
        if let Ok(h) = open_volume(letter) {
            if let Ok(j) = query_journal(h.0) {
                out.push((letter, j.journal_id, j.next_usn));
            }
        }
    }
    out
}

/// True when a cached index was built from exactly these volumes.
pub fn cache_usable(index: &Index, volumes: &[char]) -> bool {
    if index.sources.len() != volumes.len() {
        return false;
    }
    index.sources.iter().all(|s| {
        s.chars()
            .next()
            .map(|c| c.to_ascii_uppercase())
            .is_some_and(|c| volumes.contains(&c))
    })
}

/// Replay USN journal records from the saved marks. Returns false when a mark
/// is unusable (journal recreated, volume missing, journal wrapped), in which
/// case the caller should do a full scan.
pub fn catch_up(index: &mut Index, marks: &[(char, u64, i64)]) -> bool {
    for &(letter, journal_id, usn) in marks {
        let Ok(h) = open_volume(letter) else {
            return false;
        };
        let Ok(j) = query_journal(h.0) else {
            return false;
        };
        if j.journal_id != journal_id || usn < j.first_usn || usn > j.next_usn {
            return false;
        }
        let mut cursor = usn;
        let mut upserts = Vec::new();
        let mut removes = Vec::new();
        let mut guard = 0;
        loop {
            guard += 1;
            if guard > 4096 {
                return false;
            }
            match read_changes(h.0, journal_id, cursor) {
                Ok((next, delta)) if next > cursor => {
                    cursor = next;
                    upserts.extend(delta.upserts);
                    removes.extend(delta.removes);
                }
                Ok(_) => break,
                Err(_) => return false,
            }
        }
        if !upserts.is_empty() || !removes.is_empty() {
            index.apply_delta(&upserts, &removes);
        }
    }
    true
}

#[derive(Debug, Clone, Default)]
pub struct ScanReport {
    pub errors: Vec<String>,
    pub modes: Vec<(char, &'static str)>,
    pub elapsed: Duration,
    pub entries: u64,
    pub folders_indexed: usize,
}

struct VolResult {
    letter: char,
    root: String,
    entries: Vec<RawEntry>,
    mode: &'static str,
    warning: Option<String>,
}

/// Build a complete index for `letters`, using the MFT where possible and a
/// directory walk otherwise.
pub fn scan_volumes(letters: &[char], counter: &AtomicU64) -> (Index, ScanReport) {
    let started = Instant::now();
    let mut report = ScanReport::default();
    let mut volumes: Vec<VolumeData> = Vec::new();

    let results: Vec<VolResult> = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for &letter in letters {
            let root = format!("{letter}:\\");
            handles.push((
                letter,
                root.clone(),
                scope.spawn(move || match enumerate_mft(letter, counter) {
                    Ok(v) => VolResult {
                        letter,
                        root: root.clone(),
                        entries: v,
                        mode: "mft",
                        warning: None,
                    },
                    Err(e) => {
                        let (v, _) = walk_folder(&root, counter);
                        VolResult {
                            letter,
                            root: root.clone(),
                            entries: v,
                            mode: "walk",
                            warning: Some(format!(
                                "{letter}: fast MFT scan unavailable ({e}); used folder scan. \
                                 Run as administrator for instant indexing."
                            )),
                        }
                    }
                }),
            ));
        }
        handles
            .into_iter()
            .map(|(letter, root, h)| {
                h.join().unwrap_or_else(|_| VolResult {
                    letter,
                    root,
                    entries: Vec::new(),
                    mode: "walk",
                    warning: Some("scanner thread panicked".into()),
                })
            })
            .collect()
    });

    for r in results {
        if let Some(w) = r.warning {
            report.errors.push(w);
        }
        report.entries += r.entries.len() as u64;
        report.modes.push((r.letter, r.mode));
        volumes.push(VolumeData {
            root: r.root,
            root_frn: 5,
            entries: r.entries,
        });
    }

    volumes.sort_by(|a, b| a.root.cmp(&b.root));
    let mut builder = IndexBuilder::default();
    for v in volumes {
        builder.add_volume(v);
    }
    let mut index = builder.build();
    index.refreshed_clock();
    report.elapsed = started.elapsed();
    report.folders_indexed = letters.len();
    (index, report)
}

/// Index a list of folders (Everything's "folder index" mode).
pub fn scan_folders(folders: &[String], counter: &AtomicU64) -> (Index, ScanReport) {
    let started = Instant::now();
    let mut report = ScanReport::default();
    let mut builder = IndexBuilder::default();
    for folder in folders {
        let (entries, _) = walk_folder(folder, counter);
        if entries.is_empty() {
            report
                .errors
                .push(format!("{folder}: nothing readable"));
            continue;
        }
        report.entries += entries.len() as u64;
        builder.add_volume(VolumeData {
            root: folder.clone(),
            root_frn: 5,
            entries,
        });
    }
    let mut index = builder.build();
    index.refreshed_clock();
    report.elapsed = started.elapsed();
    (index, report)
}

/// Fill the metadata cache in the background. `GetFileAttributesEx` is a single
/// syscall per file (no handle open), which keeps size/date filters exact
/// without blocking the UI thread.
pub fn prefetch_stats(
    index: &Index,
    threads: usize,
    cancel: &AtomicBool,
    progress: &AtomicU64,
) {
    let n = index.len();
    if n == 0 {
        return;
    }
    let threads = threads.clamp(1, 16).min(n);
    let chunk = n.div_ceil(threads);
    std::thread::scope(|scope| {
        for t in 0..threads {
            let start = t * chunk;
            let end = (start + chunk).min(n);
            scope.spawn(move || {
                for id in start..end {
                    if id & 0xFFF == 0 && cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    if index.entry(id as u32).is_deleted() {
                        continue;
                    }
                    if index.cached_stat(id as u32).is_none() {
                        index.ensure_stat(id as u32);
                        progress.fetch_add(1, Ordering::Relaxed);
                    }
                }
            });
        }
    });
}

/// Number of logical processors, used to size the metadata pool.
pub fn cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}
