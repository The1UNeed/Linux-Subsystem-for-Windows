//! Coherence engine (Windows): tracks top-level windows, captures each with
//! Windows.Graphics.Capture and streams damaged tiles to the host over the
//! Coherence port; applies the host's activate/resize/close requests.
//!
//! `unsafe` here is Win32/WinRT FFI only; each block states its contract.

use crate::win::{Port, is_host_absent, log};
use lsw_proto::coherence::{
    COHERENCE_PORT_NAME, CoMessage, HitArea, Rect, WindowInfo, WindowKind, WindowState,
    damaged_tiles,
};
use lsw_proto::{Decoder, encode_message};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOTOWNER, GW_OWNER, GWL_EXSTYLE, GWL_STYLE, GetAncestor, GetClassNameW,
    GetForegroundWindow, GetWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, HTBOTTOM, HTBOTTOMLEFT, HTBOTTOMRIGHT, HTCAPTION, HTCLOSE, HTHELP,
    HTLEFT, HTMAXBUTTON, HTMINBUTTON, HTRIGHT, HTSYSMENU, HTTOP, HTTOPLEFT, HTTOPRIGHT, HWND_TOP,
    IsIconic, IsWindow, IsWindowVisible, IsZoomed, PostMessageW, SMTO_ABORTIFHUNG, SW_MINIMIZE,
    SW_RESTORE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOZORDER, SendMessageTimeoutW, SetForegroundWindow,
    SetWindowPos, ShowWindowAsync, SwitchToThisWindow, WM_CLOSE, WM_NCHITTEST, WS_CAPTION,
    WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};
use windows::core::{BOOL, Interface};

/// Shell windows that are never mirrored (desktop, taskbars, Start).
const SHELL_CLASSES: &[&str] = &[
    "Progman",
    "WorkerW",
    "Shell_TrayWnd",
    "Shell_SecondaryTrayWnd",
    "Windows.UI.Core.CoreWindow",
    "LswClipboard",
];

/// Classes that are popups of their owner (menus, tooltips, dropdowns).
const POPUP_CLASSES: &[&str] = &["#32768", "tooltips_class32", "ComboLBox", "DropDown"];

enum Input {
    Host(CoMessage),
    Lost,
}

/// Start the Coherence port and engine threads.
pub fn spawn() {
    std::thread::Builder::new()
        .name("coherence".into())
        .spawn(run)
        .ok();
}

fn run() {
    loop {
        let port = match Port::open(COHERENCE_PORT_NAME) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                log(format!("coherence: open failed: {e}"));
                std::thread::sleep(Duration::from_secs(3));
                continue;
            }
        };
        let (tx, rx) = channel();
        reader(port.clone(), tx);
        engine(&port, &rx);
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn reader(port: Arc<Port>, tx: Sender<Input>) {
    std::thread::spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        let mut decoder = Decoder::<CoMessage>::new();
        loop {
            let n = match port.read(&mut buf) {
                Ok(0) => {
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
                Ok(n) => n,
                Err(e) if is_host_absent(&e) => {
                    std::thread::sleep(Duration::from_millis(500));
                    continue;
                }
                Err(e) => {
                    log(format!("coherence: read: {e}"));
                    let _ = tx.send(Input::Lost);
                    return;
                }
            };
            if decoder.push(&buf[..n]).is_err() {
                decoder = Decoder::new();
                continue;
            }
            while let Ok(Some(msg)) = decoder.next_message() {
                if tx.send(Input::Host(msg)).is_err() {
                    return;
                }
            }
        }
    });
}

/// Serialized, mutex-guarded writer (frames and replies share the port).
struct Writer<'a> {
    port: &'a Port,
    lock: Mutex<()>,
}

impl Writer<'_> {
    fn send(&self, msg: &CoMessage) -> bool {
        let Ok(frame) = encode_message(msg) else {
            return true; // invalid message: drop it, keep the channel
        };
        let _g = self.lock.lock();
        self.port.write_all(&frame).is_ok()
    }
}

// ---------------------------------------------------------- window model

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tracked {
    info: WindowInfo,
}

fn hwnd_of(id: u64) -> HWND {
    HWND(id as usize as *mut _)
}

