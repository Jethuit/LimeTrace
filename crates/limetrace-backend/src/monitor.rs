use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, HWND};
use windows_sys::Win32::System::SystemInformation::GetTickCount;
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetLastInputInfo, LASTINPUTINFO};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
};

#[derive(Debug, Clone)]
pub struct ActiveWindow {
    pub pid: u32,
    pub pid_create_time: Option<u64>,
    pub exe_name: String,
    pub process_path: String,
    pub window_title: String,
}

#[derive(Debug, Clone)]
pub enum ActivityKind {
    Idle { idle_ms: u32 },
    Active(ActiveWindow),
}

#[derive(Debug, Clone)]
pub struct ActivitySample {
    pub ts: i64,
    pub kind: ActivityKind,
}

#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
struct ProcessKey {
    pid: u32,
    creation_time: u64,
}

#[derive(Debug, Clone)]
struct ProcessMeta {
    exe_name: String,
    process_path: String,
}

pub struct WindowsMonitor {
    idle_threshold_ms: u32,
    process_cache: HashMap<ProcessKey, ProcessMeta>,
}

impl WindowsMonitor {
    pub fn new(idle_threshold: Duration) -> Self {
        let threshold_ms_u64 = idle_threshold.as_millis() as u64;
        let idle_threshold_ms = threshold_ms_u64.min(u32::MAX as u64) as u32;
        Self {
            idle_threshold_ms,
            process_cache: HashMap::new(),
        }
    }

    pub fn capture(&mut self) -> ActivitySample {
        let ts = unix_seconds_now();

        if let Some(idle_ms) = idle_millis() {
            if idle_ms >= self.idle_threshold_ms {
                return ActivitySample {
                    ts,
                    kind: ActivityKind::Idle { idle_ms },
                };
            }
        }

        let hwnd = unsafe { GetForegroundWindow() };
        if hwnd == std::ptr::null_mut() {
            return ActivitySample {
                ts,
                kind: ActivityKind::Active(ActiveWindow {
                    pid: 0,
                    pid_create_time: None,
                    exe_name: "UNKNOWN".to_owned(),
                    process_path: "<foreground-window-missing>".to_owned(),
                    window_title: String::new(),
                }),
            };
        }

        let window_title = get_window_title(hwnd);
        let pid = window_pid(hwnd).unwrap_or(0);
        if pid == 0 {
            return ActivitySample {
                ts,
                kind: ActivityKind::Active(ActiveWindow {
                    pid: 0,
                    pid_create_time: None,
                    exe_name: "UNKNOWN".to_owned(),
                    process_path: "<pid-missing>".to_owned(),
                    window_title,
                }),
            };
        }

        let pid_create_time = process_creation_time(pid);
        let (exe_name, process_path) = self.resolve_process(pid, pid_create_time);
        ActivitySample {
            ts,
            kind: ActivityKind::Active(ActiveWindow {
                pid,
                pid_create_time,
                exe_name,
                process_path,
                window_title,
            }),
        }
    }

    fn resolve_process(&mut self, pid: u32, pid_create_time: Option<u64>) -> (String, String) {
        if let Some(create_time) = pid_create_time {
            let key = ProcessKey {
                pid,
                creation_time: create_time,
            };
            if let Some(meta) = self.process_cache.get(&key) {
                return (meta.exe_name.clone(), meta.process_path.clone());
            }

            if let Some(process_path) = process_path(pid) {
                let exe_name = exe_name_from_path(&process_path, pid);
                if self.process_cache.len() >= 4096 {
                    self.process_cache.clear();
                }
                self.process_cache.insert(
                    key,
                    ProcessMeta {
                        exe_name: exe_name.clone(),
                        process_path: process_path.clone(),
                    },
                );
                return (exe_name, process_path);
            }
        }

        if let Some(process_path) = process_path(pid) {
            let exe_name = exe_name_from_path(&process_path, pid);
            return (exe_name, process_path);
        }
        ("UNKNOWN".to_owned(), format!("<pid-{pid}>"))
    }
}

fn unix_seconds_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn idle_millis() -> Option<u32> {
    let mut lii = LASTINPUTINFO {
        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
        dwTime: 0,
    };
    let ok = unsafe { GetLastInputInfo(&mut lii) };
    if ok == 0 {
        return None;
    }

    let now_tick = unsafe { GetTickCount() };
    Some(now_tick.wrapping_sub(lii.dwTime))
}

fn window_pid(hwnd: HWND) -> Option<u32> {
    let mut pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, &mut pid);
    }
    if pid == 0 {
        None
    } else {
        Some(pid)
    }
}

fn get_window_title(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }

    let mut buffer: Vec<u16> = vec![0; len as usize + 1];
    let copied = unsafe { GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32) };
    if copied <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..copied as usize]).trim().to_owned()
}

fn process_creation_time(pid: u32) -> Option<u64> {
    with_process_handle(pid, |handle| {
        let mut creation = zero_filetime();
        let mut exit = zero_filetime();
        let mut kernel = zero_filetime();
        let mut user = zero_filetime();

        let ok = unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) };
        if ok == 0 {
            return None;
        }
        Some(filetime_to_u64(creation))
    })
}

fn process_path(pid: u32) -> Option<String> {
    with_process_handle(pid, |handle| {
        let mut buffer: Vec<u16> = vec![0; 4096];
        let mut size: u32 = buffer.len() as u32;
        let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) };
        if ok == 0 || size == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buffer[..size as usize]))
    })
}

fn with_process_handle<T>(pid: u32, f: impl FnOnce(HANDLE) -> Option<T>) -> Option<T> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }

    let result = f(handle);
    unsafe {
        CloseHandle(handle);
    }
    result
}

fn exe_name_from_path(path: &str, pid: u32) -> String {
    Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_else(|| format!("pid-{pid}"))
}

fn zero_filetime() -> FILETIME {
    FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    }
}

fn filetime_to_u64(value: FILETIME) -> u64 {
    ((value.dwHighDateTime as u64) << 32) | (value.dwLowDateTime as u64)
}
