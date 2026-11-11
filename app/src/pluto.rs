//! The PlutoSDR family (ADALM-Pluto, Pluto+) over the network: Corvus Sweep talks to corvus-pluto, a small
//! program that runs on the board (../firmware/pluto). The board retunes, captures and tags the blocks
//! itself, in the R82x boards' tag format with two words per IQ sample (see dsp.rs), so the engine handles
//! them like any firmware sweep. Corvus only sends the plan and the gain.
//!
//! The board is reached at --pluto HOST[:PORT] (or CORVUS_PLUTO); the default is the address a Pluto gives
//! itself on its USB network link, 192.168.2.1. corvus-pluto listens on port 30433.
//!
//! corvus-pluto 0.2 sweeps in fast mode when the board allows it (fastlock profiles over direct SPI, ~4x the
//! steps/s) and falls back to the driver path otherwise; CORVUS_PLUTO_MODE=driver asks for the driver path.
//! Blocks come 12-bit packed (pack=12) to save network bandwidth, unpacked here into the usual layout.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::hydra::{BLOCK_WORDS, EMERGENCY, Gain, RxSettings, Sink};
use crate::plan::{Plan, Rate};

pub const PORT: u16 = 30433;
/// Where a Pluto puts itself on its USB network link.
pub const DEFAULT_HOST: &str = "192.168.2.1";

const FRAME_BLOCK: u32 = 1;
const FRAME_TEXT: u32 = 2;
/// A block with 12-bit packed samples: 8 header words, 4088 I, Q pairs in 3 bytes each, 8 trailer words.
const FRAME_PACKED: u32 = 3;
const TAG_BYTES: usize = 2 * crate::hydra::TAG_WORDS;
const PACKED_BYTES: usize = 2 * TAG_BYTES + (BLOCK_WORDS - 2 * crate::hydra::TAG_WORDS) / 2 * 3;
/// The two DMA blocks still holding the previous step after a retune (measured: block 3 is the new band).
const FLUSH_BLOCKS: u8 = 2;

static ADDR: parking_lot::Mutex<Option<String>> = parking_lot::const_mutex(None);

/// Set the board's address (`--pluto`, CORVUS_PLUTO).
pub fn set_addr(a: &str) {
    *ADDR.lock() = Some(a.to_string());
}

/// The board's address as host:port.
pub fn addr() -> String {
    let a = ADDR.lock().clone().or_else(|| std::env::var("CORVUS_PLUTO").ok().filter(|v| !v.is_empty())).unwrap_or_else(|| DEFAULT_HOST.into());
    if a.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) && !a.ends_with(']') { a } else { format!("{a}:{PORT}") }
}

/// An address was given (so the picker can offer the board without seeing it on USB).
pub fn addr_given() -> bool {
    ADDR.lock().is_some() || std::env::var("CORVUS_PLUTO").is_ok_and(|v| !v.is_empty())
}

/// The AD936x's manual RX gain (0–71 dB) from the app's gain setting: the presets' 0–21 spread over the
/// range; manual LNA 0–14, mixer 0–15 and VGA 0–15 summed and scaled onto it.
pub fn gain_db(g: Gain) -> i32 {
    match g {
        Gain::Linearity(v) | Gain::Sensitivity(v) => (v.min(21) as i32 * 71 + 10) / 21,
        Gain::Manual { lna, mixer, vga } => ((lna.min(14) as i32 + mixer.min(15) as i32 + vga.min(15) as i32) * 71 + 22) / 44,
    }
}

/// The analog RX bandwidth for a rate (Rate::pluto_rf_bw): 56 MHz at 61.44 MSPS, down to 3.5 at 3.84.
fn rf_bandwidth(rate: Rate) -> u32 {
    rate.pluto_rf_bw() as u32
}

pub struct PlutoDevice {
    stream: TcpStream,
    pub version: String,
    /// Shown where the other boards show their library: the board's address.
    pub lib_path: PathBuf,
    replies: Receiver<String>,
    alive: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
    plan: Option<Plan>,
    rx: RxSettings,
    /// What the board was last configured with (rate, gain).
    configured: Option<(Rate, i32)>,
    /// How the board sweeps ("fast", "driver"; empty before the first plan), and why not fast if it isn't.
    pub mode: Arc<parking_lot::Mutex<(String, String)>>,
    /// Set while a new plan is on its way: blocks are dropped until the board answers it.
    stale: Arc<AtomicBool>,
}

