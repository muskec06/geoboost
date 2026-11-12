//! Application state: start/stop the tunnel, apply/restore Windows settings, watchdog, stats.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::task::JoinHandle;

use crate::config::{self, Config};
use crate::logx::log;
use crate::mux::{Mux, MuxConfig, Stats};
use crate::plat;
use crate::proxy::{self, Kind};

struct Engine {
    mux: Arc<Mux>,
    tasks: Vec<JoinHandle<()>>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Off,
    Connecting,
    Connected,
    Fallback,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Off => "Off",
            Phase::Connecting => "Connecting",
            Phase::Connected => "Connected",
            Phase::Fallback => "Fallback (direct connection)",
        }
    }
    pub fn key(self) -> &'static str {
        match self {
            Phase::Off => "off",
            Phase::Connecting => "connecting",
            Phase::Connected => "connected",
            Phase::Fallback => "fallback",
        }
    }
}

#[derive(Default, Clone, Copy)]
struct Rates {
    wire_up: f64,
    wire_down: f64,
    app_up: f64,
    app_down: f64,
}

pub struct App {
    pub rt: tokio::runtime::Handle,
    pub cfg: Mutex<Config>,
    engine: Mutex<Option<Engine>>,
    busy: tokio::sync::Mutex<()>,
    pub stats: Arc<Stats>,
    pub token: String,
    applied: AtomicBool,
    paused: AtomicBool,
    rates: Mutex<Rates>,
    last_error: Mutex<String>,
    bench: Mutex<Value>,
    on_change: Mutex<Option<Box<dyn Fn(Phase, usize, usize) + Send + Sync>>>,
}

impl App {
    pub fn new(rt: tokio::runtime::Handle) -> Arc<Self> {
        let mut tok = [0u8; 16];
        getrandom::getrandom(&mut tok).expect("rng");
        let app = Arc::new(Self {
            rt,
            cfg: Mutex::new(config::load()),
            engine: Mutex::new(None),
            busy: tokio::sync::Mutex::new(()),
            stats: Arc::new(Stats::default()),
            token: hex::encode(tok),
            applied: AtomicBool::new(false),
            paused: AtomicBool::new(false),
            rates: Mutex::new(Rates::default()),
            last_error: Mutex::new(String::new()),
            bench: Mutex::new(json!({"running": false})),
            on_change: Mutex::new(None),
        });
        let a = app.clone();
        app.rt.spawn(async move { a.watchdog().await });
        app
    }

    #[cfg_attr(not(windows), allow(dead_code))]
    pub fn set_on_change(&self, f: Box<dyn Fn(Phase, usize, usize) + Send + Sync>) {
        *self.on_change.lock().unwrap() = Some(f);
    }

    fn notify(&self) {
        let (p, r, t) = self.phase_counts();
        if let Some(f) = self.on_change.lock().unwrap().as_ref() {
            f(p, r, t);
        }
    }

    pub fn panel_url(&self) -> String {
        let port = self.cfg.lock().unwrap().panel_port;
        format!("http://127.0.0.1:{port}/#t={}", self.token)
    }

    pub fn is_running(&self) -> bool {
        self.engine.lock().unwrap().is_some()
    }

    pub fn phase_counts(&self) -> (Phase, usize, usize) {
        let eng = self.engine.lock().unwrap();
        match eng.as_ref() {
            None => (Phase::Off, 0, 0),
            Some(e) => {
                let ready = e.mux.ready_count();
                let total = e.mux.carrier_count();
                let p = if self.paused.load(Ordering::Relaxed) {
                    Phase::Fallback
                } else if ready > 0 {
                    Phase::Connected
                } else {
                    Phase::Connecting
                };
                (p, ready, total)
            }
        }
    }

    fn set_error(&self, e: &str) {
        *self.last_error.lock().unwrap() = e.to_string();
        if !e.is_empty() {
            log(&format!("Error: {e}"));
        }
    }

    // ------------------------------------------------------------ connect / disconnect