fn class_name(hwnd: HWND) -> String {
    let mut buf = [0u16; 128];
    // SAFETY: valid buffer; returns the number of characters copied.
    let n = unsafe { GetClassNameW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn title(hwnd: HWND) -> String {
    let mut buf = [0u16; 512];
    // SAFETY: valid buffer.
    let n = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn exe_path(hwnd: HWND) -> String {
    let mut pid = 0u32;
    // SAFETY: pid is a valid out pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    // SAFETY: limited-information handle; closed by the HANDLE's owner below.
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return String::new();
    };
    let mut buf = [0u16; 1024];
    let mut len = buf.len() as u32;
    // SAFETY: buf/len describe a valid buffer.
    let ok = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_ok();
    // SAFETY: we own the process handle.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(process);
    }
    if ok {
        String::from_utf16_lossy(&buf[..len as usize])
    } else {
        String::new()
    }
}

/// Visible bounds (without the invisible resize borders).
fn visible_rect(hwnd: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    // SAFETY: r is a valid RECT out parameter of the right size.
    unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            (&raw mut r).cast(),
            std::mem::size_of::<RECT>() as u32,
        )
    }
    .ok()?;
    Some(r)
}

fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    // SAFETY: valid u32 out parameter.
    unsafe { DwmGetWindowAttribute(hwnd, DWMWA_CLOAKED, (&raw mut cloaked).cast(), 4) }.is_ok()
        && cloaked != 0
}

extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    // SAFETY: lparam is the &mut Vec<HWND> passed by `top_level_windows`,
    // valid for the duration of EnumWindows.
    let list = unsafe { &mut *(lparam.0 as *mut Vec<HWND>) };
    list.push(hwnd);
    BOOL::from(true)
}

fn top_level_windows() -> Vec<HWND> {
    let mut list: Vec<HWND> = Vec::new();
    // SAFETY: the callback only pushes into `list`, which outlives the call.
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM((&raw mut list) as isize));
    }
    list
}

/// Describe `hwnd` if it should be mirrored.
fn describe(hwnd: HWND) -> Option<WindowInfo> {
    // SAFETY: plain queries on a window handle.
    let (visible, ex, style, owner, iconic, zoomed) = unsafe {
        (
            IsWindowVisible(hwnd).as_bool(),
            GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32,
            GetWindowLongPtrW(hwnd, GWL_STYLE) as u32,
            GetWindow(hwnd, GW_OWNER).ok(),
            IsIconic(hwnd).as_bool(),
            IsZoomed(hwnd).as_bool(),
        )
    };
    if !visible || is_cloaked(hwnd) {
        return None;
    }
    let class = class_name(hwnd);
    if SHELL_CLASSES.contains(&class.as_str()) {
        return None;
    }
    let tool = ex & WS_EX_TOOLWINDOW.0 != 0;
    let popup_style = style & WS_POPUP.0 != 0;
    let caption = style & WS_CAPTION.0 == WS_CAPTION.0;
    let kind = if POPUP_CLASSES.contains(&class.as_str())
        || (popup_style && (tool || (owner.is_some() && !caption)))
    {
        WindowKind::Popup
    } else if owner.is_some() && ex & WS_EX_APPWINDOW.0 == 0 {
        WindowKind::Dialog
    } else if tool || (ex & WS_EX_NOACTIVATE.0 != 0 && style & WS_CAPTION.0 == 0) {
        return None; // floating tool palettes, notification balloons
    } else {
        WindowKind::Normal
    };
    let r = visible_rect(hwnd)?;
    let (w, h) = (
        (r.right - r.left).max(0) as u32,
        (r.bottom - r.top).max(0) as u32,
    );
    if !iconic && (w < 2 || h < 2) {
        return None;
    }
    // Popups without an owner (menus) belong to the foreground window's
    // root owner.
    let owner_id = owner
        .or_else(|| {
            (kind == WindowKind::Popup).then(|| {
                // SAFETY: plain queries.
                unsafe { GetAncestor(GetForegroundWindow(), GA_ROOTOWNER) }
            })
        })
        .filter(|o| !o.is_invalid() && *o != hwnd)
        .map(|o| o.0 as usize as u64);
    Some(WindowInfo {
        id: hwnd.0 as usize as u64,
        owner: owner_id,
        kind,
        title: title(hwnd),
        exe: exe_path(hwnd),
        rect: Rect {
            x: r.left,
            y: r.top,
            width: w,
            height: h,
        },
        state: if iconic {
            WindowState::Minimized
        } else if zoomed {
            WindowState::Maximized
        } else {
            WindowState::Normal
        },
    })
}

// --------------------------------------------------------------- capture

struct D3d {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    winrt: IDirect3DDevice,
}

