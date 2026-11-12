//! Tiny logger: keeps the last lines in memory for the panel and appends to a file.

use std::collections::VecDeque;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

static LINES: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
static FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

pub fn init(path: PathBuf) {
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > 1024 * 1024 {
            let _ = std::fs::rename(&path, path.with_extension("log.old"));
        }
    }
    *FILE.lock().unwrap() = Some(path);
}

#[cfg(windows)]
fn stamp() -> String {
    let mut t: windows_sys::Win32::Foundation::SYSTEMTIME = unsafe { std::mem::zeroed() };
    unsafe { windows_sys::Win32::System::SystemInformation::GetLocalTime(&mut t) };
    format!("{:02}.{:02} {:02}:{:02}:{:02}", t.wDay, t.wMonth, t.wHour, t.wMinute, t.wSecond)
}

#[cfg(not(windows))]
fn stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let d = secs % 86400;
    format!("{:02}:{:02}:{:02}Z", d / 3600, (d % 3600) / 60, d % 60)
}

pub fn log(msg: &str) {
    let line = format!("{} {}", stamp(), msg);
    {
        let mut l = LINES.lock().unwrap();
        l.push_back(line.clone());
        while l.len() > 300 {
            l.pop_front();
        }
    }
    if let Some(p) = FILE.lock().unwrap().as_ref() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(p) {
            let _ = writeln!(f, "{line}");
        }
    }
    #[cfg(not(windows))]
    eprintln!("{line}");
}

pub fn recent(n: usize) -> Vec<String> {
    let l = LINES.lock().unwrap();
    l.iter().rev().take(n).rev().cloned().collect()
}