    pub async fn connect(self: &Arc<Self>) -> Result<(), String> {
        let _g = self.busy.lock().await;
        if self.is_running() {
            return Ok(());
        }
        let cfg = self.cfg.lock().unwrap().clone();
        let r = self.start_engine(&cfg).await;
        if let Err(e) = &r {
            self.set_error(e);
            self.notify();
            return r;
        }
        self.set_error("");
        // Ports may have moved to free ones during start: apply settings with the real ports.
        let cfg = self.cfg.lock().unwrap().clone();
        if cfg.set_system_proxy || cfg.set_git || cfg.set_env {
            match plat::apply_integration(&cfg) {
                Ok(()) => {
                    self.applied.store(true, Ordering::Relaxed);
                    log("System settings applied (proxy/git/env)");
                }
                Err(e) => {
                    self.applied.store(true, Ordering::Relaxed); // partial changes must still be restored
                    self.set_error(&format!("Some system settings could not be applied: {e}"));
                }
            }
        }
        self.notify();
        Ok(())
    }

    async fn start_engine(self: &Arc<Self>, cfg: &Config) -> Result<(), String> {
        cfg.validate()?;
        let psk = plat::unprotect(&cfg.psk_protected).ok_or("No PSK key has been entered")?;
        let (socks, socks_port) = config::bind_local(cfg.socks_port).await?;
        let (http, http_port) = config::bind_local(cfg.http_port).await?;
        let (gate, gate_port) = config::bind_local(cfg.gate_port).await?;
        let mut cfg = cfg.clone();
        if (socks_port, http_port, gate_port) != (cfg.socks_port, cfg.http_port, cfg.gate_port) {
            cfg.socks_port = socks_port;
            cfg.http_port = http_port;
            cfg.gate_port = gate_port;
            let mut c = self.cfg.lock().unwrap();
            c.socks_port = socks_port;
            c.http_port = http_port;
            c.gate_port = gate_port;
            let _ = config::save(&c);
        }
        let cfg = &cfg;

        let mux = Mux::new(
            MuxConfig { host: cfg.server_host.trim().to_string(), port: cfg.server_port, psk, carriers: cfg.carriers },
            self.stats.clone(),
        );
        let mut tasks = Vec::new();
        for i in 0..cfg.carriers {
            tasks.push(self.rt.spawn(mux.clone().maintain(i)));
        }
        tasks.push(self.rt.spawn(mux.clone().keepalive()));
        tasks.push(self.rt.spawn(proxy::serve(socks, mux.clone(), Kind::Socks)));
        tasks.push(self.rt.spawn(proxy::serve(http, mux.clone(), Kind::Http)));
        tasks.push(self.rt.spawn(proxy::serve(gate, mux.clone(), Kind::Gate)));
        *self.engine.lock().unwrap() = Some(Engine { mux, tasks });
        log(&format!(
            "Started → {}:{} ({} carriers) | SOCKS5 127.0.0.1:{} | HTTP 127.0.0.1:{} | 1-RTT 127.0.0.1:{}",
            cfg.server_host, cfg.server_port, cfg.carriers, cfg.socks_port, cfg.http_port, cfg.gate_port
        ));
        Ok(())
    }

    pub async fn disconnect(self: &Arc<Self>) {
        let _g = self.busy.lock().await;
        if let Some(e) = self.teardown() {
            // Wait until every task (and so every listening socket) is really gone,
            // otherwise an immediate reconnect races on the local ports.
            for t in e.tasks {
                let _ = t.await;
            }
        }
        self.notify();
    }

    /// Synchronous teardown: also used on exit and on Windows session end.
    pub fn disconnect_inner(&self) {
        let _ = self.teardown();
    }

    fn teardown(&self) -> Option<Engine> {
        if self.applied.swap(false, Ordering::Relaxed) {
            plat::restore_integration();
            log("System settings restored");
        }
        self.paused.store(false, Ordering::Relaxed);
        let e = self.engine.lock().unwrap().take()?;
        for t in &e.tasks {
            t.abort();
        }
        e.mux.shutdown();
        log("Disconnected");
        Some(e)
    }

