//! Side-by-side benchmark: direct vs tunnel (end-to-end TLS) vs 1-RTT gate.

use std::io::Read;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::config::Config;

const TARGETS: &[(&str, &str)] = &[
    ("GitHub repo", "https://github.com/leanprover/lean4"),
    ("GitHub API", "https://api.github.com/repos/torvalds/linux"),
    ("Hacker News", "https://news.ycombinator.com/"),
    ("MDN", "https://developer.mozilla.org/en-US/"),
    ("Cloudflare", "https://www.cloudflare.com/cdn-cgi/trace"),
    ("PyPI JSON", "https://pypi.org/pypi/httpx/json"),
];

#[derive(Clone, Copy)]
enum Mode {
    Direct,
    Tunnel,
    Gate,
}

fn probe(agent: &ureq::Agent, url: &str) -> Value {
    let t0 = Instant::now();
    match agent.get(url).set("User-Agent", "geoboost-bench/1").call() {
        Ok(resp) => {
            let code = resp.status();
            let ttfb = t0.elapsed().as_secs_f64();
            let mut n = 0u64;
            let mut r = resp.into_reader().take(20 * 1024 * 1024);
            let mut buf = [0u8; 16384];
            loop {
                match r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(k) => n += k as u64,
                    Err(_) => {
                        return json!({"ok": false, "err": "TRANSFER CUT", "total": t0.elapsed().as_secs_f64()});
                    }
                }
            }
            json!({"ok": (200..400).contains(&code), "err": format!("HTTP {code}"), "total": t0.elapsed().as_secs_f64(), "ttfb": ttfb, "bytes": n})
        }
        Err(ureq::Error::Status(code, _)) => {
            json!({"ok": false, "err": format!("HTTP {code}"), "total": t0.elapsed().as_secs_f64()})
        }
        Err(e) => {
            let s = e.to_string().to_lowercase();
            let err = if s.contains("timed out") || s.contains("timeout") {
                "TIMEOUT"
            } else if s.contains("reset") {
                "TCP RESET"
            } else {
                "ERROR"
            };
            json!({"ok": false, "err": err, "detail": e.to_string(), "total": t0.elapsed().as_secs_f64()})
        }
    }
}

fn burst(cfg: &Config, mode: Mode) -> (Vec<Value>, f64) {
    let mut b = ureq::AgentBuilder::new().timeout(Duration::from_secs(15)).redirects(5);
    if let Mode::Tunnel = mode {
        if let Ok(p) = ureq::Proxy::new(format!("http://127.0.0.1:{}", cfg.http_port)) {
            b = b.proxy(p);
        }
    }
    let agent = b.build();
    let gate = cfg.gate_port;
    let t0 = Instant::now();
    let handles: Vec<_> = TARGETS
        .iter()
        .map(|(_, url)| {
            let agent = agent.clone();
            let u = match mode {
                Mode::Gate => format!("http://127.0.0.1:{gate}/{url}"),
                _ => url.to_string(),
            };
            std::thread::spawn(move || probe(&agent, &u))
        })
        .collect();
    let rows = handles.into_iter().map(|h| h.join().unwrap_or(json!({"ok": false, "err": "ERROR"}))).collect();
    (rows, t0.elapsed().as_secs_f64())
}

pub fn run(cfg: &Config) -> Value {
    crate::logx::log("Speed test started");
    let (gate, gate_wall) = burst(cfg, Mode::Gate);
    let (tunnel, tunnel_wall) = burst(cfg, Mode::Tunnel);
    let (direct, direct_wall) = burst(cfg, Mode::Direct);
    let rows: Vec<Value> = TARGETS
        .iter()
        .enumerate()
        .map(|(i, (name, _))| json!({"name": name, "direct": direct[i], "tunnel": tunnel[i], "gate": gate[i]}))
        .collect();
    crate::logx::log(&format!(
        "Speed test finished: direct {direct_wall:.2}s, tunnel {tunnel_wall:.2}s, 1-RTT {gate_wall:.2}s"
    ));
    json!({
        "running": false,
        "rows": rows,
        "wall": {"direct": direct_wall, "tunnel": tunnel_wall, "gate": gate_wall},
    })
}
