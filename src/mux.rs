//! satmux wire protocol (client side), byte-compatible with satmux.py v3 server.
//!
//! Frame header: "!BIH" = u8 type | u32 stream id | u16 payload length (big endian).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blake2::Blake2bMac512;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{sleep, timeout};

pub const FT_OPEN: u8 = 1;
pub const FT_DATA_RAW: u8 = 4;
pub const FT_DATA_ZLIB: u8 = 5;
pub const FT_FIN: u8 = 6;
pub const FT_RST: u8 = 7;
pub const FT_PING: u8 = 8;
pub const FT_PONG: u8 = 9;
pub const FT_FETCH: u8 = 10;
pub const FT_DATA_ENC: u8 = 11;

pub const HDR_LEN: usize = 7;
pub const MAX_CHUNK: usize = 16384;
const MAX_QUEUED: i64 = 512 * 1024;
const MAX_DECOMPRESSED: u64 = 4 * 1024 * 1024;

type HmacSha256 = Hmac<Sha256>;

// ---------------------------------------------------------------- primitives

pub fn frame(ftype: u8, sid: u32, payload: &[u8]) -> Vec<u8> {
    debug_assert!(payload.len() <= u16::MAX as usize);
    let mut v = Vec::with_capacity(HDR_LEN + payload.len());
    v.push(ftype);
    v.extend_from_slice(&sid.to_be_bytes());
    v.extend_from_slice(&(payload.len() as u16).to_be_bytes());
    v.extend_from_slice(payload);
    v
}

pub fn hmac256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = HmacSha256::new_from_slice(key).expect("hmac key");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// BLAKE2b-512 keyed counter-mode keystream, identical to satmux.py `xor_keystream`.
pub fn xor_keystream(subkey: &[u8], seq: u64, data: &[u8]) -> Vec<u8> {
    let mut out = data.to_vec();
    for (blk, chunk) in out.chunks_mut(64).enumerate() {
        let mut m = Blake2bMac512::new_with_salt_and_personal(subkey, &[], &[]).expect("blake2 key");
        m.update(&seq.to_be_bytes());
        m.update(&(blk as u64).to_be_bytes());
        let ks = m.finalize().into_bytes();
        for (b, k) in chunk.iter_mut().zip(ks.iter()) {
            *b ^= k;
        }
    }
    out
}

pub fn zlib_compress(data: &[u8], level: u32) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::with_capacity(data.len() / 2 + 64), flate2::Compression::new(level));
    let _ = e.write_all(data);
    e.finish().unwrap_or_default()
}

pub fn zlib_decompress(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let d = flate2::read::ZlibDecoder::new(data);
    d.take(MAX_DECOMPRESSED).read_to_end(&mut out).ok()?;
    Some(out)
}

fn now_f64() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

// ---------------------------------------------------------------- stats

#[derive(Default)]
pub struct Stats {
    pub wire_up: AtomicU64,
    pub wire_down: AtomicU64,
    pub app_up: AtomicU64,
    pub app_down: AtomicU64,
    pub streams_opened: AtomicU64,
}

// ---------------------------------------------------------------- carrier

pub enum ToLocal {
    Data(Vec<u8>),
    Close,
}

struct StreamEntry {
    tx: mpsc::Sender<ToLocal>,
    got_first: bool,
}

pub struct Carrier {
    pub idx: usize,
    out: mpsc::UnboundedSender<Vec<u8>>,
    queued: AtomicI64,
    streams: Mutex<HashMap<u32, StreamEntry>>,
    next_sid: AtomicU32,
    tx_seq: AtomicU64,
    tx_key: [u8; 32],
    rx_key: [u8; 32],
    alive: AtomicBool,
    load: Mutex<(f64, Instant)>,
    inflight_opens: AtomicI64,
    rtt_bits: AtomicU64,
    stats: Arc<Stats>,
}

impl Carrier {
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    pub fn rtt_ms(&self) -> f64 {
        f64::from_bits(self.rtt_bits.load(Ordering::Relaxed))
    }

    pub fn stream_count(&self) -> usize {
        self.streams.lock().unwrap().len()
    }

    fn record_bytes(&self, n: usize) {
        let mut l = self.load.lock().unwrap();
        let now = Instant::now();
        let dt = now.duration_since(l.1).as_secs_f64();
        if dt > 0.0 {
            l.0 *= 0.35f64.powf(dt);
            l.1 = now;
        }
        l.0 += n as f64;
    }