    // ------------------------------------------------------------ config

    pub async fn update_config(self: &Arc<Self>, v: Value) -> Result<String, String> {
        let mut cfg = self.cfg.lock().unwrap().clone();
        let old = cfg.clone();
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).map(|x| x.trim().to_string());
        let n = |k: &str| v.get(k).and_then(|x| x.as_u64());
        let b = |k: &str| v.get(k).and_then(|x| x.as_bool());
        if let Some(x) = s("server_host") {
            cfg.server_host = x;
        }
        let port = |x: u64| u16::try_from(x).map_err(|_| "Ports must be between 1 and 65535".to_string());
        if let Some(x) = n("server_port") {
            cfg.server_port = port(x)?;
        }
        if let Some(x) = n("carriers") {
            cfg.carriers = x as usize;
        }
        if let Some(x) = n("socks_port") {
            cfg.socks_port = port(x)?;
        }
        if let Some(x) = n("http_port") {
            cfg.http_port = port(x)?;
        }
        if let Some(x) = n("gate_port") {
            cfg.gate_port = port(x)?;
        }
        if let Some(x) = n("panel_port") {
            cfg.panel_port = port(x)?;
        }
        if let Some(x) = n("fallback_secs") {
            cfg.fallback_secs = x;
        }
        if let Some(x) = b("set_system_proxy") {
            cfg.set_system_proxy = x;
        }
        if let Some(x) = b("set_git") {
            cfg.set_git = x;
        }
        if let Some(x) = b("set_env") {
            cfg.set_env = x;
        }
        if let Some(x) = b("autostart") {
            cfg.autostart = x;
        }
        if let Some(x) = b("auto_connect") {
            cfg.auto_connect = x;
        }
        if let Some(x) = s("bypass") {
            cfg.bypass = x;
        }
        if let Some(p) = s("psk") {
            if !p.is_empty() {
                let raw = hex::decode(&p).map_err(|_| "The PSK must be hex, e.g. the output of: openssl rand -hex 32".to_string())?;
                if raw.len() < 16 {
                    return Err("The PSK must be at least 32 hex characters".into());
                }
                cfg.psk_protected = plat::protect(&raw)?;
            }
        }
        cfg.validate()?;
        config::save(&cfg)?;
        if cfg.autostart != old.autostart {
            plat::set_autostart(cfg.autostart)?;
        }
        *self.cfg.lock().unwrap() = cfg.clone();
        log("Settings saved");

