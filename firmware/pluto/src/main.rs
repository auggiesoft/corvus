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
//!   SWEEP f0=<Hz> step=<Hz> n=<steps> [dwell=<blocks>] [settle=<blocks>] [order=bounce|asc] [mode=fast|driver]
//!         settle: extra blocks dropped after each retune, beyond what the board needs itself (the driver
//!         path's two stale blocks). Older clients send skip=<2 + settle> instead; that still works.
//!   GAIN <dB>                                                  takes effect at once, also mid-sweep
//!   STOP                                                       stop sweeping (the connection stays)
//! Board to client, frames of [u32 type][u32 length][payload], little-endian:
//!   1  one sweep block (16384 bytes)
//!   2  a text line: "HELLO ...", "OK ...", "ERR ...", "NOTE ..."
//!   3  one sweep block with 12-bit packed samples (12296 bytes; asked for with SWEEP ... pack=12)
//! Tags: step index, block within the step, pass counter (one per pass, either direction), the LO in Hz
//! (low 32 bits), and a block clock: time since the sweep started in units of one block's duration.
//!
//! Fast mode (src/fast.rs, the default when the board allows it) drives the AD936x itself: fastlock profiles
//! switched over direct SPI, gain tables written at the band edges, mmap capture blocks queued after each
//! switch. About 6x the steps/s of the driver path above, which stays as the fallback (`mode=driver`).
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

mod fast;
mod gaintable;
mod pollers;

use fast::{Driver, Fast, Profiles};

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
/// A sweep block with its samples packed to 12 bits (`pack=12`): the 8 header words, then 4088 I, Q pairs
/// in 3 bytes each (I bits 0-7; I bits 8-11 | Q bits 0-3 << 4; Q bits 4-11), then the 8 trailer words.
const FRAME_PACKED: u32 = 3;
const PACKED_BYTES: usize = 4 * TAG_WORDS + IQ_PER_BLOCK * 3;

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

/// The AD936x's registers through the driver's debugfs file (kernel SPI, ~45 us per access): for tuning
/// past the driver's range.
struct Regs(File);

impl Regs {
    fn open(phy: &Path) -> Result<Regs, String> {
        let dev = phy.file_name().unwrap_or_default().to_string_lossy().to_string();
        let p = format!("/sys/kernel/debug/iio/{dev}/direct_reg_access");
        OpenOptions::new().read(true).write(true).open(&p).map(Regs).map_err(|e| format!("{p}: {e}"))
    }

    fn read(&self, reg: u16) -> Result<u8, String> {
        self.0.write_at(format!("0x{reg:X}").as_bytes(), 0).map_err(|e| format!("register 0x{reg:X}: {e}"))?;
        let mut b = [0u8; 16];
        let n = self.0.read_at(&mut b, 0).map_err(|e| format!("register 0x{reg:X}: {e}"))?;
        let t = std::str::from_utf8(&b[..n]).unwrap_or("").trim();
        u8::from_str_radix(t.trim_start_matches("0x"), 16).map_err(|_| format!("register 0x{reg:X}: {t:?}"))
    }

    fn write(&self, reg: u16, val: u8) -> Result<(), String> {
        self.0.write_at(format!("0x{reg:X} 0x{val:X}").as_bytes(), 0).map(|_| ()).map_err(|e| format!("register 0x{reg:X}: {e}"))
    }
}

/// The driver's RX LO range: the VCO's 6-12 GHz divided by 2..128. It advertises 46_875_001 Hz but refuses it.
const DRIVER_LO: (u64, u64) = (46_880_000, 6_000_000_000);
/// How far past it the RX synthesizer is pushed (VCO outside 6-12 GHz) when the chip locks there, checked at
/// start-up. Measured on a Pluto+: locks 45-6170 MHz; at 46 MHz the clock spurs sit where they should, at 45
/// they don't. Beyond this range a step is taken at the nearest end and shifted digitally (see `phys`).
const EXT_LO: (u64, u64) = (46_000_000, 6_150_000_000);
const RFPLL_MODULUS: f64 = 8_388_593.0;
const REG_RFPLL_DIVIDERS: u16 = 0x005;
const REG_RX_INTEGER_BYTE_0: u16 = 0x231;
const REG_RX_VCO_LOCK: u16 = 0x247;

/// The AD936x: the phy (tuning, gain, rates) and the RX capture core with its buffer.
struct Radio {
    phy: PathBuf,
    rx: PathBuf,
    rx_lo: FastAttr,
    gain: FastAttr,
    dev: Option<File>,
    rate: u32,
    gain_db: i32,
    regs: Option<Regs>,
    /// The LO range this chip tunes (DRIVER_LO, or wider where it locks).
    lo_range: (u64, u64),
    /// Set while the synthesizer runs past the driver's range: the edge the driver believes it's at.
    ext_edge: std::cell::Cell<Option<u64>>,
}

