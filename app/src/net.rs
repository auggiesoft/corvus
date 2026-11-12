//! Remote operation: one corvus-sweep owns the radio and serves it (`--serve`, or "Share this radio" in the
//! window); another connects to it by address and PIN and shows it in the same window with the same controls.
//!
//! The split is at the engine's output. The server keeps the radio, the whole engine (looks, smoothing, the
//! learned passband) and the waterfall history, and keeps sweeping when no one is connected. A client gets the
//! board and settings when it connects, the waterfall history, then the traces reduced to its view's columns,
//! the waterfall rows and the stats as they come. Its changes to the sweep settings go to the server, which
//! applies them as if they had been made in its own window and sends back what it applied.
//!
//! Wire: TCP, each message a little-endian u32 length then a postcard-encoded `ToServer` / `ToClient`. The PIN
//! never crosses the wire: the server sends a random nonce, the client answers with HMAC-SHA256(PIN, nonce).
//! Nothing is encrypted (a spectrum is not secret; use a VPN such as Tailscale off the LAN). Failed PINs are
//! limited for everyone, so guessing online is slow; someone who records a login can still try PINs offline.
//! One client at a time. LAN discovery: a UDP broadcast to the same port, answered by every server.

use std::collections::VecDeque;
use std::io::{self, BufWriter, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};

use crate::app::Shared;
use crate::engine::{Detector, EqState, Live, Stats, Trace};
use crate::health;
use crate::hydra::{Board, RxSettings};
use crate::plan::Plan;

pub const PORT: u16 = 30434;
const PROTO: u32 = 2;
const VERSION: &str = env!("CARGO_PKG_VERSION");
/// The waterfall history the server keeps and sends a client when it connects (the window's waterfall depth).
const HISTORY_ROWS: usize = crate::waterfall::ROWS;
/// Failed PINs allowed (from anywhere) per `FAIL_WINDOW` before logins wait.
const FAILS_ALLOWED: usize = 10;
const FAIL_WINDOW: Duration = Duration::from_secs(600);
/// A client that has sent nothing (it pings every 2 s) for this long is gone.
const SILENT_LIMIT: Duration = Duration::from_secs(10);
const DISCOVER_ASK: &[u8] = b"CORVUS-SWEEP?";

// ---- messages ----

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Cmd {
    ClearPeak,
    ResetEq,
    Run(bool),
    /// Open the radio again (after it failed to open or went away).
    Retry,
    /// Run the simulator instead of the radio.
    Sim,
    /// Open another of the server's radios (board, serial; 0 for the first or the only one).
    Switch(Board, u64),
    /// Look for the server's radios again (the picker opened).
    Refresh,
}

/// A radio on the server's USB (or its Pluto), for the client's picker.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Radio {
    pub board: Board,
    pub serial: u64,
    pub label: String,
}

/// The radios this machine can open, as a client's picker lists them.
pub fn radios() -> Vec<Radio> {
    crate::hydra::list_devices().into_iter().chain(crate::app::soapy_found()).map(|f| Radio { board: f.board, serial: f.serial, label: f.label }).collect()
}

/// What the client's window shows: its view, in columns, and which traces.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ViewReq {
    pub lo: f64,
    pub hi: f64,
    pub cols: u32,
    pub det: Detector,
    pub avg: bool,
    pub peak: bool,
    /// The overview strip: the range and its columns.
    pub ov: Option<(f64, f64, u32)>,
    /// Trace updates per second at most.
    pub fps: u32,
}

#[derive(Serialize, Deserialize)]
enum ToServer {
    Auth { mac: [u8; 32] },
    Config { seq: u64, cfg: Shared },
    View(ViewReq),
    Cmd(Cmd),
    Csv,
    Ping,
}

/// How the radio is, for the device chip, the status bar and the footer.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct DevInfo {
    pub kind: SrcKind,
    /// Why it isn't open (`Offline`).
    pub err: String,
    pub dot: Dot,
    pub short: String,
    pub details: String,
    pub footer: String,
    pub pluto_fast: bool,
    pub host_driven: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SrcKind {
    Device,
    Sim,
    #[default]
    Offline,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Dot {
    Ok,
    Caution,
    Info,
    #[default]
    Alarm,
}

/// The server's side of the settings and the radio; sent when it changes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Status {
    pub board: Board,
    pub pluto_lo: (f64, f64, bool),
    pub cfg: Shared,
    /// The last settings change applied from a client: its session and sequence number.
    pub acked: (u64, u64),
    pub plan: Plan,
    pub port: u8,
    pub running: bool,
    pub error: Option<String>,
    pub dev: DevInfo,
    /// The server's name (its host name).
    pub name: String,
    /// The radios it can open, and the serial of the one open (0: the first found).
    pub radios: Vec<Radio>,
    pub serial: u64,
}

/// The engine's state, a few times a second.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct State {
    /// The server's engine clock (s); row and step times are on it.
    pub now: f32,
    pub stats: Stats,
    pub health: health::Summary,
    pub eq_state: EqState,
    pub burst: (f32, f32),
    pub step_t: Vec<f32>,
    pub head: Option<usize>,
}

/// The engine's layout: waterfall rows that follow have `cols` columns spanning `cover`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub generation: u64,
    pub plan: Plan,
    pub nfft: usize,
    pub cols: usize,
    pub cover: (f64, f64),
}

/// A waterfall row, 0.5 dB steps (`q8`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Row {
    pub t: f32,
    pub q: Vec<u8>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Item {
    Layout(Layout),
    Row(Row),
}

/// Traces for the view: the frequencies only when they change, levels as centi-dB (`q16`).
#[derive(Serialize, Deserialize)]
struct Traces {
    xs: Option<Vec<f64>>,
    live: Vec<u8>,
    avg: Option<Vec<u8>>,
    peak: Option<Vec<u8>>,
    ov_xs: Option<Vec<f64>>,
    ov: Option<Vec<u8>>,
}

