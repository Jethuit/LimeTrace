#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(target_os = "windows"))]
compile_error!("LimeTrace Backend only supports Windows.");

mod config;
mod db;
mod monitor;
mod recorder;

use anyhow::{Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::System::Threading::CreateMutexW;

use crate::config::Config;
use crate::db::Database;
use crate::monitor::WindowsMonitor;
use crate::recorder::Recorder;

fn main() -> Result<()> {
    let _instance_guard = match acquire_single_instance_guard() {
        Ok(Some(guard)) => guard,
        Ok(None) => {
            return Ok(());
        }
        Err(err) => {
            eprintln!("single-instance guard error: {err:#}");
            return Ok(());
        }
    };

    let config = Config::from_args()?;
    if let Some(parent) = config.db_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create db parent directory: {}", parent.display()))?;
    }

    let db = Database::open(&config.db_path)?;
    let mut monitor = WindowsMonitor::new(config.idle_threshold);
    let mut recorder = Recorder::new(db, config.rotate_segment_every);

    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_signal = Arc::clone(&shutdown);
    if let Err(err) = ctrlc::set_handler(move || {
        shutdown_signal.store(true, Ordering::SeqCst);
    }) {
        eprintln!("ctrlc handler registration warning: {err}");
    }

    eprintln!(
        "LimeTrace Backend started | db={} | poll={}ms | idle={}s | rotate={}s",
        config.db_path.display(),
        duration_millis(config.poll_interval),
        config.idle_threshold.as_secs(),
        config.rotate_segment_every.as_secs()
    );

    while !shutdown.load(Ordering::Relaxed) {
        let sample = monitor.capture();
        if let Err(err) = recorder.ingest(sample) {
            eprintln!("ingest error: {err:#}");
        }
        thread::sleep(config.poll_interval);
    }

    recorder.flush_and_close(unix_seconds_now())?;
    eprintln!("LimeTrace Backend stopped");
    Ok(())
}

fn unix_seconds_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn duration_millis(duration: Duration) -> u128 {
    duration.as_millis()
}

struct InstanceGuard {
    handle: HANDLE,
}

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}

fn acquire_single_instance_guard() -> Result<Option<InstanceGuard>> {
    const ERROR_ALREADY_EXISTS_CODE: u32 = 183;

    let name: Vec<u16> = "Local\\LimeTraceBackendSingleton"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let handle = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
    if handle.is_null() {
        return Err(anyhow::anyhow!("CreateMutexW failed"));
    }

    let last_error = unsafe { GetLastError() };
    if last_error == ERROR_ALREADY_EXISTS_CODE {
        unsafe {
            CloseHandle(handle);
        }
        return Ok(None);
    }

    Ok(Some(InstanceGuard { handle }))
}