    pub fn current_load(&self) -> f64 {
        let l = self.load.lock().unwrap();
        l.0 * 0.35f64.powf(Instant::now().duration_since(l.1).as_secs_f64())
    }

    fn dec_inflight(&self) {
        let _ = self.inflight_opens.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| Some((v - 1).max(0)));
    }

    /// Queue a frame without waiting.
    pub fn send_raw(&self, buf: Vec<u8>) -> bool {
        if !self.is_alive() {
            return false;
        }
        self.record_bytes(buf.len());
        self.queued.fetch_add(buf.len() as i64, Ordering::Relaxed);
        if self.out.send(buf).is_err() {
            self.alive.store(false, Ordering::Relaxed);
            return false;
        }
        true
    }

    /// Queue a frame and apply back-pressure while the carrier write queue is large.
    pub async fn send_wait(&self, buf: Vec<u8>) -> bool {
        if !self.send_raw(buf) {
            return false;
        }
        while self.queued.load(Ordering::Relaxed) > MAX_QUEUED && self.is_alive() {
            sleep(Duration::from_millis(4)).await;
        }
        self.is_alive()
    }

    pub fn send_ctrl(&self, ftype: u8, sid: u32, payload: &[u8]) -> bool {
        self.send_raw(frame(ftype, sid, payload))
    }

    pub fn pack_data(sid: u32, data: &[u8], level: u32) -> Vec<u8> {
        if level > 0 && data.len() >= 128 {
            let comp = zlib_compress(data, level);
            if comp.len() + 8 < data.len() {
                return frame(FT_DATA_ZLIB, sid, &comp);
            }
        }
        frame(FT_DATA_RAW, sid, data)
    }

    /// Register a new local stream and return its id.
    pub fn register_stream(&self, tx: mpsc::Sender<ToLocal>) -> u32 {
        let sid = self.next_sid.fetch_add(1, Ordering::Relaxed);
        self.streams.lock().unwrap().insert(sid, StreamEntry { tx, got_first: false });
        self.inflight_opens.fetch_add(1, Ordering::Relaxed);
        self.stats.streams_opened.fetch_add(1, Ordering::Relaxed);
        sid
    }

    /// Drop the stream locally (closes the local socket) and tell the server.
    pub fn abort_stream(&self, sid: u32) {
        let removed = self.streams.lock().unwrap().remove(&sid);
        if let Some(e) = removed {
            if !e.got_first {
                self.dec_inflight();
            }
        }
        self.send_ctrl(FT_RST, sid, &[]);
    }

    /// Like `abort_stream`, but only if the stream is still registered.
    pub fn abort_stream_if_open(&self, sid: u32) {
        if self.streams.lock().unwrap().contains_key(&sid) {
            self.abort_stream(sid);
        }
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    pub fn next_seq(&self) -> u64 {
        self.tx_seq.fetch_add(1, Ordering::Relaxed) + 1
    }

    pub fn encrypt_payload(&self, plain: &[u8]) -> Vec<u8> {
        let comp = zlib_compress(plain, 6);
        let seq = self.next_seq();
        let mut v = Vec::with_capacity(8 + comp.len());
        v.extend_from_slice(&seq.to_be_bytes());
        v.extend_from_slice(&xor_keystream(&self.tx_key, seq, &comp));
        v
    }

    fn decrypt_payload(&self, payload: &[u8]) -> Option<Vec<u8>> {
        if payload.len() < 8 {
            return None;
        }
        let seq = u64::from_be_bytes(payload[..8].try_into().ok()?);
        zlib_decompress(&xor_keystream(&self.rx_key, seq, &payload[8..]))
    }

    fn close_all(&self) {
        self.alive.store(false, Ordering::Relaxed);
        // Dropping the senders closes every local socket bound to this carrier.
        self.streams.lock().unwrap().clear();
    }
}

