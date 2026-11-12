//! Non-Windows stand-ins so the protocol core can be built and tested on Linux.

use crate::config::Config;
use crate::logx::log;

pub fn protect(raw: &[u8]) -> Result<String, String> {
    Ok(format!("plain:{}", hex::encode(raw)))
}

pub fn unprotect(s: &str) -> Option<Vec<u8>> {
    s.strip_prefix("plain:").and_then(|h| hex::decode(h).ok()).filter(|v| !v.is_empty())
}

pub fn apply_integration(cfg: &Config) -> Result<(), String> {
    log(&format!(
        "(test mode) system settings skipped: proxy={} git={} env={}",
        cfg.set_system_proxy, cfg.set_git, cfg.set_env
    ));
    Ok(())
}

pub fn restore_integration() {}

pub fn set_proxy_paused(paused: bool) {
    log(&format!("(test mode) proxy paused = {paused}"));
}

pub fn set_autostart(_on: bool) -> Result<(), String> {
    Ok(())
}

pub fn open_url(url: &str) {
    log(&format!("Panel: {url}"));
}