#[derive(Serialize, Deserialize)]
enum ToClient {
    Hello { proto: u32, version: String, name: String, board: Board, nonce: [u8; 16] },
    Denied(String),
    Welcome { session: u64 },
    Status(Box<Status>),
    State(Box<State>),
    Items(Vec<Item>),
    Traces(Traces),
    Csv(String),
}

// ---- framing, quantising, auth ----

fn send<T: Serialize>(w: &mut impl Write, m: &T) -> io::Result<()> {
    let b = postcard::to_allocvec(m).map_err(io::Error::other)?;
    w.write_all(&(b.len() as u32).to_le_bytes())?;
    w.write_all(&b)
}

fn recv<T: DeserializeOwned>(r: &mut impl Read, max: usize) -> io::Result<T> {
    recv_n(r, max).map(|(m, _)| m)
}

/// A message and its size on the wire.
fn recv_n<T: DeserializeOwned>(r: &mut impl Read, max: usize) -> io::Result<(T, usize)> {
    let mut n = [0u8; 4];
    r.read_exact(&mut n)?;
    let n = u32::from_le_bytes(n) as usize;
    if n > max {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("message of {n} bytes")));
    }
    let mut b = vec![0u8; n];
    r.read_exact(&mut b)?;
    postcard::from_bytes(&b).map(|m| (m, n + 4)).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Waterfall levels in a byte: 0.5 dB steps from −150 dBFS (1) to −23 dBFS (255); 0 is nothing measured.
const Q8_BASE: f32 = -150.0;

fn q8(v: &[f32]) -> Vec<u8> {
    v.iter().map(|&v| if v.is_finite() { ((v - Q8_BASE) * 2.0).round().clamp(1.0, 255.0) as u8 } else { 0 }).collect()
}

pub fn unq8(q: &[u8]) -> Vec<f32> {
    q.iter().map(|&q| if q == 0 { f32::NAN } else { Q8_BASE + q as f32 * 0.5 }).collect()
}

/// Trace levels as centi-dB in two bytes; i16::MIN is nothing measured.
fn q16(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|&v| (if v.is_finite() { (v * 100.0).round().clamp(-32767.0, 32767.0) as i16 } else { i16::MIN }).to_le_bytes()).collect()
}

fn unq16(b: &[u8]) -> Vec<f32> {
    b.as_chunks::<2>()
        .0
        .iter()
        .map(|&c| {
            let q = i16::from_le_bytes(c);
            if q == i16::MIN { f32::NAN } else { q as f32 / 100.0 }
        })
        .collect()
}

fn hmac(key: &[u8], msg: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(k.map(|b| b ^ 0x36));
    inner.update(msg);
    let ih = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(k.map(|b| b ^ 0x5c));
    outer.update(ih);
    outer.finalize().into()
}

/// Random bytes from the OS-seeded hasher keys, the clock and a counter (nonces and PINs, not keys).
fn random_bytes<const N: usize>() -> [u8; N] {
    use std::hash::{BuildHasher, Hasher};
    static CTR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let mut out = [0u8; N];
    for (i, c) in out.chunks_mut(8).enumerate() {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        h.write_u64(CTR.fetch_add(1, Ordering::Relaxed));
        h.write_usize(i);
        let v = h.finish().to_le_bytes();
        c.copy_from_slice(&v[..c.len()]);
    }
    out
}

/// A random six-digit PIN.
pub fn random_pin() -> String {
    format!("{:06}", u64::from_le_bytes(random_bytes::<8>()) % 1_000_000)
}

pub fn host_name() -> String {
    let n = if cfg!(windows) {
        std::env::var("COMPUTERNAME").unwrap_or_default()
    } else {
        std::fs::read_to_string("/etc/hostname").or_else(|_| std::fs::read_to_string("/proc/sys/kernel/hostname")).unwrap_or_default()
    };
    let n = n.trim();
    if n.is_empty() { "corvus".into() } else { n.to_string() }
}

/// This machine's address on its default route (no packet is sent), for showing where to connect.
pub fn local_ip() -> Option<std::net::IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

/// `HOST`, `HOST:PORT`, `IP`, `IP:PORT` (the default port when none is given).
pub fn resolve(addr: &str) -> Result<SocketAddr, String> {
    let a = addr.trim();
    let with_port = if a.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) { a.to_string() } else { format!("{a}:{PORT}") };
    with_port.to_socket_addrs().map_err(|e| format!("{a}: {e}"))?.find(|s| s.is_ipv4()).ok_or_else(|| format!("{a}: no IPv4 address"))
}

// ---- server ----

/// What a client asked the owner of the radio to do.
pub enum Incoming {
    Config { session: u64, seq: u64, cfg: Shared },
    Cmd(Cmd),
}

/// Shared between the owner of the radio (the window or `--serve`) and the connection threads.
struct Hub {
    status: Option<Status>,
    status_ver: u64,
    /// Waterfall history: layouts and rows in order, `first` the sequence number of the front one, and
    /// the layout in force at the front.
    log: VecDeque<Item>,
    rows: usize,
    first: u64,
    base: Option<Layout>,
    incoming: Vec<Incoming>,
    /// The connected client's address.
    client: Option<SocketAddr>,
    sessions: u64,
    fails: VecDeque<Instant>,
}

/// A running server. Dropping it stops it (the client is disconnected; the radio is the owner's).
pub struct Server {
    hub: Arc<Mutex<Hub>>,
    stop: Arc<AtomicBool>,
    pub addr: SocketAddr,
    pub pin: String,
    layout_gen: Option<u64>,
}

