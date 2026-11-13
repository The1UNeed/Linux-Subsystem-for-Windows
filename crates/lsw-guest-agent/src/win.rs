//! Windows glue: virtio-serial ports, clipboard, display, shares, shutdown.
//!
//! This is the only module with `unsafe` code. Every block is a direct
//! Win32 call whose preconditions are stated in its SAFETY comment.

use crate::core::{Agent, Out, Platform, PortId};
use crate::formats;
use lsw_proto::{
    CONTROL_PORT_NAME, ClipFormat, Envelope, FrameDecoder, KnownFolder, MAX_CLIPBOARD_TOTAL,
    SESSION_PORT_NAME, ShareSpec,
};
use std::collections::HashMap;
use std::io::Write as _;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_IO_PENDING, ERROR_NO_SYSTEM_RESOURCES, GetLastError,
    HANDLE, HGLOBAL, HWND, LPARAM, LRESULT, WAIT_OBJECT_0, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CDS_UPDATEREGISTRY, ChangeDisplaySettingsExW, DEVMODEW, DISP_CHANGE_SUCCESSFUL, DM_PELSHEIGHT,
    DM_PELSWIDTH, ENUM_DISPLAY_SETTINGS_MODE, EnumDisplaySettingsW,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ, FILE_GENERIC_WRITE, FILE_SHARE_NONE,
    OPEN_EXISTING, ReadFile, WriteFile,
};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardOwner, IsClipboardFormatAvailable, OpenClipboard, RegisterClipboardFormatW,
    SetClipboardData,
};
use windows::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use windows::Win32::System::Registry::{
    HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
};
use windows::Win32::System::Threading::{CreateEventW, CreateMutexW, WaitForSingleObject};
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};
use windows::Win32::UI::Shell::{
    FOLDERID_Desktop, FOLDERID_Documents, FOLDERID_Downloads, FOLDERID_Music, FOLDERID_Pictures,
    FOLDERID_Videos, KNOWN_FOLDER_FLAG, SHSetKnownFolderPath,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, HWND_MESSAGE, MSG,
    RegisterClassW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLIPBOARDUPDATE, WNDCLASSW,
};
use windows::core::{HSTRING, PCWSTR, w};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

/// Whether a port read error means "no host end connected" (vioserial).
pub(crate) fn is_host_absent(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(ERROR_NO_SYSTEM_RESOURCES.to_hresult().0)
}

/// Events delivered to the main loop.
enum Event {
    Frame(PortId, Envelope),
    PortLost(PortId),
    ClipboardChanged,
}

// ---------------------------------------------------------------- logging

fn log_path() -> PathBuf {
    let base = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
    Path::new(&base).join("LSW").join("agent.log")
}

pub fn log(msg: impl AsRef<str>) {
    static LOCK: Mutex<()> = Mutex::new(());
    let _g = LOCK.lock();
    let path = log_path();
    let _ = std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")));
    // Keep the log bounded: start over past 4 MiB.
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 4 << 20) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{secs} {}", msg.as_ref());
    }
}

// ------------------------------------------------------------------ ports

/// An open virtio-serial port (overlapped handle).
pub(crate) struct Port {
    handle: HANDLE,
}

// SAFETY: a file HANDLE may be used from any thread; overlapped reads and
// writes on it from different threads use separate OVERLAPPED structures.
unsafe impl Send for Port {}
// SAFETY: as above; the handle value itself is never mutated.
unsafe impl Sync for Port {}

impl Drop for Port {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateFileW and is closed only here.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// Owned event handle for one overlapped operation.
struct OwnedEvent(HANDLE);

impl OwnedEvent {
    fn new() -> std::io::Result<Self> {
        // SAFETY: manual-reset, initially unsignalled, unnamed event.
        let h = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }?;
        Ok(Self(h))
    }
}

