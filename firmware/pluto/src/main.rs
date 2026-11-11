//! corvus-pluto: the Corvus sweep for the PlutoSDR family (ADALM-Pluto, Pluto+), running on the board.
//!
//! It drives the AD936x through the kernel's IIO sysfs files and capture device (no libiio), retunes the RX
//! LO step by step, captures a block right after each retune and sends it to Corvus Sweep over TCP as a
//! ready-made Corvus sweep block: 8192 16-bit words, the same 8-word header and trailer tags the RFOne
//! firmware writes, and between them 4088 IQ samples as I, Q pairs of 12-bit values sign-extended to 16
//! bits. Corvus Sweep then handles the board like any firmware sweep.
//!
//! After a retune the capture's two DMA blocks still hold the previous step (measured: the third block is
//! the new band), so each step reads and drops them first (`skip`, 2 by default). Retuning costs about 1 ms
//! (the driver's synthesizer writes and VCO calibration), 20 ms where the driver reloads its gain table
//! (around 1.3 and 4 GHz); passes go back and forth so the sweep never wraps from 6 GHz to 70 MHz.
//!
//! Protocol (TCP, default port 30433). Client to board, text lines:
//!   CONFIG rate=<Hz> bw=<Hz> gain=<dB>                         sample rate, RF bandwidth, manual RX gain
//!   SWEEP f0=<Hz> step=<Hz> n=<steps> [dwell=<blocks>] [skip=<blocks>] [order=bounce|asc]
//!   GAIN <dB>                                                  takes effect at once, also mid-sweep
//!   STOP                                                       stop sweeping (the connection stays)
//! Board to client, frames of [u32 type][u32 length][payload], little-endian:
//!   1  one sweep block (16384 bytes)
//!   2  a text line: "HELLO ...", "OK ...", "ERR ..."
//! Tags: step index, block within the step, pass counter (one per pass, either direction), the LO in Hz
//! (low 32 bits), and a block clock: time since the sweep started in units of one block's duration.
//!
//! `corvus-pluto bench` measures retune and capture timing on the board instead.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const PORT: u16 = 30433;

/// A Corvus sweep block: 8 tag words, 8176 sample words (4088 IQ pairs), 8 tag words.
const BLOCK_WORDS: usize = 8192;
const TAG_WORDS: usize = 8;
const IQ_PER_BLOCK: usize = (BLOCK_WORDS - 2 * TAG_WORDS) / 2;
const BLOCK_BYTES: usize = BLOCK_WORDS * 2;
const IQ_BYTES: usize = IQ_PER_BLOCK * 4;

const FRAME_BLOCK: u32 = 1;
const FRAME_TEXT: u32 = 2;

// ---------------------------------------------------------------------------------------------------------
// IIO

fn find_iio(name: &str) -> Result<PathBuf, String> {
    for e in fs::read_dir("/sys/bus/iio/devices").map_err(|e| format!("/sys/bus/iio/devices: {e}"))?.flatten() {
        let p = e.path();
        if fs::read_to_string(p.join("name")).map(|n| n.trim() == name).unwrap_or(false) {
            return Ok(p);
        }
    }
    Err(format!("no IIO device named {name}"))
}

fn write_attr(path: &Path, val: &str) -> Result<(), String> {
    fs::write(path, val).map_err(|e| format!("{} = {val}: {e}", path.display()))
}