impl Server {
    /// Listen on `bind` (and answer discovery on its port). `live` and `epoch` are the engine's.
    pub fn start(bind: SocketAddr, pin: String, live: Arc<Mutex<Live>>, epoch: Instant) -> Result<Server, String> {
        let listener = TcpListener::bind(bind).map_err(|e| format!("can't listen on {bind}: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let bind = listener.local_addr().map_err(|e| e.to_string())?;
        let hub = Arc::new(Mutex::new(Hub {
            status: None,
            status_ver: 0,
            log: VecDeque::new(),
            rows: 0,
            first: 0,
            base: None,
            incoming: Vec::new(),
            client: None,
            sessions: 0,
            fails: VecDeque::new(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (hub, stop, pin) = (hub.clone(), stop.clone(), pin.clone());
            std::thread::Builder::new()
                .name("corvus-serve".into())
                .spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        match listener.accept() {
                            Ok((s, peer)) => {
                                let (hub, stop, pin, live) = (hub.clone(), stop.clone(), pin.clone(), live.clone());
                                let _ = std::thread::Builder::new().name("corvus-client".into()).spawn(move || {
                                    if let Err(e) = connection(s, peer, &hub, &stop, &pin, live, epoch) {
                                        log::info!("client {peer}: {e}");
                                    }
                                });
                            }
                            Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(50)),
                            Err(e) => {
                                log::warn!("accept: {e}");
                                std::thread::sleep(Duration::from_millis(200));
                            }
                        }
                    }
                })
                .map_err(|e| e.to_string())?;
        }
        let port = bind.port();
        match UdpSocket::bind(("0.0.0.0", port)) {
            Ok(u) => {
                let (hub, stop) = (hub.clone(), stop.clone());
                let _ = u.set_read_timeout(Some(Duration::from_millis(300)));
                let name = host_name();
                let _ = std::thread::Builder::new().name("corvus-discover".into()).spawn(move || {
                    let mut buf = [0u8; 64];
                    while !stop.load(Ordering::Relaxed) {
                        if let Ok((n, from)) = u.recv_from(&mut buf)
                            && &buf[..n] == DISCOVER_ASK
                        {
                            let board = hub.lock().status.as_ref().map_or("?".to_string(), |s| format!("{:?}", s.board));
                            let busy = hub.lock().client.is_some() as u8;
                            let _ = u.send_to(format!("CORVUS-SWEEP {PROTO} {port} {board} {busy} {VERSION} {name}").as_bytes(), from);
                        }
                    }
                });
            }
            Err(e) => log::warn!("LAN discovery off: UDP port {port}: {e}"),
        }
        Ok(Server { hub, stop, addr: bind, pin, layout_gen: None })
    }

    /// What clients asked for since the last call.
    pub fn take(&self) -> Vec<Incoming> {
        std::mem::take(&mut self.hub.lock().incoming)
    }

    /// The connected client's address.
    pub fn client(&self) -> Option<SocketAddr> {
        self.hub.lock().client
    }

    /// The owner's side of things, every frame or tick; clients get it only when it changed.
    pub fn publish(&self, st: Status) {
        let mut h = self.hub.lock();
        if h.status.as_ref() != Some(&st) {
            h.status = Some(st);
            h.status_ver += 1;
        }
    }

    /// Waterfall rows as they leave the engine (`l`: the engine, locked, for its layout), at engine time `t`.
    pub fn rows(&mut self, l: &Live, rows: &[Vec<f32>], t: f32) {
        let mut h = self.hub.lock();
        if self.layout_gen != Some(l.generation) {
            self.layout_gen = Some(l.generation);
            h.log.push_back(Item::Layout(Layout { generation: l.generation, plan: l.plan.clone(), nfft: l.settings.nfft, cols: l.wf_cols, cover: l.plan.cover() }));
        }
        for r in rows {
            h.log.push_back(Item::Row(Row { t, q: q8(r) }));
            h.rows += 1;
        }
        while h.rows > HISTORY_ROWS {
            match h.log.pop_front() {
                Some(Item::Layout(lay)) => h.base = Some(lay),
                Some(Item::Row(_)) => h.rows -= 1,
                None => break,
            }
            h.first += 1;
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Bookkeeping for one session, written by its reader thread.
struct SessIn {
    view: Option<ViewReq>,
    view_new: bool,
    csv: bool,
    seen: Instant,
    dead: bool,
}

fn connection(s: TcpStream, peer: SocketAddr, hub: &Arc<Mutex<Hub>>, stop: &AtomicBool, pin: &str, live: Arc<Mutex<Live>>, epoch: Instant) -> io::Result<()> {
    s.set_nonblocking(false)?;
    s.set_nodelay(true)?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    s.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut r = s.try_clone()?;
    let mut w = BufWriter::with_capacity(1 << 16, s.try_clone()?);
    let nonce = random_bytes::<16>();
    let board = hub.lock().status.as_ref().map_or(crate::hydra::board(), |s| s.board);
    send(&mut w, &ToClient::Hello { proto: PROTO, version: VERSION.into(), name: host_name(), board, nonce })?;
    w.flush()?;
    let ToServer::Auth { mac } = recv::<ToServer>(&mut r, 256)? else {
        return Err(io::Error::other("no login"));
    };
    let deny = |w: &mut BufWriter<TcpStream>, why: String| -> io::Result<()> {
        send(w, &ToClient::Denied(why.clone()))?;
        w.flush()?;
        Err(io::Error::other(why))
    };
    {
        let mut h = hub.lock();
        while h.fails.front().is_some_and(|t| t.elapsed() > FAIL_WINDOW) {
            h.fails.pop_front();
        }
        if h.fails.len() >= FAILS_ALLOWED {
            let wait = FAIL_WINDOW.saturating_sub(h.fails.front().unwrap().elapsed()).as_secs() + 1;
            drop(h);
            return deny(&mut w, format!("too many wrong PINs; logins wait {wait} s"));
        }
        let good = hmac(pin.as_bytes(), &nonce);
        if mac.iter().zip(&good).fold(0u8, |a, (x, y)| a | (x ^ y)) != 0 {
            h.fails.push_back(Instant::now());
            drop(h);
            log::warn!("client {peer}: wrong PIN");
            std::thread::sleep(Duration::from_secs(1));
            return deny(&mut w, "wrong PIN".into());
        }
        if let Some(c) = h.client {
            drop(h);
            return deny(&mut w, format!("busy: {c} is connected"));
        }
        h.client = Some(peer);
        h.sessions += 1;
    }
    let session = hub.lock().sessions;
    log::info!("client {peer} connected");
    let res = session_loop(r, &mut w, session, hub, stop, live, epoch);
    hub.lock().client = None;
    log::info!("client {peer} left");
    res
}

fn session_loop(mut r: TcpStream, w: &mut BufWriter<TcpStream>, session: u64, hub: &Arc<Mutex<Hub>>, stop: &AtomicBool, live: Arc<Mutex<Live>>, epoch: Instant) -> io::Result<()> {
    send(w, &ToClient::Welcome { session })?;
    r.set_read_timeout(None)?;
    let sess = Arc::new(Mutex::new(SessIn { view: None, view_new: false, csv: false, seen: Instant::now(), dead: false }));
    {
        let (sess, hub) = (sess.clone(), hub.clone());
        std::thread::Builder::new().name("corvus-client-rx".into()).spawn(move || {
            loop {
                let m = recv::<ToServer>(&mut r, 1 << 20);
                let mut s = sess.lock();
                s.seen = Instant::now();
                match m {
                    Ok(ToServer::Config { seq, cfg }) => hub.lock().incoming.push(Incoming::Config { session, seq, cfg }),
                    Ok(ToServer::Cmd(c)) => hub.lock().incoming.push(Incoming::Cmd(c)),
                    Ok(ToServer::View(v)) => {
                        s.view = Some(v);
                        s.view_new = true;
                    }
                    Ok(ToServer::Csv) => s.csv = true,
                    Ok(ToServer::Ping | ToServer::Auth { .. }) => {}
                    Err(_) => {
                        s.dead = true;
                        return;
                    }
                }
            }
        })?;
    }
    let mut status_ver = 0;
    let mut cursor: Option<u64> = None;
    let mut last_state = Instant::now() - Duration::from_secs(1);
    let mut last_traces = Instant::now() - Duration::from_secs(1);
    let mut last_steps = u64::MAX;
    let mut xs_key: Option<(u64, f64, f64, u32)> = None;
    let mut ov_key: Option<(u64, f64, f64, u32)> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let (view, view_new, csv) = {
            let mut s = sess.lock();
            if s.dead {
                return Ok(());
            }
            if s.seen.elapsed() > SILENT_LIMIT {
                return Err(io::Error::other("silent"));
            }
            let out = (s.view.clone(), s.view_new, s.csv);
            s.view_new = false;
            s.csv = false;
            out
        };
        // the owner's status, then the waterfall history and new rows, a batch at a time
        let (status, items) = {
            let h = hub.lock();
            let status = (h.status_ver != status_ver).then(|| h.status.clone()).flatten();
            status_ver = h.status_ver;
            let mut items = Vec::new();
            let end = h.first + h.log.len() as u64;
            let c = match cursor {
                Some(c) if c >= h.first => c,
                // first time, or fallen behind the history: from its start, with the layout in force there
                _ => {
                    items.extend(h.base.clone().map(Item::Layout));
                    h.first
                }
            };
            let take = (end - c).min(48) as usize;
            items.extend(h.log.range((c - h.first) as usize..(c - h.first) as usize + take).cloned());
            cursor = Some(c + take as u64);
            (status, items)
        };
        if let Some(st) = status {
            send(w, &ToClient::Status(Box::new(st)))?;
        }
        if !items.is_empty() {
            send(w, &ToClient::Items(items))?;
        }
        if last_state.elapsed() >= Duration::from_millis(100) {
            last_state = Instant::now();
            let l = live.lock();
            let st = State {
                now: epoch.elapsed().as_secs_f32(),
                stats: l.stats.clone(),
                health: l.health.clone(),
                eq_state: l.eq_state,
                burst: (l.burst_db, l.noise_sd),
                step_t: l.step_t.clone(),
                head: l.head,
            };
            drop(l);
            send(w, &ToClient::State(Box::new(st)))?;
        }
        if let Some(v) = &view {
            let every = Duration::from_secs_f32(1.0 / v.fps.clamp(1, 60) as f32);
            let l = live.lock();
            let fresh = l.stats.steps != last_steps || view_new || last_traces.elapsed() > Duration::from_secs(1);
            if fresh && last_traces.elapsed() >= every {
                last_traces = Instant::now();
                last_steps = l.stats.steps;
                let cols = v.cols.clamp(1, 8192) as usize;
                let (xs, live_y) = l.trace(Trace::Live, v.lo, v.hi, cols, v.det);
                let avg = v.avg.then(|| q16(&l.trace(Trace::Average, v.lo, v.hi, cols, v.det).1));
                let peak = v.peak.then(|| q16(&l.trace(Trace::Peak, v.lo, v.hi, cols, Detector::Peak).1));
                let key = Some((l.generation, v.lo, v.hi, v.cols));
                let xs = (key != xs_key).then_some(xs);
                xs_key = key;
                let (ov_xs, ov) = match v.ov {
                    Some((a, b, n)) => {
                        let (x, y) = l.trace(Trace::Live, a, b, n.clamp(1, 8192) as usize, Detector::Peak);
                        let key = Some((l.generation, a, b, n));
                        let x = (key != ov_key).then_some(x);
                        ov_key = key;
                        (x, Some(q16(&y)))
                    }
                    None => (None, None),
                };
                drop(l);
                send(w, &ToClient::Traces(Traces { xs, live: q16(&live_y), avg, peak, ov_xs, ov }))?;
            }
        }
        if csv {
            let out = live.lock().to_csv();
            send(w, &ToClient::Csv(out))?;
        }
        w.flush()?;
        std::thread::sleep(Duration::from_millis(8));
    }
}

// ---- client ----

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Connecting,
    Up,
    /// Lost or couldn't connect; it keeps trying.
    Down(String),
    /// The server said no (wrong PIN, busy, too many tries); it doesn't retry by itself.
    Denied(String),
}

#[derive(Default)]
pub struct RemoteTraces {
    pub xs: Vec<f64>,
    pub live: Vec<f32>,
    pub avg: Option<Vec<f32>>,
    pub peak: Option<Vec<f32>>,
    pub ov_xs: Vec<f64>,
    pub ov: Option<Vec<f32>>,
    /// Bumped with every update.
    pub n: u64,
}

/// What the client has from the server, for the window to take each frame.
pub struct Mirror {
    pub phase: Phase,
    pub session: u64,
    pub status: Option<Status>,
    pub status_new: bool,
    pub state: Option<State>,
    pub items: Vec<Item>,
    pub traces: RemoteTraces,
    pub csv: Option<String>,
    /// The server's clock minus ours (s), as tight as seen.
    offset: Option<f32>,
    /// Bytes received, and the rate over the last second or so (bytes/s).
    pub bytes: u64,
    pub rate: f32,
    rate_mark: (Instant, u64),
}

pub struct Client {
    pub addr: String,
    pub mirror: Arc<Mutex<Mirror>>,
    tx: Sender<ToServer>,
    stop: Arc<AtomicBool>,
    retry: Arc<AtomicBool>,
    epoch: Instant,
    last_view: Option<ViewReq>,
}

impl Client {
    pub fn start(addr: &str, pin: &str) -> Client {
        let mirror = Arc::new(Mutex::new(Mirror {
            phase: Phase::Connecting,
            session: 0,
            status: None,
            status_new: false,
            state: None,
            items: Vec::new(),
            traces: RemoteTraces::default(),
            csv: None,
            offset: None,
            bytes: 0,
            rate: 0.0,
            rate_mark: (Instant::now(), 0),
        }));
        let (tx, rx) = channel();
        let stop = Arc::new(AtomicBool::new(false));
        let retry = Arc::new(AtomicBool::new(false));
        let epoch = Instant::now();
        {
            let (mirror, stop, retry, addr, pin) = (mirror.clone(), stop.clone(), retry.clone(), addr.to_string(), pin.to_string());
            let _ = std::thread::Builder::new().name("corvus-remote".into()).spawn(move || manage(&addr, &pin, &mirror, &rx, &stop, &retry, epoch));
        }
        Client { addr: addr.to_string(), mirror, tx, stop, retry, epoch, last_view: None }
    }

    /// The server's engine clock, now.
    pub fn now(&self) -> f32 {
        self.epoch.elapsed().as_secs_f32() + self.mirror.lock().offset.unwrap_or(0.0)
    }

    pub fn phase(&self) -> Phase {
        self.mirror.lock().phase.clone()
    }

    pub fn config(&self, seq: u64, cfg: Shared) {
        let _ = self.tx.send(ToServer::Config { seq, cfg });
    }

    pub fn cmd(&self, c: Cmd) {
        let _ = self.tx.send(ToServer::Cmd(c));
    }

    pub fn csv(&self) {
        let _ = self.tx.send(ToServer::Csv);
    }

    /// The window's view; sent when it changes (and again after a reconnect).
    pub fn view(&mut self, v: ViewReq) {
        if self.last_view.as_ref() != Some(&v) {
            self.last_view = Some(v.clone());
            let _ = self.tx.send(ToServer::View(v));
        }
    }

    /// Try again now (also after the server said no).
    pub fn reconnect(&mut self) {
        self.last_view = None;
        self.retry.store(true, Ordering::Relaxed);
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Connect, read until the connection drops, connect again (every 2 s, sooner when asked), until stopped.
fn manage(addr: &str, pin: &str, mirror: &Arc<Mutex<Mirror>>, rx: &Receiver<ToServer>, stop: &AtomicBool, retry: &AtomicBool, epoch: Instant) {
    let mut last_view: Option<ViewReq> = None;
    while !stop.load(Ordering::Relaxed) {
        retry.store(false, Ordering::Relaxed);
        mirror.lock().phase = Phase::Connecting;
        match session(addr, pin, mirror, rx, stop, retry, epoch, &mut last_view) {
            Ok(()) => return,
            Err((why, denied)) => {
                log::info!("remote {addr}: {why}");
                mirror.lock().phase = if denied { Phase::Denied(why) } else { Phase::Down(why) };
                let t = Instant::now();
                while !stop.load(Ordering::Relaxed) && !retry.load(Ordering::Relaxed) && (denied || t.elapsed() < Duration::from_secs(2)) {
                    // drain what the window sends meanwhile, keeping its view for the next connection
                    while let Ok(m) = rx.try_recv() {
                        if let ToServer::View(v) = m {
                            last_view = Some(v);
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

/// One connection. Ok when stopped; Err(why, the server refused us).
#[allow(clippy::too_many_arguments)]
fn session(addr: &str, pin: &str, mirror: &Arc<Mutex<Mirror>>, rx: &Receiver<ToServer>, stop: &AtomicBool, retry: &AtomicBool, epoch: Instant, last_view: &mut Option<ViewReq>) -> Result<(), (String, bool)> {
    let e = |e: io::Error| (e.to_string(), false);
    let sa = resolve(addr).map_err(|e| (e, false))?;
    let s = TcpStream::connect_timeout(&sa, Duration::from_secs(5)).map_err(e)?;
    s.set_nodelay(true).map_err(e)?;
    s.set_read_timeout(Some(Duration::from_secs(10))).map_err(e)?;
    s.set_write_timeout(Some(Duration::from_secs(10))).map_err(e)?;
    let mut r = s.try_clone().map_err(e)?;
    let mut w = BufWriter::new(s.try_clone().map_err(e)?);
    let ToClient::Hello { proto, nonce, version, .. } = recv::<ToClient>(&mut r, 4096).map_err(e)? else {
        return Err(("not a Corvus Sweep server".into(), true));
    };
    if proto != PROTO {
        return Err((format!("the server runs Corvus Sweep {version}, which speaks another protocol"), true));
    }
    send(&mut w, &ToServer::Auth { mac: hmac(pin.as_bytes(), &nonce) }).map_err(e)?;
    w.flush().map_err(e)?;
    match recv::<ToClient>(&mut r, 4096).map_err(e)? {
        ToClient::Welcome { session } => {
            let mut m = mirror.lock();
            m.session = session;
            m.phase = Phase::Up;
            m.status = None;
            m.state = None;
            m.items.clear();
            m.traces = RemoteTraces::default();
            m.offset = None;
        }
        ToClient::Denied(why) => return Err((why, true)),
        _ => return Err(("unexpected reply".into(), false)),
    }
    // the reader: everything the server sends, into the mirror
    let dead = Arc::new(AtomicBool::new(false));
    let reader = {
        let (mirror, dead) = (mirror.clone(), dead.clone());
        std::thread::Builder::new()
            .name("corvus-remote-rx".into())
            .spawn(move || {
                let why = loop {
                    let (m, n) = match recv_n::<ToClient>(&mut r, 256 << 20) {
                        Ok(m) => m,
                        Err(e) => break e.to_string(),
                    };
                    let mut mi = mirror.lock();
                    mi.bytes += n as u64;
                    let (t, b) = mi.rate_mark;
                    if t.elapsed() >= Duration::from_secs(1) {
                        mi.rate = (mi.bytes - b) as f32 / t.elapsed().as_secs_f32();
                        mi.rate_mark = (Instant::now(), mi.bytes);
                    }
                    match m {
                        ToClient::Status(st) => {
                            mi.status = Some(*st);
                            mi.status_new = true;
                        }
                        ToClient::State(st) => {
                            let off = st.now - epoch.elapsed().as_secs_f32();
                            mi.offset = Some(match mi.offset {
                                Some(o) => off.max(o - 0.01),
                                None => off,
                            });
                            mi.state = Some(*st);
                        }
                        ToClient::Items(it) => {
                            mi.items.extend(it);
                            // a window that isn't drawing (minimised) mustn't grow this without bound
                            let n = mi.items.len();
                            if n > 4 * HISTORY_ROWS {
                                mi.items.drain(..n - 4 * HISTORY_ROWS);
                            }
                        }
                        ToClient::Traces(t) => {
                            let tr = &mut mi.traces;
                            if let Some(xs) = t.xs {
                                tr.xs = xs;
                            }
                            tr.live = unq16(&t.live);
                            tr.avg = t.avg.map(|b| unq16(&b));
                            tr.peak = t.peak.map(|b| unq16(&b));
                            if let Some(xs) = t.ov_xs {
                                tr.ov_xs = xs;
                            }
                            tr.ov = t.ov.map(|b| unq16(&b));
                            tr.n += 1;
                        }
                        ToClient::Csv(c) => mi.csv = Some(c),
                        ToClient::Hello { .. } | ToClient::Denied(_) | ToClient::Welcome { .. } => {}
                    }
                };
                dead.store(true, Ordering::Relaxed);
                why
            })
            .map_err(e)?
    };
    // the writer: what the window sends, a ping every 2 s
    if let Some(v) = last_view.clone() {
        send(&mut w, &ToServer::View(v)).map_err(e)?;
        w.flush().map_err(e)?;
    }
    let mut ping = Instant::now();
    let res = loop {
        if stop.load(Ordering::Relaxed) {
            break Ok(());
        }
        if dead.load(Ordering::Relaxed) || retry.load(Ordering::Relaxed) {
            break Err(());
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(m) => {
                if let ToServer::View(v) = &m {
                    *last_view = Some(v.clone());
                }
                if send(&mut w, &m).and_then(|_| w.flush()).is_err() {
                    break Err(());
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break Ok(()),
        }
        if ping.elapsed() > Duration::from_secs(2) {
            ping = Instant::now();
            if send(&mut w, &ToServer::Ping).and_then(|_| w.flush()).is_err() {
                break Err(());
            }
        }
    };
    let _ = s.shutdown(std::net::Shutdown::Both);
    let why = reader.join().unwrap_or_else(|_| "reader failed".into());
    match res {
        Ok(()) => Ok(()),
        Err(()) if retry.load(Ordering::Relaxed) => Err(("reconnecting".into(), false)),
        Err(()) => Err((format!("connection lost: {why}"), false)),
    }
}

// ---- discovery ----

#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub addr: SocketAddr,
    pub board: String,
    pub busy: bool,
    pub version: String,
    pub name: String,
}

/// Ask the LAN for servers (a broadcast to the default port), collecting answers for `wait`.
pub fn discover(wait: Duration) -> Vec<Found> {
    let mut out: Vec<Found> = Vec::new();
    let Ok(u) = UdpSocket::bind("0.0.0.0:0") else { return out };
    let _ = u.set_broadcast(true);
    let _ = u.set_read_timeout(Some(Duration::from_millis(100)));
    let mut targets = vec![SocketAddr::from(([255, 255, 255, 255], PORT)), SocketAddr::from(([127, 0, 0, 1], PORT))];
    // the subnet's own broadcast address, assuming a /24 (some systems send 255.255.255.255 out one interface only)
    if let Some(std::net::IpAddr::V4(ip)) = local_ip() {
        let o = ip.octets();
        targets.push(SocketAddr::from(([o[0], o[1], o[2], 255], PORT)));
    }
    for t in &targets {
        let _ = u.send_to(DISCOVER_ASK, t);
    }
    let t0 = Instant::now();
    let mut buf = [0u8; 512];
    while t0.elapsed() < wait {
        let Ok((n, from)) = u.recv_from(&mut buf) else { continue };
        let s = String::from_utf8_lossy(&buf[..n]);
        let mut it = s.splitn(7, ' ');
        if it.next() != Some("CORVUS-SWEEP") || it.next() != Some("1") {
            continue;
        }
        let (Some(port), Some(board), Some(busy), Some(version), Some(name)) = (it.next().and_then(|p| p.parse::<u16>().ok()), it.next(), it.next(), it.next(), it.next()) else { continue };
        let addr = SocketAddr::new(from.ip(), port);
        // the same server heard on loopback and on the LAN: keep the LAN address
        if out.iter().any(|f| f.name == name && f.addr.port() == port) {
            if !addr.ip().is_loopback()
                && let Some(f) = out.iter_mut().find(|f| f.name == name && f.addr.port() == port && f.addr.ip().is_loopback())
            {
                f.addr = addr;
            }
            continue;
        }
        out.push(Found { addr, board: board.to_string(), busy: busy == "1", version: version.to_string(), name: name.to_string() });
    }
    out
}

/// The status a headless server or a sharing window publishes, from its parts.
#[allow(clippy::too_many_arguments)]
pub fn status(cfg: &Shared, acked: (u64, u64), plan: &Plan, rx: RxSettings, running: bool, error: Option<String>, dev: DevInfo, radios: &[Radio], serial: u64) -> Status {
    Status {
        board: crate::hydra::board(),
        pluto_lo: crate::plan::pluto_lo(),
        cfg: cfg.clone(),
        acked,
        plan: plan.clone(),
        port: rx.port,
        running,
        error,
        dev,
        name: host_name(),
        radios: radios.to_vec(),
        serial,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_matches_rfc4231() {
        // RFC 4231 test case 2
        let mac = hmac(b"Jefe", b"what do ya want for nothing?");
        let hex: String = mac.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(hex, "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843");
    }

    /// A simulated radio served on loopback: a wrong PIN is refused; the right one gets the settings, the
    /// history, the stats and traces for its view, and its settings change reaches the owner.
    #[test]
    fn client_drives_a_server() {
        use crate::app::Config;
        let cfg = Config::default();
        let plan = cfg.plan_for_test();
        let engine = crate::engine::Engine::new(plan.clone(), cfg.dsp_for_test(), false);
        let _sim = crate::sim::Sim::start(engine.sink(), plan.clone(), RxSettings { port: 0, gain: cfg.gain });
        let mut server = Server::start("127.0.0.1:0".parse().unwrap(), "123456".into(), engine.live.clone(), engine.epoch).unwrap();
        let addr = server.addr.to_string();
        let tick = |server: &mut Server| {
            let mut l = engine.live.lock();
            let rows: Vec<Vec<f32>> = l.rows.drain(..).collect();
            server.rows(&l, &rows, engine.now());
            drop(l);
            server.publish(status(&Shared::of(&cfg), (0, 0), &plan, RxSettings { port: 0, gain: cfg.gain }, true, None, DevInfo::default(), &[], 0));
        };
        let wait = |server: &mut Server, what: &dyn Fn() -> bool| {
            let t = Instant::now();
            while !what() {
                assert!(t.elapsed() < Duration::from_secs(10), "timed out");
                tick(server);
                std::thread::sleep(Duration::from_millis(20));
            }
        };

        let bad = Client::start(&addr, "000000");
        wait(&mut server, &|| matches!(bad.phase(), Phase::Denied(_)));
        assert_eq!(bad.phase(), Phase::Denied("wrong PIN".into()));
        drop(bad);

        let mut c = Client::start(&addr, "123456");
        wait(&mut server, &|| c.phase() == Phase::Up && c.mirror.lock().status.is_some());
        assert_eq!(c.mirror.lock().status.as_ref().unwrap().cfg, Shared::of(&cfg));
        // a second client is turned away while this one is on
        let other = Client::start(&addr, "123456");
        wait(&mut server, &|| matches!(other.phase(), Phase::Denied(_)));
        drop(other);
        c.view(ViewReq { lo: plan.start_hz, hi: plan.stop_hz, cols: 500, det: Detector::Peak, avg: false, peak: true, ov: Some((plan.start_hz, plan.stop_hz, 200)), fps: 30 });
        wait(&mut server, &|| {
            let m = c.mirror.lock();
            let t = &m.traces;
            t.n > 3 && t.live.iter().any(|v| v.is_finite()) && t.xs.len() == t.live.len() && t.peak.as_ref().is_some_and(|p| p.len() == t.xs.len()) && t.ov.as_ref().is_some_and(|o| o.len() == t.ov_xs.len())
        });
        wait(&mut server, &|| c.mirror.lock().state.as_ref().is_some_and(|s| s.stats.passes > 0));
        wait(&mut server, &|| c.mirror.lock().items.iter().any(|i| matches!(i, Item::Row(_))));
        assert!(matches!(c.mirror.lock().items.first(), Some(Item::Layout(_))), "history starts with its layout");
        // the server's clock, as the client sees it
        assert!((c.now() - engine.now()).abs() < 0.5);
        let mut changed = Config::default();
        changed.stop_mhz = 900.0;
        c.config(1, Shared::of(&changed));
        let t = Instant::now();
        let (seq, sh) = loop {
            assert!(t.elapsed() < Duration::from_secs(10), "timed out");
            tick(&mut server);
            if let Some((seq, cfg)) = server.take().into_iter().find_map(|m| if let Incoming::Config { seq, cfg, .. } = m { Some((seq, cfg)) } else { None }) {
                break (seq, cfg);
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(seq, 1);
        assert_eq!(sh, Shared::of(&changed));
        // asked for, the full-resolution spectrum comes as CSV
        c.csv();
        wait(&mut server, &|| c.mirror.lock().csv.is_some());
        assert!(c.mirror.lock().csv.as_ref().unwrap().starts_with("freq_hz,"));
    }

    /// Against a running server (CORVUS_REMOTE=HOST[:PORT], CORVUS_PIN): what a full-width window costs.
    /// `cargo test remote_bandwidth -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn remote_bandwidth() {
        let addr = std::env::var("CORVUS_REMOTE").expect("CORVUS_REMOTE");
        let pin = std::env::var("CORVUS_PIN").expect("CORVUS_PIN");
        println!("discovered: {:?}", discover(Duration::from_millis(700)));
        let mut c = Client::start(&addr, &pin);
        let t = Instant::now();
        while c.mirror.lock().status.is_none() {
            assert!(t.elapsed() < Duration::from_secs(10), "no status: {:?}", c.phase());
            std::thread::sleep(Duration::from_millis(20));
        }
        let st = c.mirror.lock().status.clone().unwrap();
        println!("{} on {}: {:.1}–{:.1} MHz, {}", st.dev.short, st.name, st.plan.start_hz / 1e6, st.plan.stop_hz / 1e6, if st.running { "sweeping" } else { "paused" });
        std::thread::sleep(Duration::from_secs(2));
        let (b0, rows0) = {
            let mut m = c.mirror.lock();
            let rows = m.items.iter().filter(|i| matches!(i, Item::Row(_))).count();
            m.items.clear();
            (m.bytes, rows)
        };
        println!("connect: {:.0} kB, {rows0} history rows", b0 as f32 / 1e3);
        let p = &st.plan;
        c.view(ViewReq { lo: p.start_hz, hi: p.stop_hz, cols: 1600, det: Detector::Peak, avg: false, peak: true, ov: Some((p.start_hz, p.stop_hz, 1600)), fps: 30 });
        let (t0, n0) = (Instant::now(), c.mirror.lock().traces.n);
        std::thread::sleep(Duration::from_secs(10));
        let m = c.mirror.lock();
        let s = t0.elapsed().as_secs_f32();
        let rows = m.items.iter().filter(|i| matches!(i, Item::Row(_))).count();
        println!(
            "steady: {:.0} kB/s, {:.1} trace updates/s, {:.1} rows/s, passes/s {:.1}",
            (m.bytes - b0) as f32 / s / 1e3,
            (m.traces.n - n0) as f32 / s,
            rows as f32 / s,
            m.state.as_ref().map_or(0.0, |st| st.stats.steps_per_s / p.n() as f32)
        );
        let session = m.session;
        drop(m);
        // a settings change goes over, is applied, and comes back with the new plan
        let cfg = st.cfg.clone();
        let mut c2 = crate::app::Config::default();
        cfg.put(&mut c2);
        let want = ((c2.start_mhz + c2.stop_mhz) / 2.0).round();
        c2.stop_mhz = want;
        c.config(1, Shared::of(&c2));
        let t = Instant::now();
        loop {
            assert!(t.elapsed() < Duration::from_secs(10), "change not applied");
            let s = c.mirror.lock().status.clone().unwrap();
            if s.acked == (session, 1) && (s.plan.stop_hz / 1e6 - want).abs() < 1.0 {
                println!("stop set to {want} MHz: applied in {:.0} ms", t.elapsed().as_secs_f32() * 1e3);
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        // and back
        cfg.put(&mut c2);
        c.config(2, Shared::of(&c2));
        std::thread::sleep(Duration::from_millis(800));
    }

    /// Against a running server (CORVUS_REMOTE, CORVUS_PIN): open each of its radios in turn, check it sweeps,
    /// then go back to the one it had. `cargo test remote_switch -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn remote_switch() {
        let addr = std::env::var("CORVUS_REMOTE").expect("CORVUS_REMOTE");
        let pin = std::env::var("CORVUS_PIN").expect("CORVUS_PIN");
        let c = Client::start(&addr, &pin);
        let status = || c.mirror.lock().status.clone();
        let wait = |what: &dyn Fn(&Status) -> bool, secs: u64| -> Option<Status> {
            let t = Instant::now();
            while t.elapsed() < Duration::from_secs(secs) {
                if let Some(s) = status().filter(|s| what(s)) {
                    return Some(s);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            None
        };
        let st0 = wait(&|_| true, 10).expect("no status");
        println!("{} on {}; radios: {:?}", st0.dev.short, st0.name, st0.radios.iter().map(|r| format!("{} {:016X}", r.label, r.serial)).collect::<Vec<_>>());
        let steps = || c.mirror.lock().state.as_ref().map_or(0, |s| s.stats.steps);
        for r in &st0.radios {
            c.cmd(Cmd::Switch(r.board, r.serial));
            let st = wait(&|s| s.serial == r.serial && s.dev.kind != SrcKind::Offline && (s.board == r.board || r.board == Board::Airspy), 15);
            let Some(st) = st else {
                println!("  {} {:016X}: didn't open: {:?}", r.label, r.serial, status().map(|s| s.dev.err));
                continue;
            };
            std::thread::sleep(Duration::from_secs(2));
            let a = steps();
            std::thread::sleep(Duration::from_secs(2));
            println!("  {} {:016X}: {} · {:.0} steps/s · {:.1}–{:.1} MHz", r.label, r.serial, st.dev.short, (steps() - a) as f32 / 2.0, st.plan.start_hz / 1e6, st.plan.stop_hz / 1e6);
        }
        match st0.dev.kind {
            SrcKind::Sim => c.cmd(Cmd::Sim),
            _ => c.cmd(Cmd::Switch(st0.board, st0.serial)),
        }
        let back = wait(&|s| s.board == st0.board && s.dev.kind == st0.dev.kind, 15);
        println!("back: {:?}", back.map(|s| s.dev.short));
    }

    #[test]
    fn levels_survive_the_wire() {
        let v = [f32::NAN, -120.25, -42.0, 3.5];
        let back = unq16(&q16(&v));
        assert!(back[0].is_nan());
        assert_eq!(&back[1..], &[-120.25, -42.0, 3.5]);
        let back = unq8(&q8(&v));
        assert!(back[0].is_nan());
        assert!((back[1] + 120.25).abs() <= 0.25 && (back[2] + 42.0).abs() <= 0.25);
        assert_eq!(back[3], Q8_BASE + 127.5);
    }
}
