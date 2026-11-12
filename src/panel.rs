//! Local control panel: http://127.0.0.1:<panel_port>/  (API protected by a per-run token + Host check).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::time::timeout;

use crate::app::App;

const PAGE: &str = include_str!("panel.html");

pub async fn serve(app: Arc<App>, listener: TcpListener, port: u16) {
    loop {
        let Ok((s, _)) = listener.accept().await else { continue };
        let app = app.clone();
        tokio::spawn(async move {
            let _ = timeout(Duration::from_secs(120), handle_conn(app, s, port)).await;
        });
    }
}

async fn respond(s: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) {
    let reason = match code {
        200 => "OK",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {reason}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Frame-Options: DENY\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(head.as_bytes()).await;
    let _ = s.write_all(body).await;
    let _ = s.shutdown().await;
}

async fn json_resp(s: &mut TcpStream, code: u16, v: Value) {
    respond(s, code, "application/json; charset=utf-8", v.to_string().as_bytes()).await;
}

pub async fn handle_conn(app: Arc<App>, mut s: TcpStream, port: u16) {
    let mut r = BufReader::new(&mut s);
    let mut line = String::new();
    if r.read_line(&mut line).await.is_err() {
        return;
    }
    let mut it = line.split_whitespace();
    let method = it.next().unwrap_or("").to_string();
    let path = it.next().unwrap_or("/").to_string();
    let mut host = String::new();
    let mut token = String::new();
    let mut clen = 0usize;
    loop {
        let mut h = String::new();
        match r.read_line(&mut h).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let v = v.trim().to_string();
            match k.trim().to_ascii_lowercase().as_str() {
                "host" => host = v,
                "x-token" => token = v,
                "content-length" => clen = v.parse().unwrap_or(0).min(64 * 1024),
                _ => {}
            }
        }
    }
    let mut body = vec![0u8; clen];
    if clen > 0 && r.read_exact(&mut body).await.is_err() {
        return;
    }
    drop(r);

    let host_ok = host == format!("127.0.0.1:{port}") || host == format!("localhost:{port}");
    if !host_ok {
        respond(&mut s, 403, "text/plain", b"forbidden").await;
        return;
    }
    let path_only = path.split('?').next().unwrap_or("/");
    if path_only == "/" || path_only == "/index.html" {
        respond(&mut s, 200, "text/html; charset=utf-8", PAGE.as_bytes()).await;
        return;
    }
    if !path_only.starts_with("/api/") {
        respond(&mut s, 404, "text/plain", b"not found").await;
        return;
    }
    if token.is_empty() || token != app.token {
        json_resp(&mut s, 403, json!({"error": "Invalid session. Open the panel from the GeoBoost tray icon."})).await;
        return;
    }

    match (method.as_str(), path_only) {
        ("GET", "/api/status") => json_resp(&mut s, 200, app.status_json()).await,
        ("GET", "/api/config") => json_resp(&mut s, 200, app.config_json()).await,
        ("POST", "/api/config") => {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            match app.update_config(v).await {
                Ok(msg) => json_resp(&mut s, 200, json!({"ok": true, "message": msg})).await,
                Err(e) => json_resp(&mut s, 400, json!({"ok": false, "error": e})).await,
            }
        }
        ("POST", "/api/connect") => match app.connect().await {
            Ok(()) => json_resp(&mut s, 200, json!({"ok": true})).await,
            Err(e) => json_resp(&mut s, 400, json!({"ok": false, "error": e})).await,
        },
        ("POST", "/api/disconnect") => {
            app.disconnect().await;
            json_resp(&mut s, 200, json!({"ok": true})).await
        }
        ("POST", "/api/bench") => match app.start_bench() {
            Ok(()) => json_resp(&mut s, 200, json!({"ok": true})).await,
            Err(e) => json_resp(&mut s, 400, json!({"ok": false, "error": e})).await,
        },
        ("GET", "/api/bench") => json_resp(&mut s, 200, app.bench_json()).await,
        _ => json_resp(&mut s, 404, json!({"error": "not found"})).await,
    }
}
