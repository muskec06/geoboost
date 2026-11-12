use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub server_host: String,
    pub server_port: u16,
    /// PSK protected with Windows DPAPI (hex). Never stored in plain text on Windows.
    pub psk_protected: String,
    pub carriers: usize,
    pub socks_port: u16,
    pub http_port: u16,
    pub gate_port: u16,
    pub panel_port: u16,
    pub set_system_proxy: bool,
    pub set_git: bool,
    pub set_env: bool,
    /// Extra proxy bypass entries, separated by ';' (Windows ProxyOverride syntax).
    pub bypass: String,
    pub autostart: bool,
    pub auto_connect: bool,
    /// Turn the system proxy off automatically when no carrier is up for this many seconds (0 = never).
    pub fallback_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server_host: String::new(),
            server_port: 8444,
            psk_protected: String::new(),
            carriers: 12,
            socks_port: 11080,
            http_port: 18080,
            gate_port: 18081,
            panel_port: 18090,
            set_system_proxy: true,
            set_git: true,
            set_env: false,
            bypass: String::new(),
            autostart: false,
            auto_connect: false,
            fallback_secs: 15,
        }
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), String> {
        if self.server_host.trim().is_empty() {
            return Err("Server address is empty".into());
        }
        if self.server_port == 0 {
            return Err("Server port is invalid".into());
        }
        if !(1..=64).contains(&self.carriers) {
            return Err("Carrier count must be between 1 and 64".into());
        }
        let ports = [self.socks_port, self.http_port, self.gate_port, self.panel_port];
        for (i, p) in ports.iter().enumerate() {
            if *p == 0 {
                return Err("Local ports cannot be 0".into());
            }
            if ports[i + 1..].contains(p) {
                return Err(format!("Local port {p} is used twice"));
            }
        }
        Ok(())
    }
}

pub fn data_dir() -> PathBuf {
    if let Ok(d) = std::env::var("GEOBOOST_HOME") {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    {
        if let Ok(a) = std::env::var("APPDATA") {
            return PathBuf::from(a).join("GeoBoost");
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".config").join("GeoBoost")
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

pub fn load() -> Config {
    std::fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &Config) -> Result<(), String> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let tmp = dir.join("config.json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(cfg).unwrap()).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, config_path()).map_err(|e| e.to_string())
}

/// Bind 127.0.0.1:<preferred>; if busy, try the next 20 ports, then any free port.
/// Returns the listener and the port actually used.
pub async fn bind_local(preferred: u16) -> Result<(tokio::net::TcpListener, u16), String> {
    let mut candidates: Vec<u16> = vec![preferred];
    candidates.extend((1..=20u16).filter_map(|i| preferred.checked_add(i)));
    candidates.push(0);
    let mut last = String::new();
    for p in candidates {
        match tokio::net::TcpListener::bind(("127.0.0.1", p)).await {
            Ok(l) => {
                let actual = l.local_addr().map(|a| a.port()).unwrap_or(p);
                if actual != preferred {
                    crate::logx::log(&format!("Port {preferred} is used by another program; using {actual}"));
                }
                return Ok((l, actual));
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("No free port found on 127.0.0.1 ({last})"))
}