        let mut msg = "Saved".to_string();
        if cfg.panel_port != old.panel_port {
            msg.push_str(". The panel port changes after the app restarts");
        }
        if self.is_running() {
            self.disconnect().await;
            self.connect().await?;
            msg.push_str(". Reconnected with the new settings");
        }
        Ok(msg)
    }

    pub fn config_json(&self) -> Value {
        let c = self.cfg.lock().unwrap().clone();
        let mut v = serde_json::to_value(&c).unwrap();
        v.as_object_mut().unwrap().remove("psk_protected");
        v["has_psk"] = json!(!c.psk_protected.is_empty());
        v
    }

    // ------------------------------------------------------------ status

    pub fn status_json(&self) -> Value {
        let (phase, ready, total) = self.phase_counts();
        let (carriers, streams, mux_err) = match self.engine.lock().unwrap().as_ref() {
            Some(e) => (
                e.mux
                    .carrier_info()
                    .into_iter()
                    .map(|c| json!({"idx": c.idx, "alive": c.alive, "rtt_ms": c.rtt_ms.round(), "load": c.load.round(), "streams": c.streams}))
                    .collect::<Vec<_>>(),
                e.mux.active_streams(),
                e.mux.last_error.lock().unwrap().clone(),
            ),
            None => (vec![], 0, String::new()),
        };
        let r = *self.rates.lock().unwrap();
        let st = &self.stats;
        let wd = st.wire_down.load(Ordering::Relaxed) as f64;
        let ad = st.app_down.load(Ordering::Relaxed) as f64;
        let err = {
            let e = self.last_error.lock().unwrap().clone();
            if e.is_empty() && ready == 0 && phase != Phase::Off { mux_err } else { e }
        };
        json!({
            "phase": phase.key(),
            "phase_label": phase.label(),
            "ready": ready,
            "total": total,
            "carriers": carriers,
            "active_streams": streams,
            "rates": {"wire_up": r.wire_up, "wire_down": r.wire_down, "app_up": r.app_up, "app_down": r.app_down},
            "totals": {
                "wire_up": st.wire_up.load(Ordering::Relaxed),
                "wire_down": wd,
                "app_up": st.app_up.load(Ordering::Relaxed),
                "app_down": ad,
                "streams": st.streams_opened.load(Ordering::Relaxed),
            },
            "saving_pct": if ad > 0.0 { ((1.0 - wd / ad) * 100.0).max(0.0).round() } else { 0.0 },
            "error": err,
            "log": crate::logx::recent(60),
        })
    }

    async fn watchdog(self: Arc<Self>) {
        let mut last = (0u64, 0u64, 0u64, 0u64);
        let mut t_last = Instant::now();
        let mut zero_since: Option<Instant> = None;
        let mut last_phase = (Phase::Off, 0usize);
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            // Rates
            let st = &self.stats;
            let now = (
                st.wire_up.load(Ordering::Relaxed),
                st.wire_down.load(Ordering::Relaxed),
                st.app_up.load(Ordering::Relaxed),
                st.app_down.load(Ordering::Relaxed),
            );
            let dt = t_last.elapsed().as_secs_f64().max(0.001);
            t_last = Instant::now();
            *self.rates.lock().unwrap() = Rates {
                wire_up: (now.0 - last.0) as f64 / dt,
                wire_down: (now.1 - last.1) as f64 / dt,
                app_up: (now.2 - last.2) as f64 / dt,
                app_down: (now.3 - last.3) as f64 / dt,
            };
            last = now;

            // Fallback: if the tunnel is dead, give the user direct internet instead of a dead proxy.
            let (phase, ready, _) = self.phase_counts();
            let cfg = self.cfg.lock().unwrap().clone();
            if phase != Phase::Off && self.applied.load(Ordering::Relaxed) && cfg.set_system_proxy && cfg.fallback_secs > 0 {
                if ready == 0 {
                    let since = *zero_since.get_or_insert_with(Instant::now);
                    if since.elapsed() >= Duration::from_secs(cfg.fallback_secs) && !self.paused.swap(true, Ordering::Relaxed) {
                        plat::set_proxy_paused(true);
                        log("No carriers up: system proxy temporarily disabled (direct connection)");
                    }
                } else {
                    zero_since = None;
                    if self.paused.swap(false, Ordering::Relaxed) {
                        plat::set_proxy_paused(false);
                        log("Carriers are back: system proxy re-enabled");
                    }
                }
            } else {
                zero_since = None;
            }

            let (p2, r2, _) = self.phase_counts();
            if (p2, r2) != last_phase {
                last_phase = (p2, r2);
                self.notify();
            }
        }
    }

    // ------------------------------------------------------------ bench

    pub fn bench_json(&self) -> Value {
        self.bench.lock().unwrap().clone()
    }

    pub fn start_bench(self: &Arc<Self>) -> Result<(), String> {
        if !self.is_running() {
            return Err("Connect first".into());
        }
        {
            let mut b = self.bench.lock().unwrap();
            if b["running"].as_bool() == Some(true) {
                return Err("A test is already running".into());
            }
            *b = json!({"running": true});
        }
        let cfg = self.cfg.lock().unwrap().clone();
        let me = self.clone();
        std::thread::spawn(move || {
            let res = crate::bench::run(&cfg);
            *me.bench.lock().unwrap() = res;
        });
        Ok(())
    }
}
