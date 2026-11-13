//! Windows' clock and time zone, set from the host's (`Message::Clock`).
//!
//! `unsafe` here is Win32 FFI only; each block states its contract.

use crate::win::log;
use std::os::windows::process::CommandExt;
use std::process::Command;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_NOT_ALL_ASSIGNED, FILETIME, GetLastError, HANDLE, LUID, SYSTEMTIME,
};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::SystemInformation::{GetSystemTimeAsFileTime, SetSystemTime};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::Win32::System::Time::FileTimeToSystemTime;
use windows::core::{PCWSTR, s, w};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Set the system clock to `utc_ms` (Unix milliseconds) if it is off.
pub fn set_system_time(utc_ms: i64) -> Result<(), String> {
    // SAFETY: no arguments; returns the current system time.
    let ft = unsafe { GetSystemTimeAsFileTime() };
    let now = crate::core::filetime_to_unix_ms(
        (u64::from(ft.dwHighDateTime) << 32) | u64::from(ft.dwLowDateTime),
    );
    if !crate::core::clock_is_off(now, utc_ms) {
        return Ok(());
    }
    enable_privilege(w!("SeSystemtimePrivilege"))?;
    let value = crate::core::unix_ms_to_filetime(utc_ms);
    let ft = FILETIME {
        dwLowDateTime: value as u32,
        dwHighDateTime: (value >> 32) as u32,
    };
    let mut st = SYSTEMTIME::default();
    // SAFETY: both pointers refer to valid locals.
    unsafe { FileTimeToSystemTime(&ft, &mut st) }
        .map_err(|e| format!("FileTimeToSystemTime: {e}"))?;
    // SAFETY: `st` is a valid SYSTEMTIME.
    unsafe { SetSystemTime(&st) }.map_err(|e| format!("SetSystemTime: {e}"))?;
    log(format!(
        "clock set from the host (was {} ms off)",
        utc_ms - now
    ));
    Ok(())
}

/// Enable a privilege the (elevated) agent holds but has disabled.
fn enable_privilege(name: PCWSTR) -> Result<(), String> {
    let mut token = HANDLE::default();
    // SAFETY: the pseudo handle of this process and a valid out pointer.
    unsafe {
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
    }
    .map_err(|e| format!("OpenProcessToken: {e}"))?;
    let mut luid = LUID::default();
    // SAFETY: NUL-terminated privilege name and a valid out pointer.
    let looked_up = unsafe { LookupPrivilegeValueW(PCWSTR::null(), name, &mut luid) };
    let result = looked_up
        .map_err(|e| format!("LookupPrivilegeValueW: {e}"))
        .and_then(|()| {
            let privileges = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            // SAFETY: `token` is open with TOKEN_ADJUST_PRIVILEGES and
            // `privileges` is a valid one-entry list.
            unsafe { AdjustTokenPrivileges(token, false, Some(&privileges), 0, None, None) }
                .map_err(|e| format!("AdjustTokenPrivileges: {e}"))?;
            // SAFETY: reads this thread's last error.
            if unsafe { GetLastError() } == ERROR_NOT_ALL_ASSIGNED {
                return Err("the agent does not hold the privilege to set the clock".into());
            }
            Ok(())
        });
    // SAFETY: closing the token opened above.
    unsafe {
        let _ = CloseHandle(token);
    }
    result
}

/// Set Windows' time zone to the IANA zone `iana` (e.g. `Europe/Berlin`).
pub fn set_time_zone(iana: &str) -> Result<(), String> {
    let id = windows_zone_id(iana)?;
    let out = Command::new("tzutil")
        .args(["/s", &id])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("tzutil: {e}"))?;
    if !out.status.success() {
        return Err(format!("tzutil /s {id:?} failed ({})", out.status));
    }
    log(format!("time zone set to {id} ({iana})"));
    Ok(())
}

/// The Windows time zone id for an IANA zone, from the ICU built into
/// Windows 10 1903 and later (`icu.dll`).
fn windows_zone_id(iana: &str) -> Result<String, String> {
    // int32_t ucal_getWindowsTimeZoneID(const UChar* id, int32_t len,
    //     UChar* winid, int32_t winidCapacity, UErrorCode* status)
    type GetWindowsId = unsafe extern "C" fn(*const u16, i32, *mut u16, i32, *mut i32) -> i32;
    // SAFETY: loading a system DLL by name.
    let icu = unsafe { LoadLibraryW(w!("icu.dll")) }.map_err(|e| format!("icu.dll: {e}"))?;
    // SAFETY: a NUL-terminated export name in a loaded module.
    let f = unsafe { GetProcAddress(icu, s!("ucal_getWindowsTimeZoneID")) }
        .ok_or("icu.dll has no ucal_getWindowsTimeZoneID")?;
    // SAFETY: the export has the C signature above (ICU's stable C API).
    let f: GetWindowsId = unsafe { std::mem::transmute(f) };
    let id: Vec<u16> = iana.encode_utf16().collect();
    let mut buf = [0u16; 128];
    let mut status = 0i32;
    // SAFETY: `id` and `buf` are valid for the lengths passed.
    let n = unsafe {
        f(
            id.as_ptr(),
            id.len() as i32,
            buf.as_mut_ptr(),
            buf.len() as i32,
            &mut status,
        )
    };
    // ICU: status > 0 is a failure; 0 characters means no mapping.
    if status > 0 || n <= 0 || n as usize > buf.len() {
        return Err(format!("no Windows time zone for {iana}"));
    }
    Ok(String::from_utf16_lossy(&buf[..n as usize]))
}