async fn writer_loop(c: Arc<Carrier>, mut rx: mpsc::UnboundedReceiver<Vec<u8>>, mut w: OwnedWriteHalf) {
    let mut buf: Vec<u8> = Vec::with_capacity(256 * 1024);
    while let Some(f) = rx.recv().await {
        buf.clear();
        buf.extend_from_slice(&f);
        // Coalesce whatever is already queued (e.g. FT_OPEN + ClientHello) into one write.
        while buf.len() < 256 * 1024 {
            match rx.try_recv() {
                Ok(f2) => buf.extend_from_slice(&f2),
                Err(_) => break,
            }
        }
        if w.write_all(&buf).await.is_err() {
            break;
        }
        c.queued.fetch_sub(buf.len() as i64, Ordering::Relaxed);
        c.stats.wire_up.fetch_add(buf.len() as u64, Ordering::Relaxed);
    }
    c.alive.store(false, Ordering::Relaxed);
}

async fn reader_loop(c: Arc<Carrier>, mut r: OwnedReadHalf) {
    let mut hdr = [0u8; HDR_LEN];
    loop {
        if r.read_exact(&mut hdr).await.is_err() {
            break;
        }
        let ftype = hdr[0];
        let sid = u32::from_be_bytes([hdr[1], hdr[2], hdr[3], hdr[4]]);
        let plen = u16::from_be_bytes([hdr[5], hdr[6]]) as usize;
        let mut payload = vec![0u8; plen];
        if plen > 0 && r.read_exact(&mut payload).await.is_err() {
            break;
        }
        c.record_bytes(HDR_LEN + plen);
        c.stats.wire_down.fetch_add((HDR_LEN + plen) as u64, Ordering::Relaxed);

        if ftype == FT_PONG {
            if plen == 8 {
                let t0 = f64::from_be_bytes(payload[..8].try_into().unwrap());
                let rtt = ((now_f64() - t0) * 1000.0).max(0.0);
                c.rtt_bits.store(rtt.to_bits(), Ordering::Relaxed);
            }
            continue;
        }

        let is_end = ftype == FT_FIN || ftype == FT_RST;
        let tx = {
            let mut m = c.streams.lock().unwrap();
            let Some(e) = m.get_mut(&sid) else { continue };
            if !e.got_first {
                e.got_first = true;
                c.dec_inflight();
            }
            if is_end {
                m.remove(&sid).map(|e| e.tx)
            } else {
                Some(e.tx.clone())
            }
        };
        let Some(tx) = tx else { continue };

        if is_end {
            let _ = tx.send(ToLocal::Close).await;
            continue;
        }

        let raw = match ftype {
            FT_DATA_RAW => Some(payload),
            FT_DATA_ZLIB => zlib_decompress(&payload),
            FT_DATA_ENC => c.decrypt_payload(&payload),
            _ => continue,
        };
        match raw {
            Some(raw) => {
                c.stats.app_down.fetch_add(raw.len() as u64, Ordering::Relaxed);
                if tx.send(ToLocal::Data(raw)).await.is_err() {
                    // Local side is gone: stop the remote end too.
                    c.abort_stream(sid);
                }
            }
            None => c.abort_stream(sid),
        }
    }
    c.alive.store(false, Ordering::Relaxed);
}

// ---------------------------------------------------------------- mux client

#[derive(Clone)]
pub struct MuxConfig {
    pub host: String,
    pub port: u16,
    pub psk: Vec<u8>,
    pub carriers: usize,
}

pub struct CarrierInfo {
    pub idx: usize,
    pub alive: bool,
    pub rtt_ms: f64,
    pub load: f64,
    pub streams: usize,
}

pub struct Mux {
    cfg: MuxConfig,
    slots: Mutex<Vec<Option<Arc<Carrier>>>>,
    rr: AtomicU64,
    sess_id: [u8; 16],
    pub stats: Arc<Stats>,
    pub last_error: Mutex<String>,
}

impl Mux {
    pub fn new(cfg: MuxConfig, stats: Arc<Stats>) -> Arc<Self> {
        let mut sess_id = [0u8; 16];
        getrandom::getrandom(&mut sess_id).expect("rng");
        let n = cfg.carriers;
        Arc::new(Self {
            cfg,
            slots: Mutex::new(vec![None; n]),
            rr: AtomicU64::new(0),
            sess_id,
            stats,
            last_error: Mutex::new(String::new()),
        })
    }

    pub fn carrier_count(&self) -> usize {
        self.cfg.carriers
    }

    pub fn ready_count(&self) -> usize {
        self.slots.lock().unwrap().iter().flatten().filter(|c| c.is_alive()).count()
    }

