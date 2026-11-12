//! Local listeners: SOCKS5 (0-RTT), HTTP proxy / CONNECT (0-RTT) and the 1-RTT ground-fetch gate.

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Notify};
use tokio::time::timeout;

use crate::mux::{frame, Carrier, Mux, ToLocal, FT_FETCH, FT_FIN, MAX_CHUNK};

const SOCKS_OK: &[u8] = &[5, 0, 0, 1, 0, 0, 0, 0, 0, 0];
const SOCKS_FAIL: &[u8] = &[5, 4, 0, 1, 0, 0, 0, 0, 0, 0];
const HTTP_OK: &[u8] = b"HTTP/1.1 200 Connection Established\r\n\r\n";
const HTTP_BAD_GW: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\nContent-Length: 0\r\n\r\n";

pub async fn serve(listener: TcpListener, mux: Arc<Mux>, kind: Kind) {
    loop {
        let Ok((s, _)) = listener.accept().await else { continue };
        let _ = s.set_nodelay(true);
        let mux = mux.clone();
        tokio::spawn(async move {
            match kind {
                Kind::Socks => handle_socks(mux, s).await,
                Kind::Http => handle_http(mux, s).await,
                Kind::Gate => handle_gate(mux, s).await,
            }
        });
    }
}

#[derive(Clone, Copy)]
pub enum Kind {
    Socks,
    Http,
    Gate,
}

// ---------------------------------------------------------------- common tunnel

async fn local_writer(mut rx: mpsc::Receiver<ToLocal>, mut w: OwnedWriteHalf, idle: Option<Duration>) {
    loop {
        let msg = match idle {
            Some(d) => match timeout(d, rx.recv()).await {
                Ok(m) => m,
                Err(_) => break,
            },
            None => rx.recv().await,
        };
        match msg {
            Some(ToLocal::Data(d)) => {
                if w.write_all(&d).await.is_err() {
                    break;
                }
            }
            Some(ToLocal::Close) => break,
            None => {
                // Carrier died or stream was aborted: reset instead of a clean FIN,
                // so the application never mistakes a cut transfer for a complete one.
                #[allow(deprecated)] // linger(0) never blocks: it just sends RST on close
                let _ = w.as_ref().set_linger(Some(Duration::ZERO));
                return;
            }
        }
    }
    let _ = w.shutdown().await;
}

async fn pump<R: AsyncRead + Unpin>(c: Arc<Carrier>, sid: u32, port: u16, r: &mut R, done: Arc<Notify>) {
    // Port 443 carries TLS ciphertext: compressing it only wastes CPU.
    let level = if port == 443 { 0 } else { 6 };
    let mut buf = vec![0u8; MAX_CHUNK];
    loop {
        let res = tokio::select! {
            res = r.read(&mut buf) => res,
            _ = done.notified() => return,
        };
        match res {
            Ok(0) => {
                c.send_ctrl(FT_FIN, sid, &[]);
                return;
            }
            Ok(n) => {
                c.stats_add_up(n);
                if !c.send_wait(Carrier::pack_data(sid, &buf[..n], level)).await {
                    return;
                }
            }
            Err(_) => {
                c.abort_stream(sid);
                return;
            }
        }
    }
}

async fn tunnel<R: AsyncRead + Unpin>(
    mux: Arc<Mux>,
    host: String,
    port: u16,
    mut r: R,
    mut w: OwnedWriteHalf,
    ok_reply: &[u8],
    fail_reply: &[u8],
    first: Option<Vec<u8>>,
) {
    let (tx, rx) = mpsc::channel::<ToLocal>(64);
    let Some((c, sid)) = mux.open_optimistic(&host, port, tx).await else {
        let _ = w.write_all(fail_reply).await;
        let _ = w.shutdown().await;
        return;
    };
    if !ok_reply.is_empty() && w.write_all(ok_reply).await.is_err() {
        c.abort_stream(sid);
        return;
    }
    if let Some(data) = first {
        c.stats_add_up(data.len());
        c.send_raw(Carrier::pack_data(sid, &data, if port == 443 { 0 } else { 6 }));
    }
    let done = Arc::new(Notify::new());
    let done2 = done.clone();
    tokio::spawn(async move {
        local_writer(rx, w, None).await;
        done2.notify_one();
    });
    pump(c, sid, port, &mut r, done).await;
}