impl Drop for OwnedEvent {
    fn drop(&mut self) {
        // SAFETY: the handle came from CreateEventW and is closed only here.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

impl Port {
    pub(crate) fn open(name: &str) -> std::io::Result<Self> {
        let path = HSTRING::from(format!("\\\\.\\Global\\{name}"));
        // SAFETY: valid NUL-terminated path; no security attributes or template.
        let handle = unsafe {
            CreateFileW(
                &path,
                (FILE_GENERIC_READ | FILE_GENERIC_WRITE).0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAG_OVERLAPPED,
                None,
            )
        }?;
        Ok(Self { handle })
    }

    /// Blocking read (waits for host data).
    pub(crate) fn read(&self, buf: &mut [u8]) -> std::io::Result<usize> {
        let event = OwnedEvent::new()?;
        let mut ov = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        let mut n = 0u32;
        // SAFETY: `buf` and `ov` outlive the operation: we wait for it to
        // complete (GetOverlappedResult with bWait) before returning.
        unsafe {
            if let Err(e) = ReadFile(self.handle, Some(buf), None, Some(&mut ov))
                && e.code() != ERROR_IO_PENDING.to_hresult()
            {
                return Err(e.into());
            }
            GetOverlappedResult(self.handle, &ov, &mut n, true)?;
        }
        Ok(n as usize)
    }

    /// Write everything or fail after [`WRITE_TIMEOUT`] (host not reading).
    pub(crate) fn write_all(&self, mut data: &[u8]) -> std::io::Result<()> {
        while !data.is_empty() {
            let event = OwnedEvent::new()?;
            let mut ov = OVERLAPPED {
                hEvent: event.0,
                ..Default::default()
            };
            let mut n = 0u32;
            // SAFETY: `data` and `ov` stay alive until the operation is
            // complete or cancelled-and-completed (GetOverlappedResult with
            // bWait after CancelIoEx).
            unsafe {
                if let Err(e) = WriteFile(self.handle, Some(data), None, Some(&mut ov)) {
                    if e.code() != ERROR_IO_PENDING.to_hresult() {
                        return Err(e.into());
                    }
                    let waited = WaitForSingleObject(event.0, WRITE_TIMEOUT.as_millis() as u32);
                    if waited != WAIT_OBJECT_0 {
                        let _ = CancelIoEx(self.handle, Some(&ov));
                        let _ = GetOverlappedResult(self.handle, &ov, &mut n, true);
                        return Err(std::io::ErrorKind::TimedOut.into());
                    }
                }
                GetOverlappedResult(self.handle, &ov, &mut n, true)?;
            }
            data = &data[n as usize..];
        }
        Ok(())
    }
}

type PortSlot = Arc<Mutex<Option<Arc<Port>>>>;

/// Keep `port` open and forward its frames until the process exits.
fn port_thread(id: PortId, name: &'static str, slot: PortSlot, tx: Sender<Event>) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let port = match Port::open(name) {
                Ok(p) => Arc::new(p),
                Err(e) => {
                    log(format!("{name}: open failed: {e}"));
                    std::thread::sleep(Duration::from_secs(3));
                    continue;
                }
            };
            log(format!("{name}: open"));
            *slot.lock().expect("slot") = Some(port.clone());
            let mut decoder = FrameDecoder::new();
            let result: Result<(), String> = loop {
                let n = match port.read(&mut buf) {
                    Ok(0) => {
                        // No host connected: the port reports EOF. Poll.
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                    Ok(n) => n,
                    // vioserial fails reads with ERROR_NO_SYSTEM_RESOURCES
                    // while no host end is connected: wait quietly.
                    Err(e)
                        if e.raw_os_error() == Some(ERROR_NO_SYSTEM_RESOURCES.to_hresult().0) =>
                    {
                        std::thread::sleep(Duration::from_millis(500));
                        continue;
                    }
                    Err(e) => break Err(e.to_string()),
                };
                if let Err(e) = decoder.push(&buf[..n]) {
                    break Err(e.to_string());
                }
                loop {
                    match decoder.next_envelope() {
                        Ok(Some(env)) => {
                            if tx.send(Event::Frame(id, env)).is_err() {
                                return;
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            // A confused stream: drop it and reopen.
                            decoder = FrameDecoder::new();
                            log(format!("{name}: bad frame: {e}"));
                            break;
                        }
                    }
                }
            };
            if let Err(e) = result {
                log(format!("{name}: {e}"));
            }
            *slot.lock().expect("slot") = None;
            let _ = tx.send(Event::PortLost(id));
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

// -------------------------------------------------------------- clipboard

static CLIPBOARD_TX: OnceLock<Mutex<Sender<Event>>> = OnceLock::new();

extern "system" fn clipboard_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        // SAFETY: plain query of the clipboard owner window.
        let owner = unsafe { GetClipboardOwner() }.unwrap_or_default();
        if owner != hwnd
            && let Some(tx) = CLIPBOARD_TX.get()
            && let Ok(t) = tx.lock()
        {
            let _ = t.send(Event::ClipboardChanged);
        }
        return LRESULT(0);
    }
    // SAFETY: default handling for every other message.
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

/// Create the clipboard listener window on its own thread; returns its
/// handle (used as clipboard owner, so our own writes are not echoed).
fn clipboard_thread(tx: Sender<Event>) -> isize {
    let _ = CLIPBOARD_TX.set(Mutex::new(tx));
    let (htx, hrx) = channel();
    std::thread::spawn(move || {
        // SAFETY: standard message-only window creation and message loop on
        // this thread; the class name and window procedure are 'static.
        unsafe {
            let instance = GetModuleHandleW(None).unwrap_or_default();
            let class = WNDCLASSW {
                lpfnWndProc: Some(clipboard_wndproc),
                hInstance: instance.into(),
                lpszClassName: w!("LswClipboard"),
                ..Default::default()
            };
            RegisterClassW(&class);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("LswClipboard"),
                w!("LSW"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                Some(instance.into()),
                None,
            );
            let Ok(hwnd) = hwnd else {
                log("clipboard window creation failed");
                let _ = htx.send(0isize);
                return;
            };
            if let Err(e) = AddClipboardFormatListener(hwnd) {
                log(format!("AddClipboardFormatListener: {e}"));
            }
            let _ = htx.send(hwnd.0 as isize);
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                DispatchMessageW(&msg);
            }
        }
    });
    hrx.recv().unwrap_or(0)
}

struct ClipboardGuard;

impl ClipboardGuard {
    fn open(owner: HWND) -> Option<Self> {
        for _ in 0..20 {
            // SAFETY: OpenClipboard with our own window; closed in Drop.
            if unsafe { OpenClipboard(Some(owner)) }.is_ok() {
                return Some(Self);
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        None
    }

    fn has(&self, format: u32) -> bool {
        // SAFETY: the clipboard is open.
        unsafe { IsClipboardFormatAvailable(format) }.is_ok()
    }

    fn read(&self, format: u32) -> Option<Vec<u8>> {
        // SAFETY: the clipboard is open; the global memory is locked only
        // for the copy and its size bounds the slice.
        unsafe {
            let h = GetClipboardData(format).ok()?;
            let mem = HGLOBAL(h.0);
            let size = GlobalSize(mem);
            if size == 0 || size as u64 > MAX_CLIPBOARD_TOTAL * 2 {
                return None;
            }
            let ptr = GlobalLock(mem) as *const u8;
            if ptr.is_null() {
                return None;
            }
            let data = std::slice::from_raw_parts(ptr, size).to_vec();
            let _ = GlobalUnlock(mem);
            Some(data)
        }
    }

    fn write(&self, format: u32, data: &[u8]) -> Result<(), String> {
        // SAFETY: the clipboard is open and emptied by the caller; on
        // success the system owns the allocation, on failure it leaks a
        // small block, which is the documented contract.
        unsafe {
            let mem = GlobalAlloc(GMEM_MOVEABLE, data.len().max(1)).map_err(|e| e.to_string())?;
            let ptr = GlobalLock(mem) as *mut u8;
            if ptr.is_null() {
                return Err("GlobalLock failed".into());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            let _ = GlobalUnlock(mem);
            SetClipboardData(format, Some(HANDLE(mem.0))).map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        // SAFETY: we opened the clipboard.
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

fn from_utf16z(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

// ---------------------------------------------------------------- platform

struct Windows {
    clipboard_owner: HWND,
    cf_html: u32,
    cf_png: u32,
    /// Mounted shares: our virtiofs.exe (None for a mount adopted from a
    /// previous agent) and the mount path.
    shares: HashMap<String, (Option<Child>, String)>,
    /// The IANA time zone last applied.
    time_zone: Option<String>,
}

fn reg_dword(key: &str, value: &str) -> Option<u32> {
    let mut data = 0u32;
    let mut size = 4u32;
    // SAFETY: data/size describe a valid 4-byte buffer.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(key),
            &HSTRING::from(value),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut data).cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    Some(data)
}

fn reg_string(key: &str, value: &str) -> Option<String> {
    let mut buf = [0u16; 256];
    let mut size = (buf.len() * 2) as u32;
    // SAFETY: buf/size describe a valid buffer; RegGetValueW NUL-terminates.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            &HSTRING::from(key),
            &HSTRING::from(value),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr().cast()),
            Some(&mut size),
        )
        .ok()
        .ok()?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

fn known_folder_name(folder: KnownFolder) -> &'static str {
    match folder {
        KnownFolder::Desktop => "Desktop",
        KnownFolder::Documents => "Documents",
        KnownFolder::Downloads => "Downloads",
        KnownFolder::Pictures => "Pictures",
        KnownFolder::Music => "Music",
        KnownFolder::Videos => "Videos",
    }
}

fn drive_letters() -> Vec<char> {
    ('D'..='Z')
        .filter(|c| Path::new(&format!("{c}:\\")).exists())
        .collect()
}

/// Whether `dir` is a mount (a reparse point), not a plain folder.
fn is_mount_point(dir: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    std::fs::symlink_metadata(dir)
        .is_ok_and(|m| m.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// A working mount: WinFsp's reparse point, readable.
fn is_live_mount(dir: &Path) -> bool {
    is_mount_point(dir) && std::fs::read_dir(dir).is_ok()
}

/// WinFsp creates the mount directory itself. Remove a stale mount point or
/// a folder with nothing but shell metadata (Explorer recreates a missing
/// Desktop with a desktop.ini); move a folder with real files aside (they
/// were written while the share was missing) instead of hiding or deleting
/// them.
fn clear_mount_dir(dir: &Path) {
    let name = dir
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    // Earlier copies that turned out to hold only shell metadata.
    if let Some(parent) = dir.parent()
        && let Ok(entries) = std::fs::read_dir(parent)
    {
        let prefix = format!("{name} (local copy ");
        for e in entries.flatten() {
            if e.file_name().to_string_lossy().starts_with(&prefix) {
                remove_if_only_metadata(&e.path());
            }
        }
    }
    if !dir.exists() && !is_mount_point(dir) {
        return;
    }
    if is_mount_point(dir) {
        let _ = std::fs::remove_dir(dir);
        return;
    }
    if remove_if_only_metadata(dir) {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    let aside = dir.with_file_name(format!("{name} (local copy {stamp})"));
    match std::fs::rename(dir, &aside) {
        Ok(()) => log(format!(
            "{}: kept files written while the share was missing in {}",
            dir.display(),
            aside.display()
        )),
        Err(e) => log(format!(
            "{}: cannot move a stale folder aside: {e}",
            dir.display()
        )),
    }
}

/// Remove `dir` if it holds nothing but shell metadata. Explorer may hold
/// the folder open for a moment; if it cannot be removed, it is moved out of
/// the way into %TEMP%. Returns whether `dir` is gone.
fn remove_if_only_metadata(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let names: Vec<_> = entries.flatten().map(|e| e.path()).collect();
    if !names.iter().all(|p| crate::core::is_shell_metadata(p)) {
        return false;
    }
    for p in &names {
        if let Ok(meta) = std::fs::metadata(p) {
            let mut perms = meta.permissions();
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = std::fs::set_permissions(p, perms);
        }
        let _ = std::fs::remove_file(p);
    }
    if wait_until(Duration::from_secs(2), || std::fs::remove_dir(dir).is_ok()) {
        return true;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let trash = std::env::temp_dir().join(format!("lsw-empty-folder-{stamp}"));
    if std::fs::rename(dir, &trash).is_ok() {
        let _ = std::fs::remove_dir(&trash);
        return true;
    }
    false
}

pub(crate) fn wait_until(timeout: Duration, mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if ready() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    false
}

impl Windows {
    fn spawn_virtiofs(tag: &str, mount_point: &str) -> Result<Child, String> {
        let program_files =
            std::env::var_os("ProgramFiles").unwrap_or_else(|| "C:\\Program Files".into());
        let exe = Path::new(&program_files).join("Virtio-Win\\VioFS\\virtiofs.exe");
        if !exe.exists() {
            return Err(format!("{} not found", exe.display()));
        }
        Command::new(exe)
            .args(["-t", tag, "-m", mount_point, "-F", "LSW"])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("virtiofs.exe: {e}"))
    }

    fn mount(&mut self, share: &ShareSpec) -> Result<String, String> {
        if let Some((child, path)) = self.shares.get_mut(&share.tag) {
            let alive = match child {
                Some(c) => c.try_wait().ok().flatten().is_none(),
                None => is_live_mount(Path::new(path.as_str())),
            };
            if alive {
                return Ok(path.clone());
            }
        }
        let profile = std::env::var_os("USERPROFILE").ok_or("USERPROFILE is not set")?;
        let parent = Path::new(&profile).join("Linux");
        std::fs::create_dir_all(&parent).map_err(|e| e.to_string())?;
        let dir = parent.join(known_folder_name(share.folder));
        let dir_str = dir.to_string_lossy().into_owned();
        // A previous agent (before a restart or an update) left this share
        // mounted: keep it. Remounting would make the folder vanish for a
        // moment, which Explorer reports as an error.
        if is_live_mount(&dir) {
            return Ok(self.finish(share, None, dir_str));
        }
        clear_mount_dir(&dir);

        // Directory mount point first, then a free drive letter. Mounted
        // means WinFsp's reparse point is there and virtiofs.exe still runs:
        // a plain folder of that name is not the share.
        let mut child = Self::spawn_virtiofs(&share.tag, &dir_str)?;
        if wait_until(Duration::from_secs(10), || {
            child.try_wait().ok().flatten().is_none() && is_mount_point(&dir)
        }) && std::fs::read_dir(&dir).is_ok()
        {
            return Ok(self.finish(share, Some(child), dir_str));
        }
        let _ = child.kill();
        log(format!(
            "{}: directory mount failed; using a drive letter",
            share.tag
        ));
        let before = drive_letters();
        let child = Self::spawn_virtiofs(&share.tag, "*")?;
        let mut letter = None;
        wait_until(Duration::from_secs(10), || {
            letter = drive_letters().into_iter().find(|c| !before.contains(c));
            letter.is_some()
        });
        match letter {
            Some(c) => Ok(self.finish(share, Some(child), format!("{c}:\\"))),
            None => {
                let mut child = child;
                let _ = child.kill();
                Err("virtiofs mount did not appear (is WinFsp installed?)".into())
            }
        }
    }

    fn finish(&mut self, share: &ShareSpec, child: Option<Child>, path: String) -> String {
        let id = match share.folder {
            KnownFolder::Desktop => &FOLDERID_Desktop,
            KnownFolder::Documents => &FOLDERID_Documents,
            KnownFolder::Downloads => &FOLDERID_Downloads,
            KnownFolder::Pictures => &FOLDERID_Pictures,
            KnownFolder::Music => &FOLDERID_Music,
            KnownFolder::Videos => &FOLDERID_Videos,
        };
        // SAFETY: valid known-folder id and NUL-terminated path.
        let r = unsafe {
            SHSetKnownFolderPath(
                id,
                KNOWN_FOLDER_FLAG(0).0 as u32,
                None,
                &HSTRING::from(&path),
            )
        };
        if let Err(e) = r {
            log(format!("{}: SHSetKnownFolderPath: {e}", share.tag));
        }
        self.shares.insert(share.tag.clone(), (child, path.clone()));
        path
    }
}

impl Windows {
    fn change_display(&self, width: u32, height: u32, scale_percent: u16) -> Result<(), String> {
        // The host announces the size through virtio-gpu (EDID + display
        // event); give the driver a moment to list it as a mode. viogpudo
        // reads the EDID at boot, so the boot size is always available.
        let exact = wait_until(Duration::from_secs(3), || {
            display_modes().contains(&(width, height))
        });
        let (width, height) = if exact {
            (width, height)
        } else {
            // Not listed: the largest mode that fits (the host letterboxes).
            crate::core::best_fitting_mode(&display_modes(), width, height)
                .ok_or_else(|| format!("no display mode fits {width}x{height}"))?
        };
        let mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            dmFields: DM_PELSWIDTH | DM_PELSHEIGHT,
            dmPelsWidth: width,
            dmPelsHeight: height,
            ..Default::default()
        };
        // SAFETY: `mode` is a fully initialized DEVMODEW for the primary display.
        let r = unsafe {
            ChangeDisplaySettingsExW(PCWSTR::null(), Some(&mode), None, CDS_UPDATEREGISTRY, None)
        };
        if scale_percent != 100 {
            log(format!(
                "display scale {scale_percent}% requested; not applied in this version"
            ));
        }
        if r == DISP_CHANGE_SUCCESSFUL {
            Ok(())
        } else {
            Err(format!(
                "ChangeDisplaySettingsEx({width}x{height}) returned {}",
                r.0
            ))
        }
    }
}

/// Resolutions the primary display offers.
fn display_modes() -> Vec<(u32, u32)> {
    let mut modes = Vec::new();
    for i in 0..1024 {
        let mut mode = DEVMODEW {
            dmSize: std::mem::size_of::<DEVMODEW>() as u16,
            ..Default::default()
        };
        // SAFETY: `mode` is a valid DEVMODEW with dmSize set.
        if !unsafe {
            EnumDisplaySettingsW(PCWSTR::null(), ENUM_DISPLAY_SETTINGS_MODE(i), &mut mode)
        }
        .as_bool()
        {
            break;
        }
        modes.push((mode.dmPelsWidth, mode.dmPelsHeight));
    }
    modes.sort_unstable();
    modes.dedup();
    modes
}

impl Platform for Windows {
    fn os_build(&self) -> String {
        let key = "SOFTWARE\\Microsoft\\Windows NT\\CurrentVersion";
        format!(
            "{}.{}.{}.{}",
            reg_dword(key, "CurrentMajorVersionNumber").unwrap_or(10),
            reg_dword(key, "CurrentMinorVersionNumber").unwrap_or(0),
            reg_string(key, "CurrentBuildNumber").unwrap_or_default(),
            reg_dword(key, "UBR").unwrap_or(0)
        )
    }

    fn shutdown(&mut self, reboot: bool) -> Result<(), String> {
        let mode = if reboot { "/r" } else { "/s" };
        Command::new("shutdown.exe")
            .args([mode, "/t", "0"])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    fn mount_share(&mut self, share: &ShareSpec) -> Result<String, String> {
        let r = self.mount(share);
        log(format!("share {}: {r:?}", share.tag));
        r
    }

    fn set_display(&mut self, width: u32, height: u32, scale_percent: u16) -> Result<(), String> {
        let r = self.change_display(width, height, scale_percent);
        log(format!("display {width}x{height} @{scale_percent}%: {r:?}"));
        r
    }

    fn list_apps(&mut self) -> Vec<lsw_proto::AppEntry> {
        let apps = crate::apps::list();
        log(format!("listed {} apps", apps.len()));
        apps
    }

    fn launch(&mut self, launch: &str) -> Result<(), String> {
        log(format!("launch {launch}"));
        crate::apps::launch(launch)
    }

    fn set_clock(&mut self, utc_ms: i64, time_zone: &str) -> Result<(), String> {
        // Each part on its own: a failing clock must not keep the zone.
        let clock = crate::clock::set_system_time(utc_ms);
        let zone = if !time_zone.is_empty() && self.time_zone.as_deref() != Some(time_zone) {
            crate::clock::set_time_zone(time_zone).map(|()| {
                self.time_zone = Some(time_zone.to_owned());
            })
        } else {
            Ok(())
        };
        let errors: Vec<String> = [clock, zone].into_iter().filter_map(Result::err).collect();
        if errors.is_empty() {
            Ok(())
        } else {
            log(format!("clock: {}", errors.join("; ")));
            Err(errors.join("; "))
        }
    }

    fn clipboard_formats(&mut self) -> Vec<ClipFormat> {
        let Some(cb) = ClipboardGuard::open(self.clipboard_owner) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if cb.has(u32::from(CF_UNICODETEXT.0)) {
            out.push(ClipFormat::Text);
        }
        if self.cf_html != 0 && cb.has(self.cf_html) {
            out.push(ClipFormat::Html);
        }
        if (self.cf_png != 0 && cb.has(self.cf_png)) || cb.has(u32::from(CF_DIB.0)) {
            out.push(ClipFormat::Png);
        }
        out
    }

    fn clipboard_read(&mut self, format: ClipFormat) -> Option<Vec<u8>> {
        let cb = ClipboardGuard::open(self.clipboard_owner)?;
        match format {
            ClipFormat::Text => cb
                .read(u32::from(CF_UNICODETEXT.0))
                .map(|d| from_utf16z(&d).into_bytes()),
            ClipFormat::Html => cb
                .read(self.cf_html)
                .and_then(|d| formats::cf_html_fragment(&d))
                .map(String::into_bytes),
            ClipFormat::Png => {
                if self.cf_png != 0
                    && let Some(png) = cb.read(self.cf_png)
                {
                    return Some(png);
                }
                cb.read(u32::from(CF_DIB.0))
                    .and_then(|d| formats::dib_to_png(&d))
            }
        }
    }

    fn clipboard_write(&mut self, items: &[(ClipFormat, Vec<u8>)]) -> Result<(), String> {
        let cb = ClipboardGuard::open(self.clipboard_owner).ok_or("clipboard busy")?;
        // SAFETY: the clipboard is open by us.
        unsafe { EmptyClipboard() }.map_err(|e| e.to_string())?;
        for (format, data) in items {
            match format {
                ClipFormat::Text => {
                    let text = String::from_utf8_lossy(data);
                    cb.write(u32::from(CF_UNICODETEXT.0), &utf16z(&text))?;
                }
                ClipFormat::Html => {
                    let html = String::from_utf8_lossy(data);
                    cb.write(self.cf_html, &formats::cf_html_wrap(&html))?;
                }
                ClipFormat::Png => {
                    if self.cf_png != 0 {
                        cb.write(self.cf_png, data)?;
                    }
                    if let Some(dib) = formats::png_to_dib(data) {
                        cb.write(u32::from(CF_DIB.0), &dib)?;
                    }
                }
            }
        }
        Ok(())
    }
}

// ------------------------------------------------------------ self-update

/// If the LSW Tools CD carries another agent build, or WinFsp is missing
/// and the CD carries it, start its installer (which stops this process
/// and replaces it) and return true.
fn self_update() -> bool {
    let Ok(exe_path) = std::env::current_exe() else {
        return false;
    };
    let Ok(exe) = std::fs::read(&exe_path) else {
        return false;
    };
    let own = lsw_proto::agent_build_id(crate::core::AGENT_VERSION, &exe);
    let Some(drive) = ('D'..='Z').find(|c| {
        Path::new(&format!("{c}:\\lsw\\install-agent.ps1")).exists()
            && Path::new(&format!("{c}:\\lsw\\version.txt")).exists()
    }) else {
        return false;
    };
    let media = format!("{drive}:");
    let cd = std::fs::read_to_string(format!("{media}\\lsw\\version.txt")).unwrap_or_default();
    let marker = log_path().with_file_name("update-attempt.txt");
    let last = std::fs::read_to_string(&marker).ok();
    // A guest installed without WinFsp is repaired once a CD carries it.
    let program_files_x86 =
        std::env::var_os("ProgramFiles(x86)").unwrap_or_else(|| "C:\\Program Files (x86)".into());
    let winfsp_missing = !Path::new(&program_files_x86)
        .join("WinFsp\\bin\\winfsp-x64.dll")
        .exists()
        && Path::new(&format!("{media}\\lsw\\winfsp.msi")).exists();
    let Some(target) = crate::core::install_target(&own, &cd, last.as_deref(), winfsp_missing)
    else {
        return false;
    };
    log(format!(
        "installing LSW Tools {target} from {media} (agent {own})"
    ));
    let _ = std::fs::write(&marker, &target);
    let script = format!("{media}\\lsw\\install-agent.ps1");
    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
            &script,
            "-Media",
            &media,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .is_ok()
}

// -------------------------------------------------------------------- run

pub fn run() -> i32 {
    // One agent per session.
    // SAFETY: named mutex creation; the handle lives for the process.
    let _mutex = unsafe { CreateMutexW(None, true, w!("Local\\LswAgent")) };
    // SAFETY: reads the thread's last-error value.
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return 0;
    }
    // SAFETY: process-wide setting, called before any window exists.
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    log(format!("lsw-agent {} starting", crate::core::AGENT_VERSION));
    if self_update() {
        return 0;
    }

    let (tx, rx) = channel();
    let owner = HWND(clipboard_thread(tx.clone()) as *mut _);
    // SAFETY: registering well-known clipboard format names.
    let (cf_html, cf_png) = unsafe {
        (
            RegisterClipboardFormatW(w!("HTML Format")),
            RegisterClipboardFormatW(w!("PNG")),
        )
    };
    let platform = Windows {
        clipboard_owner: owner,
        cf_html,
        cf_png,
        shares: HashMap::new(),
        time_zone: None,
    };
    let mut agent = Agent::new(platform);

    let slots: [PortSlot; 2] = Default::default();
    port_thread(
        PortId::Control,
        CONTROL_PORT_NAME,
        slots[0].clone(),
        tx.clone(),
    );
    port_thread(
        PortId::Session,
        SESSION_PORT_NAME,
        slots[1].clone(),
        tx.clone(),
    );
    drop(tx);
    crate::coherence::spawn();

    let mut seq = 1u64;
    for event in rx {
        let outs: Vec<Out> = match event {
            Event::Frame(port, env) => agent.on_message(port, env),
            Event::PortLost(port) => {
                agent.port_lost(port);
                Vec::new()
            }
            Event::ClipboardChanged => agent.local_clipboard_changed(),
        };
        for out in outs {
            if !agent.attached(out.port) {
                continue;
            }
            let env = Envelope {
                seq,
                reply_to: out.reply_to,
                msg: out.msg,
            };
            seq += 1;
            let slot = &slots[match out.port {
                PortId::Control => 0,
                PortId::Session => 1,
            }];
            let port = slot.lock().expect("slot").clone();
            let sent = match (port, lsw_proto::encode(&env)) {
                (Some(p), Ok(frame)) => p.write_all(&frame).map_err(|e| e.to_string()),
                (None, _) => Err("port closed".into()),
                (_, Err(e)) => Err(e.to_string()),
            };
            if let Err(e) = sent {
                log(format!(
                    "{:?}: send failed: {e}; waiting for the host to attach again",
                    out.port
                ));
                agent.port_lost(out.port);
            }
        }
    }
    0
}
