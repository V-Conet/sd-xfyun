//! 极简日志：模块的 stdout 是 speech-dispatcher 的协议通道，所有诊断信息只能走 stderr 或文件。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

pub const ERROR: u8 = 1;
pub const WARN: u8 = 2;
pub const INFO: u8 = 3;
pub const DEBUG: u8 = 4;
pub const TRACE: u8 = 5;

static LEVEL: AtomicU8 = AtomicU8::new(WARN);
static FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

pub fn set_level(level: u8) {
    LEVEL.store(level, Ordering::Relaxed);
}

pub fn level() -> u8 {
    LEVEL.load(Ordering::Relaxed)
}

pub fn enabled(level: u8) -> bool {
    level <= LEVEL.load(Ordering::Relaxed)
}

/// 追加日志文件（`DEBUG ON <file>` 与配置项 XfyunLogFile 都用它）。
pub fn set_file(path: Option<&Path>) {
    let mut slot = FILE.lock().unwrap();
    *slot = path.and_then(|p| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .map_err(|e| eprintln!("[xfyun] 无法打开日志文件 {}: {e}", p.display()))
            .ok()
    });
}

fn timestamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs();
    let ms = now.subsec_millis();
    // 只做简单的 UTC 时分秒换算，日志用途足够
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

pub fn log(level: u8, args: std::fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    let tag = match level {
        ERROR => "ERROR",
        WARN => "WARN",
        INFO => "INFO",
        DEBUG => "DEBUG",
        _ => "TRACE",
    };
    let line = format!("[xfyun {} {}] {}\n", timestamp(), tag, args);
    let _ = std::io::stderr().write_all(line.as_bytes());
    if let Ok(mut slot) = FILE.lock() {
        if let Some(f) = slot.as_mut() {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

#[macro_export]
macro_rules! log_error { ($($a:tt)*) => { $crate::log::log($crate::log::ERROR, format_args!($($a)*)) } }
#[macro_export]
macro_rules! log_warn  { ($($a:tt)*) => { $crate::log::log($crate::log::WARN,  format_args!($($a)*)) } }
#[macro_export]
macro_rules! log_info  { ($($a:tt)*) => { $crate::log::log($crate::log::INFO,  format_args!($($a)*)) } }
#[macro_export]
macro_rules! log_debug { ($($a:tt)*) => { $crate::log::log($crate::log::DEBUG, format_args!($($a)*)) } }
#[macro_export]
macro_rules! log_trace { ($($a:tt)*) => { $crate::log::log($crate::log::TRACE, format_args!($($a)*)) } }