// ---------------------------------------------------------------- SOCKS5

async fn handle_socks(mux: Arc<Mux>, s: TcpStream) {
    let (r, mut w) = s.into_split();
    let mut r = BufReader::new(r);
    let res: std::io::Result<(String, u16)> = async {
        let mut h = [0u8; 2];
        r.read_exact(&mut h).await?;
        if h[0] != 5 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        let mut methods = vec![0u8; h[1] as usize];
        r.read_exact(&mut methods).await?;
        w.write_all(&[5, 0]).await?;
        let mut req = [0u8; 4];
        r.read_exact(&mut req).await?;
        if req[1] != 1 {
            w.write_all(&[5, 7, 0, 1, 0, 0, 0, 0, 0, 0]).await?;
            return Err(std::io::ErrorKind::Unsupported.into());
        }
        let host = match req[3] {
            1 => {
                let mut a = [0u8; 4];
                r.read_exact(&mut a).await?;
                Ipv4Addr::from(a).to_string()
            }
            3 => {
                let mut l = [0u8; 1];
                r.read_exact(&mut l).await?;
                let mut d = vec![0u8; l[0] as usize];
                r.read_exact(&mut d).await?;
                String::from_utf8_lossy(&d).into_owned()
            }
            4 => {
                let mut a = [0u8; 16];
                r.read_exact(&mut a).await?;
                Ipv6Addr::from(a).to_string()
            }
            _ => return Err(std::io::ErrorKind::InvalidData.into()),
        };
        let mut p = [0u8; 2];
        r.read_exact(&mut p).await?;
        Ok((host, u16::from_be_bytes(p)))
    }
    .await;
    if let Ok((host, port)) = res {
        tunnel(mux, host, port, r, w, SOCKS_OK, SOCKS_FAIL, None).await;
    }
}

// ---------------------------------------------------------------- HTTP head parsing

struct Head {
    method: String,
    target: String,
    proto: String,
    headers: Vec<String>, // without CRLF
}

async fn read_head<R: AsyncBufReadExt + Unpin>(r: &mut R) -> Option<Head> {
    let mut line = String::new();
    timeout(Duration::from_secs(10), r.read_line(&mut line)).await.ok()?.ok()?;
    let mut parts = line.trim_end().splitn(3, ' ');
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let proto = parts.next().unwrap_or("HTTP/1.1").to_string();
    let mut headers = Vec::new();
    let mut total = 0usize;
    loop {
        let mut h = String::new();
        let n = timeout(Duration::from_secs(10), r.read_line(&mut h)).await.ok()?.ok()?;
        total += n;
        if n == 0 || h == "\r\n" || h == "\n" || total > 64 * 1024 || headers.len() > 200 {
            break;
        }
        headers.push(h.trim_end_matches(['\r', '\n']).to_string());
    }
    Some(Head { method, target, proto, headers })
}

fn split_host_port(s: &str, default_port: u16) -> Option<(String, u16)> {
    if let Some(rest) = s.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = rest[..end].to_string();
        let port = rest[end + 1..].strip_prefix(':').map(|p| p.parse().ok()).unwrap_or(Some(default_port))?;
        return Some((host, port));
    }
    match s.rsplit_once(':') {
        Some((h, p)) if !h.contains(':') => Some((h.to_string(), p.parse().ok()?)),
        _ => Some((s.to_string(), default_port)),
    }
}

fn header_name(h: &str) -> String {
    h.split(':').next().unwrap_or("").trim().to_ascii_lowercase()
}

// ---------------------------------------------------------------- HTTP proxy