/// A FRAME_PACKED payload back into the block's 16-bit words (samples sign-extended from 12 bits).
fn unpack12(p: &[u8]) -> Vec<u16> {
    let mut w = Vec::with_capacity(BLOCK_WORDS);
    let word = |b: &[u8; 2]| u16::from_le_bytes(*b);
    w.extend(p[..TAG_BYTES].as_chunks::<2>().0.iter().map(word));
    for c in p[TAG_BYTES..PACKED_BYTES - TAG_BYTES].as_chunks::<3>().0 {
        let i = c[0] as u16 | ((c[1] as u16 & 0xF) << 8);
        let q = (c[1] as u16 >> 4) | ((c[2] as u16) << 4);
        w.push((((i << 4) as i16) >> 4) as u16);
        w.push((((q << 4) as i16) >> 4) as u16);
    }
    w.extend(p[PACKED_BYTES - TAG_BYTES..PACKED_BYTES].as_chunks::<2>().0.iter().map(word));
    w
}

fn reply_value(line: &str, key: &str) -> Option<String> {
    let rest = line.split(&format!(" {key}=")).nth(1)?;
    if let Some(q) = rest.strip_prefix('"') {
        return q.split('"').next().map(str::to_string);
    }
    rest.split_whitespace().next().map(str::to_string)
}

fn read_frame(s: &mut TcpStream, buf: &mut Vec<u8>) -> std::io::Result<u32> {
    let mut h = [0u8; 8];
    s.read_exact(&mut h)?;
    let kind = u32::from_le_bytes(h[..4].try_into().unwrap());
    let n = u32::from_le_bytes(h[4..].try_into().unwrap()) as usize;
    if n > 1 << 20 {
        return Err(std::io::Error::other(format!("frame of {n} bytes")));
    }
    buf.resize(n, 0);
    s.read_exact(buf)?;
    Ok(kind)
}

fn reader_loop(mut s: TcpStream, sink: Sink, replies: Sender<String>, alive: Arc<AtomicBool>, mode: Arc<parking_lot::Mutex<(String, String)>>, stale: Arc<AtomicBool>) {
    let mut buf = Vec::new();
    while alive.load(Ordering::Relaxed) {
        let frame = read_frame(&mut s, &mut buf);
        // blocks of the previous plan, still in flight when a new one was sent: dropped until the board's
        // OK (frames arrive in order), or the engine would count them as gaps against the new plan
        if matches!(frame, Ok(FRAME_BLOCK | FRAME_PACKED)) && stale.load(Ordering::Relaxed) {
            continue;
        }
        match frame {
            Ok(FRAME_BLOCK) if buf.len() == BLOCK_WORDS * 2 => {
                let words: Vec<u16> = buf.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect();
                sink.push(words);
            }
            Ok(FRAME_PACKED) if buf.len() == PACKED_BYTES => sink.push(unpack12(&buf)),
            Ok(FRAME_TEXT) => {
                let t = String::from_utf8_lossy(&buf).into_owned();
                if t.starts_with("ERR") || t.starts_with("NOTE") {
                    log::warn!("corvus-pluto: {t}");
                }
                if t.starts_with("OK SWEEP") || t.starts_with("ERR") {
                    stale.store(false, Ordering::Relaxed);
                }
                if let Some(why) = t.strip_prefix("NOTE fast mode off: ") {
                    *mode.lock() = ("driver".into(), why.to_string());
                }
                let _ = replies.send(t);
            }
            Ok(_) => {}
            Err(e) => {
                if alive.load(Ordering::Relaxed) {
                    log::warn!("corvus-pluto connection: {e}");
                    let _ = replies.send(format!("ERR connection lost: {e}"));
                }
                break;
            }
        }
    }
    alive.store(false, Ordering::Relaxed);
}