fn read_attr(path: &Path) -> String {
    fs::read_to_string(path).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// An attribute kept open for fast repeated writes (the LO, the gain): each write at offset 0 is one
/// sysfs store.
struct FastAttr {
    file: File,
    path: PathBuf,
}

impl FastAttr {
    fn open(path: PathBuf) -> Result<FastAttr, String> {
        let file = OpenOptions::new().read(true).write(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(FastAttr { file, path })
    }

    fn set(&self, val: u64) -> Result<(), String> {
        let s = val.to_string();
        self.file.write_at(s.as_bytes(), 0).map(|_| ()).map_err(|e| format!("{} = {s}: {e}", self.path.display()))
    }
}

/// The AD936x: the phy (tuning, gain, rates) and the RX capture core with its buffer.
struct Radio {
    phy: PathBuf,
    rx: PathBuf,
    rx_lo: FastAttr,
    gain: FastAttr,
    dev: Option<File>,
    rate: u32,
}

impl Radio {
    fn open() -> Result<Radio, String> {
        let phy = find_iio("ad9361-phy")?;
        let rx = find_iio("cf-ad9361-lpc")?;
        let rx_lo = FastAttr::open(phy.join("out_altvoltage0_RX_LO_frequency"))?;
        let gain = FastAttr::open(phy.join("in_voltage0_hardwaregain"))?;
        let rate = read_attr(&phy.join("in_voltage_sampling_frequency")).parse().unwrap_or(0);
        Ok(Radio { phy, rx, rx_lo, gain, dev: None, rate })
    }

    fn describe(&self) -> String {
        let fw = fs::read_to_string("/opt/VERSIONS").ok().and_then(|v| v.lines().next().map(|l| l.trim().to_string())).unwrap_or_default();
        let model = fs::read_to_string("/sys/firmware/devicetree/base/model").unwrap_or_default().trim_end_matches('\0').trim().to_string();
        format!("corvus-pluto {VERSION} model=\"{model}\" fw=\"{fw}\"")
    }

    /// Sample rate, analog bandwidth and manual gain; DC and quadrature tracking on; the TX LO off (its
    /// leakage lands in the RX band).
    fn configure(&mut self, rate: u32, bw: u32, gain_db: i32) -> Result<(), String> {
        self.stop_capture();
        write_attr(&self.phy.join("in_voltage_sampling_frequency"), &rate.to_string())?;
        write_attr(&self.phy.join("in_voltage_rf_bandwidth"), &bw.to_string())?;
        let _ = write_attr(&self.phy.join("out_altvoltage1_TX_LO_powerdown"), "1");
        for a in ["in_voltage_quadrature_tracking_en", "in_voltage_rf_dc_offset_tracking_en", "in_voltage_bb_dc_offset_tracking_en"] {
            let _ = write_attr(&self.phy.join(a), "1");
        }
        write_attr(&self.phy.join("in_voltage0_gain_control_mode"), "manual")?;
        self.set_gain(gain_db)?;
        self.rate = read_attr(&self.phy.join("in_voltage_sampling_frequency")).parse().unwrap_or(rate);
        Ok(())
    }

    fn set_gain(&self, db: i32) -> Result<(), String> {
        self.gain.set(db.clamp(0, 73) as u64)
    }

    fn tune(&self, hz: u64) -> Result<(), String> {
        self.rx_lo.set(hz)
    }

    /// Enable I and Q and the buffer: two DMA blocks of one sweep block's samples each.
    fn start_capture(&mut self) -> Result<(), String> {
        if self.dev.is_some() {
            return Ok(());
        }
        let se = self.rx.join("scan_elements");
        write_attr(&self.rx.join("buffer/enable"), "0")?;
        write_attr(&se.join("in_voltage0_en"), "1")?;
        write_attr(&se.join("in_voltage1_en"), "1")?;
        write_attr(&self.rx.join("buffer/length"), &(2 * IQ_PER_BLOCK).to_string())?;
        let _ = write_attr(&self.rx.join("buffer/watermark"), &IQ_PER_BLOCK.to_string());
        let devname = self.rx.file_name().unwrap().to_string_lossy().to_string();
        let dev = File::open(format!("/dev/{devname}")).map_err(|e| format!("/dev/{devname}: {e}"))?;
        write_attr(&self.rx.join("buffer/enable"), "1")?;
        self.dev = Some(dev);
        Ok(())
    }

    fn stop_capture(&mut self) {
        if self.dev.take().is_some() {
            let _ = write_attr(&self.rx.join("buffer/enable"), "0");
        }
    }

    /// One block's worth of IQ bytes (4088 samples) into `out`.
    fn read_iq(&mut self, out: &mut [u8]) -> Result<(), String> {
        let dev = self.dev.as_mut().ok_or("capture not started")?;
        dev.read_exact(&mut out[..IQ_BYTES]).map_err(|e| format!("read: {e}"))
    }
}

impl Drop for Radio {
    fn drop(&mut self) {
        self.stop_capture();
    }
}

// ---------------------------------------------------------------------------------------------------------
// Blocks

struct Tag {
    step: u16,
    block: u16,
    sweep: u32,
    freq_hz: u32,
    dma_count: u32,
}

/// Wrap `iq` (IQ_BYTES) in the Corvus header and trailer: the words the RFOne firmware writes.
fn make_block(iq: &[u8], t: &Tag, out: &mut [u8]) {
    let h: [u16; TAG_WORDS] = [0xA55A, 0x5357, t.step, t.block, t.sweep as u16, (t.sweep >> 16) as u16, t.freq_hz as u16, (t.freq_hz >> 16) as u16];
    let r: [u16; TAG_WORDS] = [0x5AA5, 0, t.step, t.block, t.dma_count as u16, (t.dma_count >> 16) as u16, t.freq_hz as u16, (t.freq_hz >> 16) as u16];
    for (i, w) in h.iter().enumerate() {
        out[2 * i..2 * i + 2].copy_from_slice(&w.to_le_bytes());
    }
    out[2 * TAG_WORDS..2 * TAG_WORDS + IQ_BYTES].copy_from_slice(&iq[..IQ_BYTES]);
    let tail = 2 * (BLOCK_WORDS - TAG_WORDS);
    for (i, w) in r.iter().enumerate() {
        out[tail + 2 * i..tail + 2 * i + 2].copy_from_slice(&w.to_le_bytes());
    }
}

/// Mean power of an IQ block in dB (12-bit full scale = 0 dB), for the bench.
fn iq_power_db(iq: &[u8]) -> f64 {
    let n = IQ_PER_BLOCK as f64;
    let pairs = || iq[..IQ_BYTES].as_chunks::<4>().0.iter().map(|s| (i16::from_le_bytes([s[0], s[1]]) as f64, i16::from_le_bytes([s[2], s[3]]) as f64));
    let (mi, mq) = pairs().fold((0.0, 0.0), |(a, b), (i, q)| (a + i / n, b + q / n));
    let acc: f64 = pairs().map(|(i, q)| (i - mi).powi(2) + (q - mq).powi(2)).sum();
    10.0 * (acc / n / (2048.0 * 2048.0) + 1e-20).log10()
}

// ---------------------------------------------------------------------------------------------------------
// Sweeping

#[derive(Clone, Copy, Debug)]
struct SweepPlan {
    f0: u64,
    step: u64,
    n: u32,
    dwell: u16,
    skip: u16,
    /// Odd passes walk the steps backwards (no 6 GHz -> 70 MHz wrap).
    bounce: bool,
}

impl SweepPlan {
    /// Step index for position `i` of pass `pass`.
    fn index(&self, pass: u32, i: u32) -> u32 {
        if self.bounce && pass % 2 == 1 { self.n - 1 - i } else { i }
    }

    fn freq(&self, step: u32) -> u64 {
        self.f0 + step as u64 * self.step
    }
}

/// The sweep state of one connection, and the radio it drives.
struct Sweeper<'a> {
    radio: &'a mut Radio,
    plan: Option<SweepPlan>,
    /// Pass counter for the tags: bumped on every pass and every new plan.
    sweep: u32,
    /// Passes of the current plan (direction parity).
    pass: u32,
    t0: Instant,
    /// The LO now set, so the turnaround of a back-and-forth sweep isn't retuned.
    lo: Option<u64>,
    iq: Vec<u8>,
}

impl Sweeper<'_> {
    /// Apply a command and return its reply line.
    fn apply(&mut self, cmd: Result<Cmd, String>) -> String {
        let r = match cmd {
            Err(e) => Err(e),
            Ok(Cmd::Config { rate, bw, gain }) => {
                self.plan = None;
                self.lo = None;
                self.radio.configure(rate, bw, gain).map(|_| format!("OK CONFIG rate={} bw={bw} gain={gain}", self.radio.rate))
            }
            Ok(Cmd::Gain(db)) => self.radio.set_gain(db).map(|_| format!("OK GAIN {db}")),
            Ok(Cmd::Stop) => {
                self.plan = None;
                self.radio.stop_capture();
                Ok("OK STOP".into())
            }
            Ok(Cmd::Sweep(p)) => self.radio.start_capture().map(|_| {
                self.plan = Some(p);
                self.sweep = self.sweep.wrapping_add(1);
                self.pass = 0;
                self.t0 = Instant::now();
                format!("OK SWEEP f0={} step={} n={} dwell={} skip={} order={}", p.f0, p.step, p.n, p.dwell, p.skip, if p.bounce { "bounce" } else { "asc" })
            }),
        };
        r.unwrap_or_else(|e| format!("ERR {e}"))
    }

    /// One pass of the current plan. Commands are checked every few steps: a gain change applies in
    /// place, anything else ends the pass.
    fn pass(&mut self, cmds: &Receiver<Result<Cmd, String>>, out: &SyncSender<Vec<u8>>, alive: &AtomicBool) -> Result<(), String> {
        let Some(p) = self.plan else { return Ok(()) };
        let block_s = IQ_PER_BLOCK as f64 / self.radio.rate.max(1) as f64;
        for i in 0..p.n {
            if !alive.load(Ordering::Relaxed) {
                return Ok(());
            }
            let step = p.index(self.pass, i);
            let f = p.freq(step);
            if self.lo != Some(f) {
                self.lo = None;
                self.radio.tune(f)?;
                self.lo = Some(f);
            }
            for _ in 0..p.skip {
                self.radio.read_iq(&mut self.iq)?;
            }
            for b in 0..p.dwell {
                self.radio.read_iq(&mut self.iq)?;
                let dma = (self.t0.elapsed().as_secs_f64() / block_s) as u32;
                let mut buf = vec![0u8; 8 + BLOCK_BYTES];
                buf[..4].copy_from_slice(&FRAME_BLOCK.to_le_bytes());
                buf[4..8].copy_from_slice(&(BLOCK_BYTES as u32).to_le_bytes());
                make_block(&self.iq, &Tag { step: step as u16, block: b, sweep: self.sweep, freq_hz: f as u32, dma_count: dma }, &mut buf[8..]);
                if out.send(buf).is_err() {
                    return Ok(());
                }
            }
            if i % 16 == 15 {
                match cmds.try_recv() {
                    Ok(Ok(Cmd::Gain(db))) => {
                        let _ = out.send(text(&self.apply(Ok(Cmd::Gain(db)))));
                    }
                    Ok(c) => {
                        let _ = out.send(text(&self.apply(c)));
                        return Ok(());
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }
        }
        self.pass += 1;
        self.sweep = self.sweep.wrapping_add(1);
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------
// Bench

fn bench() -> Result<(), String> {
    let mut r = Radio::open()?;
    println!("{}", r.describe());
    let lo0 = read_attr(&r.phy.join("out_altvoltage0_RX_LO_frequency"));
    r.configure(61_440_000, 56_000_000, 40)?;
    println!("rate {} bw {}", r.rate, read_attr(&r.phy.join("in_voltage_rf_bandwidth")));
    r.start_capture()?;
    let mut iq = vec![0u8; IQ_BYTES];

    for _ in 0..8 {
        r.read_iq(&mut iq)?;
    }
    let t = Instant::now();
    for _ in 0..200 {
        r.read_iq(&mut iq)?;
    }
    println!("steady read: {:.1} us per block (its duration {:.1} us)", t.elapsed().as_secs_f64() / 200.0 * 1e6, IQ_PER_BLOCK as f64 / r.rate as f64 * 1e6);

    for (a, b) in [(1_000_000_000u64, 1_040_000_000u64), (100_000_000, 2_400_000_000), (70_000_000, 5_900_000_000)] {
        let mut ts = Vec::new();
        for i in 0..20 {
            let t = Instant::now();
            r.tune(if i % 2 == 0 { a } else { b })?;
            ts.push(t.elapsed().as_secs_f64() * 1e6);
        }
        ts.sort_by(|x, y| x.total_cmp(y));
        println!("retune {:>4} <-> {:>4} MHz: median {:7.0} us, max {:7.0} us", a / 1_000_000, b / 1_000_000, ts[10], ts[19]);
    }

    println!("blocks after a retune (dB), busy 98 MHz / quiet 1420 MHz:");
    for f in [98_000_000u64, 1_420_000_000, 98_000_000, 1_420_000_000] {
        std::thread::sleep(Duration::from_millis(20));
        r.tune(f)?;
        let p: Vec<String> = (0..5).map(|_| r.read_iq(&mut iq).map(|_| format!("{:6.1}", iq_power_db(&iq)))).collect::<Result<_, _>>()?;
        println!("  {:>6.1} MHz: {}", f as f64 / 1e6, p.join(" "));
    }

    let plan = SweepPlan { f0: 70_000_000, step: 40_000_000, n: 149, dwell: 1, skip: 2, bounce: false };
    for bounce in [false, true] {
        let p = SweepPlan { bounce, ..plan };
        let mut lo = None;
        let t = Instant::now();
        for pass in 0..4 {
            for i in 0..p.n {
                let f = p.freq(p.index(pass, i));
                if lo != Some(f) {
                    r.tune(f)?;
                    lo = Some(f);
                }
                for _ in 0..=p.skip {
                    r.read_iq(&mut iq)?;
                }
            }
        }
        let steps = 4.0 * p.n as f64 / t.elapsed().as_secs_f64();
        println!("70-6000 MHz, 40 MHz steps, {}: {steps:.0} steps/s, {:.1} GHz/s", if bounce { "back and forth" } else { "ascending     " }, steps * 0.04);
    }
    r.stop_capture();
    let _ = write_attr(&r.phy.join("out_altvoltage0_RX_LO_frequency"), &lo0);
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------
// Server

enum Cmd {
    Config { rate: u32, bw: u32, gain: i32 },
    Sweep(SweepPlan),
    Gain(i32),
    Stop,
}

fn kv<'a>(words: &[&'a str], key: &str) -> Option<&'a str> {
    words.iter().find_map(|w| w.strip_prefix(key).and_then(|v| v.strip_prefix('=')))
}

fn parse_cmd(line: &str) -> Result<Cmd, String> {
    let w: Vec<&str> = line.split_whitespace().collect();
    let num = |k: &str| kv(&w, k).and_then(|v| v.parse::<u64>().ok());
    let need = |k: &str| num(k).ok_or(format!("missing or bad {k}="));
    match w.first().copied() {
        Some("CONFIG") => Ok(Cmd::Config { rate: need("rate")? as u32, bw: need("bw")? as u32, gain: need("gain")? as i32 }),
        Some("SWEEP") => Ok(Cmd::Sweep(SweepPlan {
            f0: need("f0")?,
            step: need("step")?,
            n: need("n")?.clamp(1, 65_535) as u32,
            dwell: num("dwell").unwrap_or(1).clamp(1, 64) as u16,
            skip: num("skip").unwrap_or(2).min(64) as u16,
            bounce: kv(&w, "order") != Some("asc"),
        })),
        Some("GAIN") => w.get(1).and_then(|v| v.parse().ok()).map(Cmd::Gain).ok_or("GAIN <dB>".into()),
        Some("STOP") => Ok(Cmd::Stop),
        _ => Err(format!("unknown command: {line}")),
    }
}

fn text(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + s.len());
    v.extend_from_slice(&FRAME_TEXT.to_le_bytes());
    v.extend_from_slice(&(s.len() as u32).to_le_bytes());
    v.extend_from_slice(s.as_bytes());
    v
}

/// One client: commands in on one thread, frames out on another, the radio on this one.
fn session(stream: TcpStream, radio: &mut Radio) {
    let _ = stream.set_nodelay(true);
    let peer = stream.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    eprintln!("client {peer}");
    let alive = Arc::new(AtomicBool::new(true));
    let (out, out_rx) = sync_channel::<Vec<u8>>(256);
    let (cmd_tx, cmds) = sync_channel::<Result<Cmd, String>>(64);
    let writer = {
        let mut s = stream.try_clone().expect("clone stream");
        let alive = alive.clone();
        std::thread::spawn(move || {
            for buf in out_rx {
                if s.write_all(&buf).is_err() {
                    break;
                }
            }
            alive.store(false, Ordering::Relaxed);
        })
    };
    let reader = {
        let s = stream.try_clone().expect("clone stream");
        let alive = alive.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(s).lines() {
                let Ok(line) = line else { break };
                let line = line.trim();
                if !line.is_empty() && cmd_tx.send(parse_cmd(line)).is_err() {
                    break;
                }
            }
            alive.store(false, Ordering::Relaxed);
        })
    };

    let _ = out.send(text(&format!("HELLO {}", radio.describe())));
    let mut sw = Sweeper { radio, plan: None, sweep: 0, pass: 0, t0: Instant::now(), lo: None, iq: vec![0u8; IQ_BYTES] };
    while alive.load(Ordering::Relaxed) {
        let cmd = if sw.plan.is_some() {
            match cmds.try_recv() {
                Ok(c) => Some(c),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => break,
            }
        } else {
            match cmds.recv_timeout(Duration::from_millis(200)) {
                Ok(c) => Some(c),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        };
        if let Some(c) = cmd {
            if out.send(text(&sw.apply(c))).is_err() {
                break;
            }
            continue;
        }
        if let Err(e) = sw.pass(&cmds, &out, &alive) {
            let _ = out.send(text(&format!("ERR {e}")));
            sw.plan = None;
            sw.lo = None;
            sw.radio.stop_capture();
        }
    }
    sw.radio.stop_capture();
    drop(out);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = writer.join();
    let _ = reader.join();
    eprintln!("client {peer} gone");
}