impl Radio {
    fn open() -> Result<Radio, String> {
        let phy = find_iio("ad9361-phy")?;
        let rx = find_iio("cf-ad9361-lpc")?;
        let rx_lo = FastAttr::open(phy.join("out_altvoltage0_RX_LO_frequency"))?;
        let gain = FastAttr::open(phy.join("in_voltage0_hardwaregain"))?;
        let rate = read_attr(&phy.join("in_voltage_sampling_frequency")).parse().unwrap_or(0);
        let regs = Regs::open(&phy).ok();
        let mut r = Radio { phy, rx, rx_lo, gain, dev: None, rate, gain_db: 40, regs, lo_range: DRIVER_LO, ext_edge: std::cell::Cell::new(None) };
        r.lo_range = r.probe_lo_range();
        let _ = r.tune(1_000_000_000);
        Ok(r)
    }

    /// How far past the driver's range this chip's synthesizer locks (in 10 MHz / 0.2 MHz steps from EXT_LO).
    fn probe_lo_range(&self) -> (u64, u64) {
        if self.regs.is_none() {
            return DRIVER_LO;
        }
        let mut hi = EXT_LO.1;
        while hi > DRIVER_LO.1 && self.tune_beyond(hi).is_err() {
            hi -= 10_000_000;
        }
        let mut lo = EXT_LO.0;
        while lo < DRIVER_LO.0 && self.tune_beyond(lo).is_err() {
            lo += 200_000;
        }
        (lo.min(DRIVER_LO.0), hi.max(DRIVER_LO.1))
    }

    /// The LO a step centred on `hz` is taken at: `hz` itself where the chip tunes, else the nearest end.
    fn phys(&self, hz: u64) -> u64 {
        hz.clamp(self.lo_range.0, self.lo_range.1)
    }