impl PlutoDevice {
    pub fn open(sink: Sink, rx: RxSettings) -> Result<PlutoDevice, String> {
        let addr = addr();
        let sa = addr
            .to_socket_addrs()
            .map_err(|e| format!("Pluto address {addr}: {e}"))?
            .next()
            .ok_or_else(|| format!("Pluto address {addr}: no such host"))?;
        let mut stream = TcpStream::connect_timeout(&sa, Duration::from_secs(3)).map_err(|e| {
            format!("no corvus-pluto at {addr} ({e}). Is the board on the network, and corvus-pluto installed on it? See firmware/pluto/README.md")
        })?;
        let _ = stream.set_nodelay(true);
        stream.set_read_timeout(Some(Duration::from_secs(3))).map_err(|e| e.to_string())?;
        let mut buf = Vec::new();
        let hello = match read_frame(&mut stream, &mut buf) {
            Ok(FRAME_TEXT) => String::from_utf8_lossy(&buf).into_owned(),
            Ok(k) => return Err(format!("{addr}: unexpected first frame {k}")),
            Err(e) => return Err(format!("{addr}: no greeting from corvus-pluto ({e}); is another program connected to it?")),
        };
        let hello = hello.strip_prefix("HELLO ").ok_or_else(|| format!("{addr}: not corvus-pluto ({hello})"))?.to_string();
        stream.set_read_timeout(None).map_err(|e| e.to_string())?;
        let fw = hello.split("fw=\"").nth(1).and_then(|r| r.split('"').next()).unwrap_or("").to_string();
        // corvus-pluto 0.2.1+: the LO range this chip tunes, e.g. lo=46000000-6150000000, and steps past it
        // shifted; older boards: the driver's range only
        match reply_value(&format!(" {hello}"), "lo").and_then(|v| v.split_once('-').map(|(a, b)| (a.parse::<f64>(), b.parse::<f64>()))) {
            Some((Ok(a), Ok(b))) => crate::plan::set_pluto_lo(a, b, true),
            _ => crate::plan::set_pluto_lo(crate::plan::PLUTO_DRIVER_LO.0, crate::plan::PLUTO_DRIVER_LO.1, false),
        }
        let prog = hello.split(" model=").next().unwrap_or(&hello).to_string();
        let version = if fw.is_empty() { format!("Pluto {prog}") } else { format!("Pluto {prog} ({fw})") };
        let (tx, replies) = channel();
        let alive = Arc::new(AtomicBool::new(true));
        let mode = Arc::new(parking_lot::Mutex::new((String::new(), String::new())));
        let stale = Arc::new(AtomicBool::new(false));
        let reader = {
            let s = stream.try_clone().map_err(|e| e.to_string())?;
            let alive = alive.clone();
            let mode = mode.clone();
            let stale = stale.clone();
            std::thread::Builder::new().name("pluto-rx".into()).spawn(move || reader_loop(s, sink, tx, alive, mode, stale)).map_err(|e| e.to_string())?
        };
        let stop = stream.try_clone().map_err(|e| e.to_string())?;
        let stop = parking_lot::Mutex::new(stop);
        *EMERGENCY.lock() = Some(Box::new(move || {
            let _ = stop.lock().write_all(b"STOP\n");
        }));
        log::info!("{version} at {addr}");
        Ok(PlutoDevice { stream, version, lib_path: PathBuf::from(format!("network {addr}")), replies, alive, reader: Some(reader), plan: None, rx, configured: None, mode, stale })
    }