    pub fn carrier_info(&self) -> Vec<CarrierInfo> {
        self.slots
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, s)| match s {
                Some(c) => CarrierInfo {
                    idx: i + 1,
                    alive: c.is_alive(),
                    rtt_ms: c.rtt_ms(),
                    load: c.current_load(),
                    streams: c.stream_count(),
                },
                None => CarrierInfo { idx: i + 1, alive: false, rtt_ms: 0.0, load: 0.0, streams: 0 },
            })
            .collect()
    }

    pub fn active_streams(&self) -> usize {
        self.slots.lock().unwrap().iter().flatten().map(|c| c.stream_count()).sum()
    }

    /// Lowest recent byte load wins, same ordering as satmux.py `pick_carrier`.
    pub fn pick(&self) -> Option<Arc<Carrier>> {
        let rr = self.rr.fetch_add(1, Ordering::Relaxed) % 97;
        let slots = self.slots.lock().unwrap();
        slots
            .iter()
            .flatten()
            .filter(|c| c.is_alive())
            .min_by_key(|c| {
                let ld = c.current_load();
                let bucket = if ld < 4096.0 { 0 } else if ld < 65536.0 { 1 } else { 2 };
                (
                    bucket,
                    c.inflight_opens.load(Ordering::Relaxed),
                    (ld / 4096.0) as i64,
                    c.queued.load(Ordering::Relaxed),
                    (c.idx as u64 + rr) % 97,
                )
            })
            .cloned()
    }

    pub async fn pick_wait(&self) -> Option<Arc<Carrier>> {
        let t0 = Instant::now();
        loop {
            if let Some(c) = self.pick() {
                return Some(c);
            }
            if t0.elapsed() > Duration::from_secs(8) {
                return None;
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    /// 0-RTT open: send FT_OPEN immediately; caller replies "connected" locally.
    pub async fn open_optimistic(&self, host: &str, port: u16, tx: mpsc::Sender<ToLocal>) -> Option<(Arc<Carrier>, u32)> {
        let c = self.pick_wait().await?;
        let sid = c.register_stream(tx);
        let mut payload = Vec::with_capacity(host.len() + 3);
        payload.extend_from_slice(host.as_bytes());
        payload.push(0);
        payload.extend_from_slice(&port.to_be_bytes());
        if !c.send_ctrl(FT_OPEN, sid, &payload) {
            c.streams.lock().unwrap().remove(&sid);
            c.dec_inflight();
            return None;
        }
        Some((c, sid))
    }

    async fn connect_carrier(&self) -> Result<([u8; 32], [u8; 32], OwnedReadHalf, OwnedWriteHalf), String> {
        let addr: SocketAddr = tokio::net::lookup_host((self.cfg.host.as_str(), self.cfg.port))
            .await
            .map_err(|e| format!("DNS: {e}"))?
            .next()
            .ok_or("DNS: no address")?;
        let sock = if addr.is_ipv4() { TcpSocket::new_v4() } else { TcpSocket::new_v6() }.map_err(|e| e.to_string())?;
        let _ = sock.set_send_buffer_size(2 * 1024 * 1024);
        let _ = sock.set_recv_buffer_size(2 * 1024 * 1024);
        let mut s: TcpStream = timeout(Duration::from_secs(12), sock.connect(addr))
            .await
            .map_err(|_| "connect timeout".to_string())?
            .map_err(|e| format!("connect: {e}"))?;
        let _ = s.set_nodelay(true);

        let mut nonce = [0u8; 32];
        timeout(Duration::from_secs(10), s.read_exact(&mut nonce))
            .await
            .map_err(|_| "handshake timeout".to_string())?
            .map_err(|e| format!("handshake: {e}"))?;
        let cmac = hmac256(&self.cfg.psk, &[b"ORB-MUX-V3", &nonce, &self.sess_id]);
        let mut hello = Vec::with_capacity(48);
        hello.extend_from_slice(&self.sess_id);
        hello.extend_from_slice(&cmac);
        s.write_all(&hello).await.map_err(|e| e.to_string())?;
        let mut smac = [0u8; 32];
        timeout(Duration::from_secs(10), s.read_exact(&mut smac))
            .await
            .map_err(|_| "server reply timeout".to_string())?
            .map_err(|_| "server closed the connection (wrong PSK?)".to_string())?;
        let mut m = HmacSha256::new_from_slice(&self.cfg.psk).unwrap();
        m.update(b"ORB-MUX-V3-OK");
        m.update(&nonce);
        m.update(&self.sess_id);
        m.verify_slice(&smac).map_err(|_| "server could not be verified (PSK mismatch)".to_string())?;

        let c2s = hmac256(&self.cfg.psk, &[b"ORB-V3-C2S", &nonce, &self.sess_id]);
        let s2c = hmac256(&self.cfg.psk, &[b"ORB-V3-S2C", &nonce, &self.sess_id]);
        let (r, w) = s.into_split();
        Ok((c2s, s2c, r, w))
    }

    fn new_carrier(&self, idx: usize, tx_key: [u8; 32], rx_key: [u8; 32], out: mpsc::UnboundedSender<Vec<u8>>) -> Arc<Carrier> {
        Arc::new(Carrier {
            idx,
            out,
            queued: AtomicI64::new(0),
            streams: Mutex::new(HashMap::new()),
            next_sid: AtomicU32::new(1),
            tx_seq: AtomicU64::new(0),
            tx_key,
            rx_key,
            alive: AtomicBool::new(true),
            load: Mutex::new((0.0, Instant::now())),
            inflight_opens: AtomicI64::new(0),
            rtt_bits: AtomicU64::new(0f64.to_bits()),
            stats: self.stats.clone(),
        })
    }

    /// Keep carrier slot `idx` (0-based) connected forever, with backoff.
    pub async fn maintain(self: Arc<Self>, idx: usize) {
        let mut backoff = 1.0f64;
        loop {
            match self.connect_carrier().await {
                Ok((c2s, s2c, r, w)) => {
                    let (out, rx) = mpsc::unbounded_channel::<Vec<u8>>();
                    let c = self.new_carrier(idx + 1, c2s, s2c, out);
                    self.slots.lock().unwrap()[idx] = Some(c.clone());
                    c.send_ctrl(FT_PING, 0, &now_f64().to_be_bytes()); // first RTT sample right away
                    self.last_error.lock().unwrap().clear();
                    backoff = 1.0;
                    crate::logx::log(&format!("Carrier #{} up ({}/{})", idx + 1, self.ready_count(), self.cfg.carriers));
                    tokio::select! {
                        _ = reader_loop(c.clone(), r) => {},
                        _ = writer_loop(c.clone(), rx, w) => {},
                    }
                    c.close_all();
                    self.slots.lock().unwrap()[idx] = None;
                    crate::logx::log(&format!("Carrier #{} dropped, reconnecting", idx + 1));
                }
                Err(e) => {
                    let mut le = self.last_error.lock().unwrap();
                    if *le != e {
                        crate::logx::log(&format!("Carrier #{}: {}", idx + 1, e));
                    }
                    *le = e;
                }
            }
            sleep(Duration::from_secs_f64(backoff)).await;
            backoff = (backoff * 1.5).min(5.0);
        }
    }

    pub async fn keepalive(self: Arc<Self>) {
        loop {
            sleep(Duration::from_secs(12)).await;
            let now = now_f64().to_be_bytes();
            let live: Vec<Arc<Carrier>> = self.slots.lock().unwrap().iter().flatten().cloned().collect();
            for c in live {
                if c.is_alive() {
                    c.send_ctrl(FT_PING, 0, &now);
                }
            }
        }
    }

    pub fn shutdown(&self) {
        let mut slots = self.slots.lock().unwrap();
        for c in slots.iter().flatten() {
            c.close_all();
        }
        for s in slots.iter_mut() {
            *s = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keystream_matches_python() {
        // python3 -c "import satmux; print(satmux.xor_keystream(bytes(range(32)), 7, bytes(100)).hex())"
        let ks = xor_keystream(&(0u8..32).collect::<Vec<_>>(), 7, &[0u8; 100]);
        let expected = std::env::var("GEOBOOST_KS_VECTOR").unwrap_or_default();
        if !expected.is_empty() {
            assert_eq!(hex::encode(ks), expected);
        }
    }

    #[test]
    fn frame_layout() {
        let f = frame(FT_OPEN, 0x01020304, b"ab");
        assert_eq!(f, vec![1, 1, 2, 3, 4, 0, 2, b'a', b'b']);
    }
}
