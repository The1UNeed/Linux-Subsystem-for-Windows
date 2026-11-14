//! Installed apps (`shell:AppsFolder`, the list Start shows) and app
//! identity of windows (AppUserModelID).
//!
//! `unsafe` here is COM/Win32 FFI only; each block states its contract.

use crate::formats;
use crate::win::log;
use lsw_proto::{AppEntry, MAX_ICON_LEN, app_identity};
use std::os::windows::process::CommandExt;
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Foundation::{HWND, SIZE};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC, GetDIBits,
    GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoInitializeEx, CoTaskMemFree,
};
use windows::Win32::UI::Shell::PropertiesSystem::{IPropertyStore, SHGetPropertyStoreForWindow};
use windows::Win32::UI::Shell::{
    BHID_EnumItems, FOLDERID_AppsFolder, IEnumShellItems, IShellItem, IShellItemImageFactory,
    KF_FLAG_DEFAULT, SHGetKnownFolderItem, SIGDN_NORMALDISPLAY, SIGDN_PARENTRELATIVEPARSING,
    SIIGBF_ICONONLY,
};
use windows::core::{GUID, PWSTR};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// `PKEY_AppUserModel_ID` {9F4C2855-9F79-4B39-A8D0-E1D42DE1D5F3}, 5.
const PKEY_APP_USER_MODEL_ID: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0x9f4c2855_9f79_4b39_a8d0_e1d42de1d5f3),
    pid: 5,
};

/// Take ownership of a COM-allocated string.
fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: `p` is a NUL-terminated string allocated with CoTaskMemAlloc
    // by the callee; we read it once and free it.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const _));
        s
    }
}

/// The AppUserModelID of a window, if it has one (Store apps, and desktop
/// apps that set it).
pub fn window_aumid(hwnd: HWND) -> Option<String> {
    // SAFETY: COM call on a window handle; the returned interface is owned.
    let store: IPropertyStore = unsafe { SHGetPropertyStoreForWindow(hwnd) }.ok()?;
    // SAFETY: valid key; the PROPVARIANT is owned and dropped (cleared).
    let value = unsafe { store.GetValue(&PKEY_APP_USER_MODEL_ID) }.ok()?;
    // SAFETY: converts any string-like PROPVARIANT; result owned by us.
    let s = take_pwstr(unsafe { PropVariantToStringAlloc(&value) }.ok()?);
    Some(s).filter(|s| !s.is_empty())
}

/// The AppUserModelID of the process owning `hwnd` (packaged desktop apps
/// such as the new Paint set it on the process, not on the window).
fn process_aumid(hwnd: HWND) -> Option<String> {
    use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;
    let mut pid = 0u32;
    // SAFETY: pid is a valid out pointer.
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    // SAFETY: limited-information handle, closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 512];
    let mut len = buf.len() as u32;
    // SAFETY: buf/len describe a valid buffer.
    let r = unsafe { GetApplicationUserModelId(process, &mut len, Some(PWSTR(buf.as_mut_ptr()))) };
    // SAFETY: we own the handle.
    unsafe {
        let _ = windows::Win32::Foundation::CloseHandle(process);
    }
    if r.0 != 0 {
        return None; // not a packaged process
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n])).filter(|s| !s.is_empty())
}

/// App identity of a window: its AUMID (window, else process), else the
/// executable file name.
pub fn window_app(hwnd: HWND, exe: &str) -> String {
    let aumid = window_aumid(hwnd).or_else(|| process_aumid(hwnd));
    app_identity(aumid.as_deref(), exe)
}

/// Read a 64×64 icon as PNG.
fn icon_png(item: &IShellItem) -> Vec<u8> {
    let Ok(factory) = windows::core::Interface::cast::<IShellItemImageFactory>(item) else {
        return Vec::new();
    };
    // SAFETY: COM call; the returned bitmap is owned and deleted below.
    let Ok(bitmap) = (unsafe { factory.GetImage(SIZE { cx: 64, cy: 64 }, SIIGBF_ICONONLY) }) else {
        return Vec::new();
    };
    let png = bitmap_to_png(bitmap).unwrap_or_default();
    // SAFETY: we own the bitmap.
    unsafe {
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
    }
    if png.len() > MAX_ICON_LEN {
        Vec::new()
    } else {
        png
    }
}