fn serve(port: u16) -> Result<(), String> {
    let mut radio = Radio::open()?;
    let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("listen on {port}: {e}"))?;
    eprintln!("{} listening on {port}", radio.describe());
    for stream in listener.incoming() {
        match stream {
            Ok(s) => session(s, &mut radio),
            Err(e) => eprintln!("accept: {e}"),
        }
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let r = match args.first().map(String::as_str) {
        Some("bench") => bench(),
        Some("--version") => {
            println!("corvus-pluto {VERSION}");
            Ok(())
        }
        Some("serve") | None => serve(args.get(1).and_then(|p| p.parse().ok()).unwrap_or(PORT)),
        Some(other) => Err(format!("usage: corvus-pluto [serve [port] | bench | --version]  (got {other})")),
    };
    if let Err(e) = r {
        eprintln!("corvus-pluto: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commands_parse() {
        let Ok(Cmd::Sweep(p)) = parse_cmd("SWEEP f0=70000000 step=40000000 n=149 dwell=2 skip=3") else { panic!() };
        assert_eq!((p.f0, p.step, p.n, p.dwell, p.skip, p.bounce), (70_000_000, 40_000_000, 149, 2, 3, true));
        let Ok(Cmd::Sweep(p)) = parse_cmd("SWEEP f0=1 step=2 n=3 order=asc") else { panic!() };
        assert!(!p.bounce && p.skip == 2 && p.dwell == 1);
        assert!(matches!(parse_cmd("CONFIG rate=61440000 bw=56000000 gain=40"), Ok(Cmd::Config { rate: 61_440_000, bw: 56_000_000, gain: 40 })));
        assert!(matches!(parse_cmd("GAIN 33"), Ok(Cmd::Gain(33))));
        assert!(parse_cmd("SWEEP f0=1 n=3").is_err());
        assert!(parse_cmd("NOPE").is_err());
    }

    #[test]
    fn bounce_walks_back_and_repeats_the_turnaround() {
        let p = SweepPlan { f0: 0, step: 1, n: 4, dwell: 1, skip: 2, bounce: true };
        let seq: Vec<u32> = (0..3).flat_map(|pass| (0..4).map(move |i| p.index(pass, i))).collect();
        assert_eq!(seq, [0, 1, 2, 3, 3, 2, 1, 0, 0, 1, 2, 3]);
    }

    #[test]
    fn block_tags_match_the_firmware_layout() {
        let iq = vec![0x11u8; IQ_BYTES];
        let mut out = vec![0u8; BLOCK_BYTES];
        make_block(&iq, &Tag { step: 7, block: 1, sweep: 0x0001_0002, freq_hz: 0x4321_8765, dma_count: 0xABCD_0123 }, &mut out);
        let w: Vec<u16> = out.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(&w[..8], &[0xA55A, 0x5357, 7, 1, 2, 1, 0x8765, 0x4321]);
        assert_eq!(&w[BLOCK_WORDS - 8..], &[0x5AA5, 0, 7, 1, 0x0123, 0xABCD, 0x8765, 0x4321]);
        assert_eq!(w[8], 0x1111);
    }
}