    fn describe(&self) -> String {
        let fw = fs::read_to_string("/opt/VERSIONS").ok().and_then(|v| v.lines().next().map(|l| l.trim().to_string())).unwrap_or_default();
        let model = fs::read_to_string("/sys/firmware/devicetree/base/model").unwrap_or_default().trim_end_matches('\0').trim().to_string();
        let fast = if fast::unavailable(&self.phy).is_none() { "fast" } else { "driver" };
        format!("corvus-pluto {VERSION} model=\"{model}\" fw=\"{fw}\" modes={fast} lo={}-{}", self.lo_range.0, self.lo_range.1)
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

    fn set_gain(&mut self, db: i32) -> Result<(), String> {
        self.gain_db = db.clamp(0, 73);
        self.gain.set(self.gain_db as u64)
    }

    /// Tune the RX LO: through the driver inside its range, past it by programming the synthesizer.
    fn tune(&self, hz: u64) -> Result<(), String> {
        if hz < DRIVER_LO.0 || hz > DRIVER_LO.1 {
            if hz < self.lo_range.0 || hz > self.lo_range.1 {
                return Err(format!("{hz} Hz is outside this chip's LO range"));
            }
            return self.tune_beyond(hz);
        }
        self.driver_tune(hz)
    }

    /// A driver retune that really happens: after tuning past the range the driver still believes it's at the
    /// edge, and it skips a retune to the frequency it believes is set.
    fn driver_tune(&self, hz: u64) -> Result<(), String> {
        if self.ext_edge.take() == Some(hz) {
            self.rx_lo.set(if hz > DRIVER_LO.0 + 1_000_000 { hz - 1_000_000 } else { hz + 1_000_000 })?;
        }
        self.rx_lo.set(hz)
    }

    /// Tune past the driver's range: the driver tunes the nearest edge (the VCO settings and divider for it),
    /// then the integer and fractional words are written for `hz` with the same divider, the VCO now outside
    /// 6-12 GHz; writing integer byte 0 starts the VCO calibration. Err if it doesn't lock within 5 ms.
    fn tune_beyond(&self, hz: u64) -> Result<(), String> {
        let regs = self.regs.as_ref().ok_or("no register access (debugfs)")?;
        let edge = hz.clamp(DRIVER_LO.0, DRIVER_LO.1);
        self.driver_tune(edge)?;
        let rd = |a: u16| regs.read(a).map(|v| v as u32);
        let div = (rd(REG_RFPLL_DIVIDERS)? & 0x0F) as i32; // RX: LO = VCO / 2^(div + 1)
        let integer = rd(REG_RX_INTEGER_BYTE_0)? | ((rd(REG_RX_INTEGER_BYTE_0 + 1)? & 7) << 8);
        let fract = rd(REG_RX_INTEGER_BYTE_0 + 2)? | (rd(REG_RX_INTEGER_BYTE_0 + 3)? << 8) | ((rd(REG_RX_INTEGER_BYTE_0 + 4)? & 0x7F) << 16);
        let mult = (1u64 << (div + 1)) as f64;
        let reference = edge as f64 * mult / (integer as f64 + fract as f64 / RFPLL_MODULUS);
        let n = hz as f64 * mult / reference;
        let (ni, nf) = (n.floor() as u32, ((n - n.floor()) * RFPLL_MODULUS).round() as u32);
        let b1 = regs.read(REG_RX_INTEGER_BYTE_0 + 1)?;
        regs.write(REG_RX_INTEGER_BYTE_0 + 4, ((nf >> 16) & 0x7F) as u8)?;
        regs.write(REG_RX_INTEGER_BYTE_0 + 3, (nf >> 8) as u8)?;
        regs.write(REG_RX_INTEGER_BYTE_0 + 2, nf as u8)?;
        regs.write(REG_RX_INTEGER_BYTE_0 + 1, (b1 & !7) | ((ni >> 8) & 7) as u8)?;
        self.ext_edge.set(Some(edge));
        regs.write(REG_RX_INTEGER_BYTE_0, ni as u8)?;
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(5) {
            if regs.read(REG_RX_VCO_LOCK)? & 2 != 0 {
                return Ok(());
            }
        }
        Err(format!("the synthesizer doesn't lock at {hz} Hz"))
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

impl Driver for Radio {
    fn phy(&self) -> &Path {
        &self.phy
    }
    fn rx(&self) -> &Path {
        &self.rx
    }
    fn tune(&self, hz: u64) -> Result<(), String> {
        Radio::tune(self, hz)
    }
    fn stop_capture(&mut self) {
        Radio::stop_capture(self)
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
    /// Trailer word 1: bit 15 set when the synthesizer didn't report lock (the RFOne firmware's meaning).
    lock: u16,
}

/// Wrap `iq` (IQ_BYTES) in the Corvus header and trailer: the words the RFOne firmware writes.
fn make_block(iq: &[u8], t: &Tag, out: &mut [u8]) {
    make_block_tags(t, out);
    out[2 * TAG_WORDS..2 * TAG_WORDS + IQ_BYTES].copy_from_slice(&iq[..IQ_BYTES]);
}

/// The header and trailer words of a block (the sample area untouched).
fn make_block_tags(t: &Tag, out: &mut [u8]) {
    let h: [u16; TAG_WORDS] = [0xA55A, 0x5357, t.step, t.block, t.sweep as u16, (t.sweep >> 16) as u16, t.freq_hz as u16, (t.freq_hz >> 16) as u16];
    let r: [u16; TAG_WORDS] = [0x5AA5, t.lock, t.step, t.block, t.dma_count as u16, (t.dma_count >> 16) as u16, t.freq_hz as u16, (t.freq_hz >> 16) as u16];
    for (i, w) in h.iter().enumerate() {
        out[2 * i..2 * i + 2].copy_from_slice(&w.to_le_bytes());
    }
    let tail = 2 * (BLOCK_WORDS - TAG_WORDS);
    for (i, w) in r.iter().enumerate() {
        out[tail + 2 * i..tail + 2 * i + 2].copy_from_slice(&w.to_le_bytes());
    }
}

/// The tags as in make_block, with the samples packed to 12 bits (FRAME_PACKED).
fn make_packed(iq: &[u8], t: &Tag, out: &mut [u8]) {
    let mut words = [0u8; BLOCK_BYTES];
    make_block_tags(t, &mut words);
    out[..2 * TAG_WORDS].copy_from_slice(&words[..2 * TAG_WORDS]);
    let tail = 2 * (BLOCK_WORDS - TAG_WORDS);
    out[PACKED_BYTES - 2 * TAG_WORDS..].copy_from_slice(&words[tail..]);
    let body = &mut out[2 * TAG_WORDS..PACKED_BYTES - 2 * TAG_WORDS];
    for (o, s) in body.as_chunks_mut::<3>().0.iter_mut().zip(iq[..IQ_BYTES].as_chunks::<4>().0) {
        let i = u16::from_le_bytes([s[0], s[1]]) & 0xFFF;
        let q = u16::from_le_bytes([s[2], s[3]]) & 0xFFF;
        *o = [i as u8, (i >> 8) as u8 | ((q as u8 & 0xF) << 4), (q >> 4) as u8];
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
    /// Extra blocks dropped after each retune (beyond the driver path's two stale ones).
    settle: u16,
    /// Odd passes walk the steps backwards (no 6 GHz -> 70 MHz wrap).
    bounce: bool,
    /// Fast mode wanted (used when the board allows it).
    fast: bool,
    /// Send blocks as FRAME_PACKED.
    pack12: bool,
}

impl SweepPlan {
    /// Step index for position `i` of pass `pass`.
    fn index(&self, pass: u32, i: u32) -> u32 {
        if self.bounce && pass % 2 == 1 { self.n - 1 - i } else { i }
    }

    fn freq(&self, step: u32) -> u64 {
        self.f0 + step as u64 * self.step
    }

    /// Blocks the driver path drops after a retune: the two stale DMA blocks, plus settle.
    fn driver_skip(&self) -> u16 {
        2 + self.settle
    }

    fn freqs(&self) -> Vec<u64> {
        (0..self.n).map(|i| self.freq(i)).collect()
    }
}

/// The LOs a plan's steps are taken at (each once): steps beyond the chip's range share its nearest end.
fn phys_freqs(p: &SweepPlan, radio: &Radio) -> Vec<u64> {
    let mut v: Vec<u64> = p.freqs().iter().map(|&f| radio.phys(f)).collect();
    v.dedup();
    v
}

/// Shift a block's spectrum by -`hz` (a tone at +`hz` moves to 0 Hz) in place: a step centred `hz` away from
/// the LO it was taken at then looks as if the LO had been at its centre. 12-bit values, clipped.
fn shift_iq(iq: &mut [u8], hz: f64, fs: f64) {
    // the LO's leakage is DC before the shift and would land off centre after it (where Corvus Sweep doesn't
    // bridge it): take the block's mean out first
    let n = (IQ_BYTES / 4) as f32;
    let (mut mi, mut mq) = (0.0f32, 0.0f32);
    for smp in iq[..IQ_BYTES].as_chunks::<4>().0 {
        mi += i16::from_le_bytes([smp[0], smp[1]]) as f32;
        mq += i16::from_le_bytes([smp[2], smp[3]]) as f32;
    }
    let (mi, mq) = (mi / n, mq / n);
    let w = -2.0 * std::f64::consts::PI * hz / fs;
    let mut ph = (1.0f32, 0.0f32);
    let (c, s) = (w.cos() as f32, w.sin() as f32);
    for (k, smp) in iq[..IQ_BYTES].as_chunks_mut::<4>().0.iter_mut().enumerate() {
        if k % 256 == 0 {
            ph = (((w * k as f64).cos()) as f32, ((w * k as f64).sin()) as f32); // no drift
        }
        let i = i16::from_le_bytes([smp[0], smp[1]]) as f32 - mi;
        let q = i16::from_le_bytes([smp[2], smp[3]]) as f32 - mq;
        let ni = (i * ph.0 - q * ph.1).round().clamp(-2048.0, 2047.0) as i16;
        let nq = (i * ph.1 + q * ph.0).round().clamp(-2048.0, 2047.0) as i16;
        smp[..2].copy_from_slice(&ni.to_le_bytes());
        smp[2..].copy_from_slice(&nq.to_le_bytes());
        ph = (ph.0 * c - ph.1 * s, ph.0 * s + ph.1 * c);
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
    /// Fast mode, while it owns the radio; its calibrated profiles (kept across plans).
    fast: Option<Fast>,
    profiles: &'a mut Profiles,
    /// Why fast mode isn't running, when it was asked for.
    fast_note: Option<String>,
    /// A command that arrived during a fast pass, to apply next.
    pending: Option<Result<Cmd, String>>,
    /// Fast mode's bookkeeping between passes (a one-step plan makes thousands of passes a second): a reused
    /// scratch block, and when the bus and the profiles' age were last checked.
    scratch: Vec<u8>,
    checked: Instant,
    /// Other programs that poll the radio, paused while a plan runs (src/pollers.rs).
    paused: Option<pollers::Paused>,
}

impl Sweeper<'_> {
    /// Hand the radio back from fast mode to the driver (no-op otherwise).
    fn release(&mut self) {
        if let Some(f) = self.fast.take() {
            let resume = self.lo.or(self.plan.map(|p| p.f0)).unwrap_or(1_000_000_000);
            if let Err(e) = f.release(self.radio, resume) {
                eprintln!("fast mode release: {e}");
            }
            let _ = self.radio.set_gain(self.radio.gain_db);
            self.lo = Some(resume);
        }
    }

    /// Start a plan: fast mode if wanted and possible (calibrating its profiles first), else the driver path.
    fn start(&mut self, p: SweepPlan) -> Result<&'static str, String> {
        self.release();
        self.fast_note = None;
        if self.paused.is_none() {
            let paused = pollers::pause();
            if paused.count() > 0 {
                eprintln!("paused {} programs that poll the radio", paused.count());
            }
            self.paused = Some(paused);
        }
        if p.fast {
            let why = fast::unavailable(&self.radio.phy);
            let r = match why {
                Some(w) => Err(w),
                None => self.take_fast(&p),
            };
            match r {
                Ok(()) => return Ok("fast"),
                Err(e) => {
                    eprintln!("fast mode off: {e}");
                    self.fast_note = Some(e);
                }
            }
        }
        self.radio.start_capture()?;
        Ok("driver")
    }

    fn take_fast(&mut self, p: &SweepPlan) -> Result<(), String> {
        let freqs = phys_freqs(p, self.radio);
        let t = Instant::now();
        let n = self.profiles.calibrate(self.radio, &freqs)?;
        if n > 0 {
            eprintln!("calibrated {n} fastlock profiles in {:.2} s", t.elapsed().as_secs_f64());
        }
        let f = Fast::take(self.radio, freqs[0], self.radio.gain_db, IQ_PER_BLOCK + fast::DISCARD)?;
        self.fast = Some(f);
        self.lo = None;
        Ok(())
    }

    /// Apply a command and return its reply line.
    fn apply(&mut self, cmd: Result<Cmd, String>) -> String {
        let r = match cmd {
            Err(e) => Err(e),
            Ok(Cmd::Config { rate, bw, gain }) => {
                self.release();
                self.plan = None;
                self.paused = None;
                self.lo = None;
                self.radio.configure(rate, bw, gain).map(|_| format!("OK CONFIG rate={} bw={bw} gain={gain}", self.radio.rate))
            }
            Ok(Cmd::Gain(db)) => match self.fast.as_mut() {
                Some(f) => {
                    self.radio.gain_db = db.clamp(0, 73);
                    f.set_gain(self.radio.gain_db).map(|_| format!("OK GAIN {db}"))
                }
                None => self.radio.set_gain(db).map(|_| format!("OK GAIN {db}")),
            },
            Ok(Cmd::Stop) => {
                self.release();
                self.plan = None;
                self.paused = None;
                self.radio.stop_capture();
                Ok("OK STOP".into())
            }
            Ok(Cmd::Sweep(p)) => self.start(p).map(|mode| {
                self.plan = Some(p);
                self.sweep = self.sweep.wrapping_add(1);
                self.pass = 0;
                self.t0 = Instant::now();
                let note = self.fast_note.as_ref().map(|n| format!(" note=\"{n}\"")).unwrap_or_default();
                format!(
                    "OK SWEEP f0={} step={} n={} dwell={} settle={} order={} mode={mode} pack={}{note}",
                    p.f0,
                    p.step,
                    p.n,
                    p.dwell,
                    p.settle,
                    if p.bounce { "bounce" } else { "asc" },
                    if p.pack12 { 12 } else { 16 }
                )
            }),
        };
        r.unwrap_or_else(|e| format!("ERR {e}"))
    }

    /// A frame for one block. `scratch` (IQ_BYTES) is where samples from the uncached capture buffer are
    /// copied in bulk before packing; None when `iq` is ordinary memory already.
    fn frame(&self, iq: &[u8], step: u32, b: u16, f: u64, locked: bool, scratch: Option<&mut [u8]>) -> Vec<u8> {
        let block_s = IQ_PER_BLOCK as f64 / self.radio.rate.max(1) as f64;
        let dma = (self.t0.elapsed().as_secs_f64() / block_s) as u32;
        let tag = Tag { step: step as u16, block: b, sweep: self.sweep, freq_hz: f as u32, dma_count: dma, lock: if locked { 0 } else { 0x8000 } };
        if self.plan.is_some_and(|p| p.pack12) {
            let src = match scratch {
                Some(sc) => {
                    sc[..IQ_BYTES].copy_from_slice(&iq[..IQ_BYTES]);
                    &sc[..IQ_BYTES]
                }
                None => &iq[..IQ_BYTES],
            };
            let mut buf = vec![0u8; 8 + PACKED_BYTES];
            buf[..4].copy_from_slice(&FRAME_PACKED.to_le_bytes());
            buf[4..8].copy_from_slice(&(PACKED_BYTES as u32).to_le_bytes());
            make_packed(src, &tag, &mut buf[8..]);
            buf
        } else {
            let mut buf = vec![0u8; 8 + BLOCK_BYTES];
            buf[..4].copy_from_slice(&FRAME_BLOCK.to_le_bytes());
            buf[4..8].copy_from_slice(&(BLOCK_BYTES as u32).to_le_bytes());
            make_block(iq, &tag, &mut buf[8..]);
            buf
        }
    }

    /// One pass of the current plan. Commands are checked every few steps: a gain change applies in
    /// place, anything else ends the pass.
    fn pass(&mut self, cmds: &Receiver<Result<Cmd, String>>, out: &SyncSender<Vec<u8>>, alive: &AtomicBool) -> Result<(), String> {
        let Some(p) = self.plan else { return Ok(()) };
        if let Some(c) = self.pending.take() {
            let gain = matches!(c, Ok(Cmd::Gain(_)));
            let _ = out.send(text(&self.apply(c)));
            if !gain {
                return Ok(());
            }
        }
        if self.fast.is_some() {
            return self.fast_pass(p, cmds, out, alive);
        }
        for i in 0..p.n {
            if !alive.load(Ordering::Relaxed) {
                return Ok(());
            }
            let step = p.index(self.pass, i);
            let f = p.freq(step);
            let lo = self.radio.phys(f);
            if self.lo != Some(lo) {
                self.lo = None;
                self.radio.tune(lo)?;
                self.lo = Some(lo);
            }
            for _ in 0..p.driver_skip() {
                self.radio.read_iq(&mut self.iq)?;
            }
            for b in 0..p.dwell {
                self.radio.read_iq(&mut self.iq)?;
                if f != lo {
                    shift_iq(&mut self.iq, f as f64 - lo as f64, self.radio.rate as f64);
                }
                let buf = self.frame(&self.iq, step, b, f, true, None);
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

    fn fast_pass(&mut self, p: SweepPlan, cmds: &Receiver<Result<Cmd, String>>, out: &SyncSender<Vec<u8>>, alive: &AtomicBool) -> Result<(), String> {
        let steps: Vec<u32> = (0..p.n).map(|i| p.index(self.pass, i)).collect();
        let freqs: Vec<u64> = steps.iter().map(|&s| p.freq(s)).collect();
        let los: Vec<u64> = freqs.iter().map(|&f| self.radio.phys(f)).collect();
        let fs = self.radio.rate as f64;
        let mut unlocked = Vec::new();
        let mut pending = None;
        let mut fast = self.fast.take().unwrap();
        let mut scratch = std::mem::take(&mut self.scratch);
        let r = fast.pass(&los, self.profiles, p.settle as usize, p.dwell as usize, |i, b, iq, locked| {
            if !alive.load(Ordering::Relaxed) {
                return false;
            }
            if !locked && b == 0 {
                unlocked.push(los[i]);
            }
            let frame = if freqs[i] != los[i] {
                // a step past the chip's range: taken at its end, shifted to the step's centre
                scratch[..IQ_BYTES].copy_from_slice(&iq[..IQ_BYTES]);
                shift_iq(&mut scratch, freqs[i] as f64 - los[i] as f64, fs);
                self.frame(&scratch, steps[i], b, freqs[i], locked, None)
            } else {
                self.frame(iq, steps[i], b, freqs[i], locked, Some(&mut scratch))
            };
            if out.send(frame).is_err() {
                return false;
            }
            if b == 0 && i % 16 == 15 {
                match cmds.try_recv() {
                    Ok(c) => {
                        pending = Some(c);
                        return false;
                    }
                    Err(TryRecvError::Empty) => {}
                    Err(TryRecvError::Disconnected) => return false,
                }
            }
            true
        });
        // the bus and the profiles' age: checked every 100 ms, not every pass
        let check = self.checked.elapsed() > Duration::from_millis(100);
        if check {
            self.checked = Instant::now();
        }
        let bus_ok = !check || fast.bus_ok();
        self.fast = Some(fast);
        self.scratch = scratch;
        self.lo = los.last().copied();
        let complete = match r {
            Ok(c) => c,
            Err(e) => {
                self.release();
                self.radio.start_capture()?;
                let _ = out.send(text(&format!("NOTE fast mode off: {e}")));
                return Ok(());
            }
        };
        if !bus_ok {
            // something else drove the SPI bus during the pass: give the radio back, carry on via the driver
            self.release();
            self.radio.start_capture()?;
            let _ = out.send(text("NOTE fast mode off: another program used the radio"));
            return Ok(());
        }
        if let Some(c) = pending {
            self.pending = Some(c);
        }
        if complete {
            self.pass += 1;
            self.sweep = self.sweep.wrapping_add(1);
        }
        if !unlocked.is_empty() || (check && self.profiles.stale(&phys_freqs(&p, self.radio))) {
            // recalibrate unlocked or old profiles through the driver, then take the radio again
            for f in &unlocked {
                self.profiles.forget(*f);
            }
            self.release();
            if let Err(e) = self.take_fast(&p) {
                self.radio.start_capture()?;
                let _ = out.send(text(&format!("NOTE fast mode off: {e}")));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------------------
// Bench

fn bench() -> Result<(), String> {
    let mut r = Radio::open()?;
    let _paused = pollers::pause();
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

    let plan = SweepPlan { f0: 70_000_000, step: 40_000_000, n: 149, dwell: 1, settle: 0, bounce: false, fast: false, pack12: false };
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
                for _ in 0..=p.driver_skip() {
                    r.read_iq(&mut iq)?;
                }
            }
        }
        let steps = 4.0 * p.n as f64 / t.elapsed().as_secs_f64();
        println!("70-6000 MHz, 40 MHz steps, {}: {steps:.0} steps/s, {:.1} GHz/s", if bounce { "back and forth" } else { "ascending     " }, steps * 0.04);
    }
    r.stop_capture();

    // fast mode: the same plan, back and forth, with the per-block work it does when serving
    match fast::unavailable(&r.phy) {
        Some(why) => println!("fast mode unavailable: {why}"),
        None => {
            let mut profiles = Profiles::default();
            let freqs = plan.freqs();
            let t = Instant::now();
            profiles.calibrate(&r, &freqs)?;
            println!("fast mode: calibrated {} profiles in {:.2} s", freqs.len(), t.elapsed().as_secs_f64());
            let gain = r.gain_db;
            let mut f = Fast::take(&mut r, freqs[0], gain, IQ_PER_BLOCK + fast::DISCARD)?;
            let mut frame = vec![0u8; 8 + BLOCK_BYTES];
            for (what, work) in [("no work per block", 0), ("+ copy out of the DMA block", 1), ("+ build the frame", 2)] {
                let t = Instant::now();
                let mut steps = 0u64;
                for pass in 0..10u32 {
                    let order: Vec<u64> = (0..plan.n).map(|i| plan.freq(SweepPlan { bounce: true, ..plan }.index(pass, i))).collect();
                    f.pass(&order, &profiles, 0, 1, |i, _, iq, _| {
                        match work {
                            1 => frame[8..8 + IQ_BYTES].copy_from_slice(iq),
                            2 => make_block(iq, &Tag { step: i as u16, block: 0, sweep: pass, freq_hz: 0, dma_count: 0, lock: 0 }, &mut frame[8..]),
                            _ => {}
                        }
                        steps += 1;
                        true
                    })?;
                }
                let sps = steps as f64 / t.elapsed().as_secs_f64();
                println!("fast mode, {what:28}: {sps:.0} steps/s, {:.1} GHz/s, {:.0} us per step", sps * 0.04, 1e6 / sps);
            }
            println!("  synthesizer unlocked after a switch: {} times; bus stayed ours: {}", f.unlocked, f.bus_ok());
            f.release(&mut r, lo0.parse().unwrap_or(1_000_000_000))?;
        }
    }
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
            // settle=, or the older skip= (which counted the driver path's two stale blocks)
            settle: num("settle").or(num("skip").map(|k| k.saturating_sub(2))).unwrap_or(0).min(62) as u16,
            bounce: kv(&w, "order") != Some("asc"),
            fast: kv(&w, "mode") != Some("driver"),
            pack12: kv(&w, "pack") == Some("12"),
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
fn session(stream: TcpStream, radio: &mut Radio, profiles: &mut Profiles) {
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
            // next to the network interrupt (CPU0); several frames per write when they queue up
            pin_to_cpu(0);
            let mut batch: Vec<Vec<u8>> = Vec::with_capacity(16);
            while let Ok(first) = out_rx.recv() {
                batch.push(first);
                while batch.len() < 16 {
                    match out_rx.try_recv() {
                        Ok(b) => batch.push(b),
                        Err(_) => break,
                    }
                }
                if write_all_vectored(&mut s, &batch).is_err() {
                    break;
                }
                batch.clear();
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

    // the sweep loop spins while capture blocks fill: keep it off the core that takes the interrupts
    pin_to_cpu(1);
    let _ = out.send(text(&format!("HELLO {}", radio.describe())));
    let mut sw = Sweeper { radio, plan: None, sweep: 0, pass: 0, t0: Instant::now(), lo: None, iq: vec![0u8; IQ_BYTES], fast: None, profiles, fast_note: None, pending: None, scratch: vec![0u8; IQ_BYTES], checked: Instant::now(), paused: None };
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
            sw.release();
            sw.plan = None;
            sw.paused = None;
            sw.lo = None;
            sw.radio.stop_capture();
        }
    }
    sw.release();
    sw.radio.stop_capture();
    sw.paused = None;
    drop(out);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = writer.join();
    let _ = reader.join();
    eprintln!("client {peer} gone");
}

/// CPUs online, from sysfs ("0-1"). Not available_parallelism() or musl's sysconf: both follow the calling
/// thread's own affinity, which is one CPU once the sweep loop is pinned.
fn cpus() -> usize {
    let s = std::fs::read_to_string("/sys/devices/system/cpu/online").unwrap_or_default();
    s.trim()
        .split(',')
        .map(|r| match r.split_once('-') {
            Some((a, b)) => b.parse::<usize>().unwrap_or(0).saturating_sub(a.parse().unwrap_or(0)) + 1,
            None => usize::from(!r.is_empty()),
        })
        .sum::<usize>()
        .max(1)
}

/// Pin the calling thread to one CPU (no-op on a single-core board).
fn pin_to_cpu(cpu: usize) {
    if cpu < cpus() {
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(cpu, &mut set);
            libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
        }
    }
}

/// Write every buffer, gathering them into as few syscalls as the socket takes.
fn write_all_vectored(s: &mut TcpStream, bufs: &[Vec<u8>]) -> std::io::Result<()> {
    let (mut i, mut off) = (0, 0);
    while i < bufs.len() {
        let mut slices: Vec<std::io::IoSlice> = Vec::with_capacity(bufs.len() - i);
        slices.push(std::io::IoSlice::new(&bufs[i][off..]));
        slices.extend(bufs[i + 1..].iter().map(|b| std::io::IoSlice::new(b)));
        let mut n = s.write_vectored(&slices)?;
        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }
        while i < bufs.len() && n >= bufs[i].len() - off {
            n -= bufs[i].len() - off;
            i += 1;
            off = 0;
        }
        off += n;
    }
    Ok(())
}

fn serve(port: u16) -> Result<(), String> {
    let mut radio = Radio::open()?;
    let mut profiles = Profiles::default();
    if let Some(why) = fast::unavailable(&radio.phy) {
        eprintln!("fast mode unavailable: {why}");
    }
    let n = pollers::resume_stopped();
    if n > 0 {
        eprintln!("resumed {n} radio pollers an earlier run left paused");
    }
    let listener = TcpListener::bind(("0.0.0.0", port)).map_err(|e| format!("listen on {port}: {e}"))?;
    eprintln!("{} listening on {port}", radio.describe());
    for stream in listener.incoming() {
        match stream {
            Ok(s) => session(s, &mut radio, &mut profiles),
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
        let Ok(Cmd::Sweep(p)) = parse_cmd("SWEEP f0=70000000 step=40000000 n=149 dwell=2 settle=1") else { panic!() };
        assert_eq!((p.f0, p.step, p.n, p.dwell, p.settle, p.bounce, p.fast), (70_000_000, 40_000_000, 149, 2, 1, true, true));
        assert_eq!(p.driver_skip(), 3);
        // older clients: skip= counted the two stale blocks
        let Ok(Cmd::Sweep(p)) = parse_cmd("SWEEP f0=1 step=2 n=3 skip=3 order=asc mode=driver") else { panic!() };
        assert!(!p.bounce && p.settle == 1 && p.dwell == 1 && !p.fast);
        let Ok(Cmd::Sweep(p)) = parse_cmd("SWEEP f0=1 step=2 n=3") else { panic!() };
        assert_eq!(p.settle, 0);
        assert!(matches!(parse_cmd("CONFIG rate=61440000 bw=56000000 gain=40"), Ok(Cmd::Config { rate: 61_440_000, bw: 56_000_000, gain: 40 })));
        assert!(matches!(parse_cmd("GAIN 33"), Ok(Cmd::Gain(33))));
        assert!(parse_cmd("SWEEP f0=1 n=3").is_err());
        assert!(parse_cmd("NOPE").is_err());
    }

    #[test]
    fn packed_blocks_unpack_to_the_same_samples() {
        let mut iq = vec![0u8; IQ_BYTES];
        let vals: Vec<i16> = (0..2 * IQ_PER_BLOCK).map(|k| ((k as i32 * 37 % 4096) - 2048) as i16).collect();
        for (k, v) in vals.iter().enumerate() {
            iq[2 * k..2 * k + 2].copy_from_slice(&v.to_le_bytes());
        }
        let t = Tag { step: 3, block: 0, sweep: 9, freq_hz: 0x1234_5678, dma_count: 77, lock: 0 };
        let mut packed = vec![0u8; PACKED_BYTES];
        make_packed(&iq, &t, &mut packed);
        let mut full = vec![0u8; BLOCK_BYTES];
        make_block(&iq, &t, &mut full);
        assert_eq!(&packed[..16], &full[..16]);
        assert_eq!(&packed[PACKED_BYTES - 16..], &full[BLOCK_BYTES - 16..]);
        let body = &packed[16..PACKED_BYTES - 16];
        for (k, c) in body.chunks_exact(3).enumerate() {
            let i = (c[0] as u16 | ((c[1] as u16 & 0xF) << 8)) as i16;
            let q = ((c[1] as u16 >> 4) | ((c[2] as u16) << 4)) as i16;
            let sx = |v: i16| (v << 4) >> 4;
            assert_eq!((sx(i), sx(q)), (vals[2 * k], vals[2 * k + 1]), "pair {k}");
        }
    }

    #[test]
    fn shift_moves_a_tone_to_dc() {
        // a tone at +5 MHz (61.44 MSPS), shifted by 5 MHz, becomes DC: constant I and Q
        let fs = 61.44e6;
        let mut iq = vec![0u8; IQ_BYTES];
        for (k, smp) in iq.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let a = 2.0 * std::f64::consts::PI * 5e6 * k as f64 / fs;
            smp[..2].copy_from_slice(&((1000.0 * a.cos()).round() as i16).to_le_bytes());
            smp[2..].copy_from_slice(&((1000.0 * a.sin()).round() as i16).to_le_bytes());
        }
        shift_iq(&mut iq, 5e6, fs);
        for smp in iq.as_chunks::<4>().0.iter().step_by(97) {
            let (i, q) = (i16::from_le_bytes([smp[0], smp[1]]), i16::from_le_bytes([smp[2], smp[3]]));
            assert!((i - 1000).abs() <= 3 && q.abs() <= 3, "{i} {q}");
        }
    }

    #[test]
    fn bounce_walks_back_and_repeats_the_turnaround() {
        let p = SweepPlan { f0: 0, step: 1, n: 4, dwell: 1, settle: 0, bounce: true, fast: false, pack12: false };
        let seq: Vec<u32> = (0..3).flat_map(|pass| (0..4).map(move |i| p.index(pass, i))).collect();
        assert_eq!(seq, [0, 1, 2, 3, 3, 2, 1, 0, 0, 1, 2, 3]);
    }

    #[test]
    fn block_tags_match_the_firmware_layout() {
        let iq = vec![0x11u8; IQ_BYTES];
        let mut out = vec![0u8; BLOCK_BYTES];
        make_block(&iq, &Tag { step: 7, block: 1, sweep: 0x0001_0002, freq_hz: 0x4321_8765, dma_count: 0xABCD_0123, lock: 0x8000 }, &mut out);
        let w: Vec<u16> = out.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
        assert_eq!(&w[..8], &[0xA55A, 0x5357, 7, 1, 2, 1, 0x8765, 0x4321]);
        assert_eq!(&w[BLOCK_WORDS - 8..], &[0x5AA5, 0x8000, 7, 1, 0x0123, 0xABCD, 0x8765, 0x4321]);
        assert_eq!(w[8], 0x1111);
    }
}