/// 32-bit top-down DIB of an HBITMAP, as PNG.
fn bitmap_to_png(bitmap: HBITMAP) -> Option<Vec<u8>> {
    let mut bm = BITMAP::default();
    // SAFETY: `bm` is a BITMAP-sized out buffer.
    let n = unsafe {
        GetObjectW(
            HGDIOBJ(bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some((&raw mut bm).cast()),
        )
    };
    if n == 0 || bm.bmWidth <= 0 || bm.bmHeight == 0 || bm.bmWidth > 256 || bm.bmHeight.abs() > 256
    {
        return None;
    }
    let (w, h) = (bm.bmWidth, bm.bmHeight.abs());
    let header = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: w,
        biHeight: -h, // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB.0,
        ..Default::default()
    };
    let mut info = BITMAPINFO {
        bmiHeader: header,
        ..Default::default()
    };
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    // SAFETY: screen DC released below; `pixels` holds w*h*4 bytes, the
    // size GetDIBits writes for this header.
    let lines = unsafe {
        let dc = GetDC(None);
        let lines = GetDIBits(
            dc,
            bitmap,
            0,
            h as u32,
            Some(pixels.as_mut_ptr().cast()),
            &mut info,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, dc);
        lines
    };
    if lines == 0 {
        return None;
    }
    // Reuse the DIB → PNG converter with a packed DIB.
    let mut dib = Vec::with_capacity(40 + pixels.len());
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&w.to_le_bytes());
    dib.extend_from_slice(&(-h).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&[0u8; 24]);
    dib.extend_from_slice(&pixels);
    formats::dib_to_png(&dib)
}

/// Installed apps from `shell:AppsFolder`.
pub fn list() -> Vec<AppEntry> {
    // SAFETY: per-thread COM initialization (S_FALSE if already done).
    let _ = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    match list_inner() {
        Ok(apps) => apps,
        Err(e) => {
            log(format!("apps: {e}"));
            Vec::new()
        }
    }
}

fn list_inner() -> windows::core::Result<Vec<AppEntry>> {
    // SAFETY: COM calls; every returned interface is owned.
    let folder: IShellItem =
        unsafe { SHGetKnownFolderItem(&FOLDERID_AppsFolder, KF_FLAG_DEFAULT, None)? };
    // SAFETY: COM call on an owned item; the enumerator is owned.
    let items: IEnumShellItems = unsafe { folder.BindToHandler(None, &BHID_EnumItems)? };
    let _ = CLSCTX_INPROC_SERVER;
    let mut apps = Vec::new();
    loop {
        let mut batch = [None];
        let mut fetched = 0u32;
        // SAFETY: one-element output array and count.
        if unsafe { items.Next(&mut batch, Some(&mut fetched)) }.is_err() || fetched == 0 {
            break;
        }
        let Some(item) = batch[0].take() else { break };
        // SAFETY: COM calls returning CoTaskMemAlloc strings.
        let (name, launch) = unsafe {
            (
                take_pwstr(
                    item.GetDisplayName(SIGDN_NORMALDISPLAY)
                        .unwrap_or(PWSTR::null()),
                ),
                take_pwstr(
                    item.GetDisplayName(SIGDN_PARENTRELATIVEPARSING)
                        .unwrap_or(PWSTR::null()),
                ),
            )
        };
        if name.is_empty() || launch.is_empty() {
            continue;
        }
        // Store apps are listed by AUMID ("Package!App"); desktop apps by
        // path (possibly "{KnownFolderGuid}\\…\\app.exe").
        let app = if launch.contains('!') {
            launch.clone()
        } else {
            app_identity(None, &launch)
        };
        // Skip uninstallers, help files and web links in Start.
        let lower = launch.to_ascii_lowercase();
        if lower.starts_with("http") || lower.ends_with(".chm") || lower.ends_with(".url") {
            continue;
        }
        apps.push(AppEntry {
            launch,
            name,
            app,
            icon_png: icon_png(&item),
        });
        if apps.len() >= 1024 {
            break;
        }
    }
    Ok(apps)
}

/// Start an app by its AppsFolder id.
pub fn launch(launch: &str) -> Result<(), String> {
    if launch.is_empty() || launch.contains(['"', '\0']) {
        return Err("invalid app id".into());
    }
    std::process::Command::new("explorer.exe")
        .arg(format!("shell:AppsFolder\\{launch}"))
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}