fn d3d() -> windows::core::Result<D3d> {
    let mut last = None;
    // Hardware first (tier A/B/C), then WARP (tier D has no 3D driver).
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let mut device = None;
        let mut context = None;
        // SAFETY: out parameters are valid Options; no feature level list.
        let r = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        match (r, device, context) {
            (Ok(()), Some(device), Some(context)) => {
                let dxgi: IDXGIDevice = device.cast()?;
                // SAFETY: valid DXGI device.
                let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
                return Ok(D3d {
                    device,
                    context,
                    winrt: inspectable.cast()?,
                });
            }
            (Err(e), _, _) => last = Some(e),
            _ => {}
        }
    }
    Err(last.unwrap_or_else(windows::core::Error::empty))
}

struct Capture {
    pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    size: SizeInt32,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    previous: Option<(u32, u32, Vec<u8>)>,
}

impl Capture {
    fn start(d3d: &D3d, hwnd: HWND) -> windows::core::Result<Self> {
        let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        // SAFETY: valid top-level window handle.
        let item: GraphicsCaptureItem = unsafe { interop.CreateForWindow(hwnd)? };
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &d3d.winrt,
            DirectXPixelFormat::B8G8R8A8UIntNormalized,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        // Windows 11: no yellow capture border, no cursor in the frames.
        let _ = session.SetIsBorderRequired(false);
        let _ = session.SetIsCursorCaptureEnabled(false);
        session.StartCapture()?;
        Ok(Self {
            pool,
            session,
            size,
            staging: None,
            previous: None,
        })
    }

    /// The newest frame as tightly packed BGRA, if the window changed.
    fn poll(&mut self, d3d: &D3d) -> windows::core::Result<Option<(u32, u32, Vec<u8>)>> {
        let mut newest = None;
        while let Ok(frame) = self.pool.TryGetNextFrame() {
            newest = Some(frame);
        }
        let Some(frame) = newest else {
            return Ok(None);
        };
        let content = frame.ContentSize()?;
        if content.Width != self.size.Width || content.Height != self.size.Height {
            self.size = content;
            self.pool.Recreate(
                &d3d.winrt,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                2,
                content,
            )?;
        }
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        // SAFETY: the surface is a D3D11 texture on our device.
        let texture: ID3D11Texture2D = unsafe { access.GetInterface()? };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: valid texture and out parameter.
        unsafe { texture.GetDesc(&mut desc) };
        let (w, h) = (
            (content.Width.max(1) as u32).min(desc.Width),
            (content.Height.max(1) as u32).min(desc.Height),
        );
        if self
            .staging
            .as_ref()
            .is_none_or(|(_, sw, sh)| (*sw, *sh) != (desc.Width, desc.Height))
        {
            let sdesc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                ..desc
            };
            let mut staging = None;
            // SAFETY: valid description and out parameter.
            unsafe {
                d3d.device
                    .CreateTexture2D(&sdesc, None, Some(&mut staging))?
            };
            self.staging = staging.map(|t| (t, desc.Width, desc.Height));
        }
        let Some((staging, _, _)) = &self.staging else {
            return Ok(None);
        };
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        // SAFETY: both textures have identical size and format; the mapped
        // pointer is valid for RowPitch * Height bytes until Unmap.
        unsafe {
            d3d.context.CopyResource(staging, &texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            d3d.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let base = mapped.pData as *const u8;
            for row in 0..h as usize {
                let line = std::slice::from_raw_parts(
                    base.add(row * mapped.RowPitch as usize),
                    w as usize * 4,
                );
                pixels.extend_from_slice(line);
            }
            d3d.context.Unmap(staging, 0);
        }
        Ok(Some((w, h, pixels)))
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        let _ = self.session.Close();
        let _ = self.pool.Close();
    }
}

// ------------------------------------------------------- host requests