async fn handle_http(mux: Arc<Mux>, s: TcpStream) {
    let (r, mut w) = s.into_split();
    let mut r = BufReader::new(r);
    let Some(head) = read_head(&mut r).await else { return };

    if head.method.eq_ignore_ascii_case("CONNECT") {
        let Some((host, port)) = split_host_port(&head.target, 443) else {
            let _ = w.write_all(HTTP_BAD_GW).await;
            return;
        };
        tunnel(mux, host, port, r, w, HTTP_OK, HTTP_BAD_GW, None).await;
        return;
    }

    // Plain HTTP request in absolute-form (proxy style) or origin-form with Host header.
    let (hostport, path) = if let Some(rest) = head.target.strip_prefix("http://") {
        match rest.find('/') {
            Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
            None => (rest.to_string(), "/".to_string()),
        }
    } else if head.target.starts_with('/') {
        let host = head
            .headers
            .iter()
            .find(|h| header_name(h) == "host")
            .and_then(|h| h.split_once(':').map(|(_, v)| v.trim().to_string()))
            .unwrap_or_default();
        (host, head.target.clone())
    } else {
        let _ = w.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\nContent-Length: 0\r\n\r\n").await;
        return;
    };
    let Some((host, port)) = split_host_port(&hostport, 80) else { return };
    if host.is_empty() {
        return;
    }
    // One upstream per client connection: force Connection: close so a reused
    // proxy connection never sends a second request to the wrong host.
    let mut req = format!("{} {} {}\r\n", head.method, path, head.proto);
    for h in &head.headers {
        let n = header_name(h);
        if n.starts_with("proxy-") || n == "connection" || n == "keep-alive" {
            continue;
        }
        req.push_str(h);
        req.push_str("\r\n");
    }
    req.push_str("Connection: close\r\n\r\n");
    tunnel(mux, host, port, r, w, b"", HTTP_BAD_GW, Some(req.into_bytes())).await;
}

// ---------------------------------------------------------------- 1-RTT ground fetch gate

async fn handle_gate(mux: Arc<Mux>, s: TcpStream) {
    let (r, mut w) = s.into_split();
    let mut r = BufReader::new(r);
    let Some(head) = read_head(&mut r).await else { return };
    let url = head.target.trim_start_matches('/').to_string();
    if !(url.starts_with("https://") || url.starts_with("http://")) {
        let _ = w
            .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\nPass /https://<host>/<path>\n")
            .await;
        return;
    }
    let Some(c) = mux.pick_wait().await else {
        let _ = w.write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\nNo warm carrier\n").await;
        return;
    };
    let (tx, rx) = mpsc::channel::<ToLocal>(64);
    let sid = c.register_stream(tx);
    let mut lines = vec![head.method.to_ascii_uppercase(), url];
    lines.extend(head.headers.iter().map(|h| h.trim().to_string()));
    let raw = lines.join("\n");
    let payload = c.encrypt_payload(raw.as_bytes());
    if payload.len() > u16::MAX as usize {
        c.abort_stream(sid);
        let _ = w.write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\nConnection: close\r\n\r\n").await;
        return;
    }
    c.stats_add_up(raw.len());
    if !c.send_raw(frame(FT_FETCH, sid, &payload)) {
        let _ = w.write_all(HTTP_BAD_GW).await;
        return;
    }
    drop(r);
    local_writer(rx, w, Some(Duration::from_secs(30))).await;
    // If we timed out the stream may still be registered: clean it up.
    c.abort_stream_if_open(sid);
}

impl Carrier {
    pub fn stats_add_up(&self, n: usize) {
        self.stats().app_up.fetch_add(n as u64, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::split_host_port;
    #[test]
    fn host_port() {
        assert_eq!(split_host_port("example.com:443", 80), Some(("example.com".into(), 443)));
        assert_eq!(split_host_port("example.com", 80), Some(("example.com".into(), 80)));
        assert_eq!(split_host_port("[::1]:8443", 80), Some(("::1".into(), 8443)));
        assert_eq!(split_host_port("[::1]", 80), Some(("::1".into(), 80)));
    }
}