    /// Send one command and wait for its reply ("OK ..." or "ERR ...").
    fn cmd(&mut self, line: &str, timeout: Duration) -> Result<String, String> {
        while self.replies.try_recv().is_ok() {}
        if !self.alive.load(Ordering::Relaxed) {
            return Err("the connection to corvus-pluto is closed".into());
        }
        self.stream.write_all(format!("{line}\n").as_bytes()).map_err(|e| format!("corvus-pluto: {e}"))?;
        let word = line.split_whitespace().next().unwrap_or("");
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.replies.recv_timeout(left) {
                Ok(r) if r.starts_with("ERR") => return Err(format!("corvus-pluto: {}", &r[4..])),
                Ok(r) if r.starts_with(&format!("OK {word}")) => return Ok(r),
                Ok(_) => continue,
                Err(_) => return Err(format!("corvus-pluto didn't answer {word}")),
            }
        }
    }

    fn configure(&mut self, rate: Rate) -> Result<(), String> {
        let g = gain_db(self.rx.gain);
        if self.configured != Some((rate, g)) {
            self.cmd(&format!("CONFIG rate={} bw={} gain={g}", rate.adc_hz() as u64, rf_bandwidth(rate)), Duration::from_secs(5))?;
            self.configured = Some((rate, g));
        }
        Ok(())
    }

    pub fn sweep(&mut self, plan: &Plan) -> Result<(), String> {
        self.configure(plan.rate)?;
        let want = if std::env::var("CORVUS_PLUTO_MODE").is_ok_and(|m| m == "driver") { "driver" } else { "fast" };
        self.stale.store(true, Ordering::Relaxed);
        // settle= for corvus-pluto 0.2 (it adds its own flush where it needs one); skip= for 0.1, which ignores
        // settle, mode and pack. Fast mode calibrates its profiles on a new plan first (~0.7 s).
        let reply = self.cmd(
            &format!(
                "SWEEP f0={} step={} n={} dwell={} settle={} skip={} order={} mode={want} pack=12",
                plan.f0,
                plan.step_hz,
                plan.n_steps,
                plan.dwell.max(1),
                plan.skip,
                FLUSH_BLOCKS + plan.skip,
                if plan.order == crate::plan::Order::VcoContinuous { "bounce" } else { "asc" }
            ),
            Duration::from_secs(10),
        )?;
        let mode = reply_value(&reply, "mode").unwrap_or_else(|| "driver".into());
        let note = reply_value(&reply, "note").unwrap_or_default();
        if !note.is_empty() {
            log::warn!("corvus-pluto sweeps through the driver: {note}");
        }
        *self.mode.lock() = (mode, note);
        self.plan = Some(plan.clone());
        Ok(())
    }

    /// The gain changes while the board sweeps.
    pub fn set_rx(&mut self, rx: RxSettings) -> Result<(), String> {
        self.rx = rx;
        let g = gain_db(rx.gain);
        self.cmd(&format!("GAIN {g}"), Duration::from_secs(2))?;
        if let Some((rate, _)) = self.configured {
            self.configured = Some((rate, g));
        }
        Ok(())
    }

    pub fn pause(&mut self) {
        let _ = self.cmd("STOP", Duration::from_secs(2));
    }

    pub fn resume(&mut self) -> Result<(), String> {
        let plan = self.plan.clone().ok_or("no plan")?;
        self.sweep(&plan)
    }
}

impl Drop for PlutoDevice {
    fn drop(&mut self) {
        EMERGENCY.lock().take();
        let _ = self.stream.write_all(b"STOP\n");
        self.alive.store(false, Ordering::Relaxed);
        let _ = self.stream.shutdown(Shutdown::Both);
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gains_span_the_ad936x_range() {
        assert_eq!(gain_db(Gain::Linearity(0)), 0);
        assert_eq!(gain_db(Gain::Sensitivity(21)), 71);
        assert_eq!(gain_db(Gain::Manual { lna: 14, mixer: 15, vga: 15 }), 71);
        for v in 0..=21 {
            assert!((0..=71).contains(&gain_db(Gain::Linearity(v))));
        }
    }

    #[test]
    fn packed_blocks_unpack_to_words() {
        let mut p = vec![0u8; PACKED_BYTES];
        p[0] = 0x5A;
        p[1] = 0xA5;
        // first pair: I = -1 (0xFFF), Q = 0x123
        p[TAG_BYTES] = 0xFF;
        p[TAG_BYTES + 1] = 0x0F | (0x3 << 4);
        p[TAG_BYTES + 2] = 0x12;
        p[PACKED_BYTES - TAG_BYTES] = 0xA5;
        let w = unpack12(&p);
        assert_eq!(w.len(), BLOCK_WORDS);
        assert_eq!(w[0], 0xA55A);
        assert_eq!((w[8] as i16, w[9]), (-1, 0x123));
        assert_eq!(w[BLOCK_WORDS - 8], 0x00A5);
    }

    #[test]
    fn sweep_reply_values() {
        let r = "OK SWEEP f0=1 step=2 n=3 dwell=1 settle=0 order=bounce mode=driver pack=12 note=\"the AD936x isn't on e0006000.spi\"";
        assert_eq!(reply_value(r, "mode").as_deref(), Some("driver"));
        assert_eq!(reply_value(r, "note").as_deref(), Some("the AD936x isn't on e0006000.spi"));
        assert_eq!(reply_value("OK SWEEP f0=1", "mode"), None);
    }

    #[test]
    fn address_gets_the_default_port() {
        set_addr("10.0.0.5");
        assert_eq!(addr(), "10.0.0.5:30433");
        set_addr("pluto.local:1234");
        assert_eq!(addr(), "pluto.local:1234");
        *ADDR.lock() = None;
    }
}