/// Bring a window to the front. Windows only lets the foreground process
/// change the foreground window; `SwitchToThisWindow` is the Alt+Tab path
/// and works from the agent. (A synthetic Alt tap would also lift the lock,
/// but it puts Windows into menu/access-key mode and eats the next click.)
fn activate(hwnd: HWND) {
    // SAFETY: plain window-manager calls on a handle we track.
    unsafe {
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindowAsync(hwnd, SW_RESTORE);
        }
        SwitchToThisWindow(hwnd, true);
        if GetForegroundWindow() != hwnd {
            let _ = SetForegroundWindow(hwnd);
        }
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Resize so the *visible* bounds become `width`×`height`.
fn resize(hwnd: HWND, width: u32, height: u32) {
    let mut outer = RECT::default();
    // SAFETY: valid out parameter.
    if unsafe { GetWindowRect(hwnd, &mut outer) }.is_err() {
        return;
    }
    let Some(visible) = visible_rect(hwnd) else {
        return;
    };
    let extra_w = (outer.right - outer.left) - (visible.right - visible.left);
    let extra_h = (outer.bottom - outer.top) - (visible.bottom - visible.top);
    // SAFETY: plain window-manager call.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            width as i32 + extra_w,
            height as i32 + extra_h,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

fn hit_test(hwnd: HWND, x: i32, y: i32) -> HitArea {
    let Some(r) = visible_rect(hwnd) else {
        return HitArea::Client;
    };
    let (sx, sy) = (r.left + x, r.top + y);
    let lparam = LPARAM(((sy as u16 as u32) << 16 | (sx as u16 as u32)) as isize);
    let mut result = 0usize;
    // SAFETY: bounded wait; result is a valid out pointer.
    let ok = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_NCHITTEST,
            WPARAM(0),
            lparam,
            SMTO_ABORTIFHUNG,
            100,
            Some(&mut result),
        )
    };
    if ok.0 == 0 {
        return HitArea::Client;
    }
    match result as u32 {
        HTCAPTION => HitArea::Caption,
        HTLEFT => HitArea::Left,
        HTRIGHT => HitArea::Right,
        HTTOP => HitArea::Top,
        HTBOTTOM => HitArea::Bottom,
        HTTOPLEFT => HitArea::TopLeft,
        HTTOPRIGHT => HitArea::TopRight,
        HTBOTTOMLEFT => HitArea::BottomLeft,
        HTBOTTOMRIGHT => HitArea::BottomRight,
        HTMINBUTTON | HTMAXBUTTON | HTCLOSE | HTSYSMENU | HTHELP => HitArea::Button,
        _ => HitArea::Client,
    }
}

fn is_tracked_window(windows: &HashMap<u64, Tracked>, id: u64) -> Option<HWND> {
    let hwnd = hwnd_of(id);
    // Only act on windows we mirror (ids come from the host).
    // SAFETY: IsWindow accepts any value.
    (windows.contains_key(&id) && unsafe { IsWindow(Some(hwnd)) }.as_bool()).then_some(hwnd)
}

// ---------------------------------------------------------------- engine

fn engine(port: &Port, rx: &Receiver<Input>) {
    // SAFETY: per-thread WinRT initialization.
    let _ = unsafe { RoInitialize(RO_INIT_MULTITHREADED) };
    let writer = Writer {
        port,
        lock: Mutex::new(()),
    };
    let d3d = match d3d() {
        Ok(d) => Some(d),
        Err(e) => {
            log(format!("coherence: no Direct3D device: {e}"));
            None
        }
    };
    let mut enabled = false;
    let mut interval = Duration::from_millis(33);
    let mut windows: HashMap<u64, Tracked> = HashMap::new();
    let mut captures: HashMap<u64, Capture> = HashMap::new();
    let mut foreground: Option<u64> = None;
    let mut next_tick = Instant::now();

    loop {
        let timeout = next_tick.saturating_duration_since(Instant::now());
        match rx.recv_timeout(if enabled {
            timeout
        } else {
            Duration::from_secs(3600)
        }) {
            Ok(Input::Host(msg)) => match msg {
                CoMessage::Start { max_fps } => {
                    enabled = true;
                    interval = Duration::from_millis(1000 / u64::from(max_fps.clamp(1, 60)));
                    windows.clear();
                    captures.clear();
                    foreground = None;
                    let (dw, dh) = desktop_size();
                    writer.send(&CoMessage::Desktop {
                        width: dw,
                        height: dh,
                    });
                    log(format!("coherence: started at {max_fps} fps"));
                }
                CoMessage::Stop => {
                    enabled = false;
                    captures.clear();
                    windows.clear();
                }
                CoMessage::Activate { id } => {
                    if let Some(h) = is_tracked_window(&windows, id) {
                        activate(h);
                    }
                }
                CoMessage::Resize { id, width, height } => {
                    if let Some(h) = is_tracked_window(&windows, id) {
                        resize(h, width, height);
                    }
                }
                CoMessage::Close { id } => {
                    if let Some(h) = is_tracked_window(&windows, id) {
                        // SAFETY: posting WM_CLOSE to a window we track.
                        unsafe {
                            let _ = PostMessageW(Some(h), WM_CLOSE, WPARAM(0), LPARAM(0));
                        }
                    }
                }
                CoMessage::Minimize { id } => {
                    if let Some(h) = is_tracked_window(&windows, id) {
                        // SAFETY: plain window-manager call.
                        unsafe {
                            let _ = ShowWindowAsync(h, SW_MINIMIZE);
                        }
                    }
                }
                CoMessage::Restore { id } => {
                    if let Some(h) = is_tracked_window(&windows, id) {
                        activate(h);
                    }
                }
                CoMessage::HitTest { id, serial, x, y } => {
                    let area = is_tracked_window(&windows, id)
                        .map_or(HitArea::Client, |h| hit_test(h, x, y));
                    writer.send(&CoMessage::HitTestResult { id, serial, area });
                }
                _ => {} // guest-to-host messages are ignored
            },
            Ok(Input::Lost) | Err(RecvTimeoutError::Disconnected) => return,
            Err(RecvTimeoutError::Timeout) => {}
        }
        if !enabled || Instant::now() < next_tick {
            continue;
        }
        next_tick = Instant::now() + interval;
        if !tick(
            &writer,
            d3d.as_ref(),
            &mut windows,
            &mut captures,
            &mut foreground,
        ) {
            return; // the host stopped reading
        }
    }
}

fn desktop_size() -> (u32, u32) {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    };
    // SAFETY: plain query.
    let (w, h) = unsafe {
        (
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    (w.max(1) as u32, h.max(1) as u32)
}

/// One pass: window list diff, foreground, and frames. Returns false when
/// a write fails (host gone).
fn tick(
    writer: &Writer<'_>,
    d3d: Option<&D3d>,
    windows: &mut HashMap<u64, Tracked>,
    captures: &mut HashMap<u64, Capture>,
    foreground: &mut Option<u64>,
) -> bool {
    let mut seen = Vec::new();
    for hwnd in top_level_windows() {
        let Some(info) = describe(hwnd) else { continue };
        seen.push(info.id);
        match windows.get(&info.id) {
            None => {
                if !writer.send(&CoMessage::Opened(info.clone())) {
                    return false;
                }
                windows.insert(info.id, Tracked { info });
            }
            Some(t) if t.info != info => {
                if !writer.send(&CoMessage::Changed(info.clone())) {
                    return false;
                }
                windows.insert(info.id, Tracked { info });
            }
            Some(_) => {}
        }
    }
    let gone: Vec<u64> = windows
        .keys()
        .filter(|id| !seen.contains(id))
        .copied()
        .collect();
    for id in gone {
        windows.remove(&id);
        captures.remove(&id);
        if !writer.send(&CoMessage::Closed { id }) {
            return false;
        }
    }
    // SAFETY: plain query.
    let fg = unsafe { GetForegroundWindow() };
    let fg = Some(fg.0 as usize as u64).filter(|id| windows.contains_key(id));
    if fg != *foreground {
        *foreground = fg;
        if !writer.send(&CoMessage::Foreground { id: fg }) {
            return false;
        }
    }
    let Some(d3d) = d3d else { return true };
    for (id, t) in windows.iter() {
        if t.info.state == WindowState::Minimized {
            captures.remove(id);
            continue;
        }
        let capture = match captures.entry(*id) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(v) => match Capture::start(d3d, hwnd_of(*id))
            {
                Ok(c) => v.insert(c),
                Err(e) => {
                    log(format!("coherence: cannot capture {:?}: {e}", t.info.title));
                    continue;
                }
            },
        };
        let frame = match capture.poll(d3d) {
            Ok(Some(f)) => f,
            Ok(None) => continue,
            Err(e) => {
                log(format!(
                    "coherence: capture failed for {:?}: {e}",
                    t.info.title
                ));
                continue;
            }
        };
        let (w, h, pixels) = frame;
        let previous = capture
            .previous
            .as_ref()
            .filter(|(pw, ph, _)| (*pw, *ph) == (w, h))
            .map(|(_, _, p)| p.as_slice());
        let tiles = damaged_tiles(&pixels, previous, w, h);
        if !tiles.is_empty()
            && !writer.send(&CoMessage::Frame {
                id: *id,
                width: w,
                height: h,
                tiles,
            })
        {
            return false;
        }
        capture.previous = Some((w, h, pixels));
    }
    true
}
