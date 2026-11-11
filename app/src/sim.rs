//! A stand-in for the RFOne: produces tagged sweep blocks in the firmware's visiting order and at
//! its measured hop timing (docs/measurements.md, "Visiting order"), over a made-up band plan. Lets the
//! UI run without the radio (`--sim`), and makes the ~19 % VCO-order gain visible there too.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rustfft::FftPlanner;
use rustfft::num_complex::Complex32;

use crate::hydra::{BLOCK_WORDS, Gain, RxSettings, Sink, Tag, tag_block};
use crate::plan::{Order, Plan, Rate, vco_divider};

struct Ctl {
    plan: Plan,
    rx: RxSettings,
    running: bool,
    restart: bool,
}

pub struct Sim {
    ctl: Arc<Mutex<Ctl>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Sim {
    pub fn start(sink: Sink, plan: Plan, rx: RxSettings) -> Sim {
        let ctl = Arc::new(Mutex::new(Ctl { plan, rx, running: true, restart: true }));
        let stop = Arc::new(AtomicBool::new(false));
        let (c, s) = (ctl.clone(), stop.clone());
        let thread = std::thread::Builder::new().name("corvus-sim".into()).spawn(move || run(c, s, sink)).expect("spawn sim");
        Sim { ctl, stop, thread: Some(thread) }
    }

    pub fn sweep(&self, plan: &Plan) {
        let mut c = self.ctl.lock();
        c.plan = plan.clone();
        c.restart = true;
        c.running = true;
    }

    pub fn set_rx(&self, rx: RxSettings) {
        let mut c = self.ctl.lock();
        c.rx = rx;
        c.restart = true;
    }

    pub fn pause(&self) {
        self.ctl.lock().running = false;
    }

    pub fn resume(&self) {
        let mut c = self.ctl.lock();
        c.running = true;
        c.restart = true;
    }
}

impl Drop for Sim {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f32 {
        (self.next() >> 40) as f32 / (1u64 << 24) as f32
    }
    /// Approximately normal (Irwin–Hall, 4 terms).
    fn gauss(&mut self) -> f32 {
        (self.unit() + self.unit() + self.unit() + self.unit() - 2.0) * 1.732
    }
    fn range(&mut self, lo: u32, hi: u32) -> u32 {
        lo + (self.next() % (hi - lo + 1) as u64) as u32
    }
}

fn run(ctl: Arc<Mutex<Ctl>>, stop: Arc<AtomicBool>, sink: Sink) {
    let bands = band_plan();
    let ifft = FftPlanner::<f32>::new().plan_fft_inverse(BLOCK_WORDS);
    let mut spec = vec![Complex32::default(); BLOCK_WORDS];
    let mut scratch = vec![Complex32::default(); ifft.get_inplace_scratch_len()];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let mut base = Instant::now();
    let mut dma: u32 = 0;
    let mut sweep: u32 = 0;
    let (mut plan, mut rx) = {
        let c = ctl.lock();
        (c.plan.clone(), c.rx)
    };
    let mut seq: Vec<u16> = Vec::new();
    let mut pos = 0usize;
    let mut pass = 0u32;
    let mut prev_rf: Option<f64> = None;
    let mut tune_dma: u32 = 0;
    let mut batch: Vec<u16> = Vec::with_capacity(BLOCK_WORDS * 16);
    while !stop.load(Ordering::Relaxed) {
        {
            let mut c = ctl.lock();
            if !c.running {
                drop(c);
                if !batch.is_empty() {
                    sink.push(std::mem::take(&mut batch));
                }
                std::thread::sleep(Duration::from_millis(30));
                base = Instant::now() - Duration::from_secs_f64(dma as f64 * plan.rate.block_s());
                continue;
            }
            if c.restart {
                c.restart = false;
                plan = c.plan.clone();
                rx = c.rx;
                seq = plan.sequence();
                pos = 0;
                pass = 0;
                sweep += 1;
                dma += 6; // the stop/start round trip
                // the block length may have changed with the rate
                base = Instant::now() - Duration::from_secs_f64(dma as f64 * plan.rate.block_s());
            }
        }
        // next step in the firmware's order
        let vco = plan.order == Order::VcoContinuous;
        if pos >= seq.len() {
            pos = 0;
            pass += 1;
            sweep += 1;
        }
        let idx = if vco && pass % 2 == 1 { seq[seq.len() - 1 - pos] } else { seq[pos] };
        pos += 1;
        let rf = plan.centre(idx as usize);
        // HackRF: steps 4g, 4g+1 are slices below a tune's LO, 4g+2, 4g+3 the ones above; the two slices of
        // a tune share its block (sides 0 and 1), and each tune costs 2 discarded blocks.
        let complex = plan.rate.sliced();
        let side: u16 = if complex && idx % 4 >= 2 { 1 } else { 0 };
        // hop cost: 2 blocks of retune + lock at either rate (measured), more in the low band, ~10 ms
        // across a divider switch between neighbours, ~0.5 ms for a VCO-continuous join, ~8 ms for
        // the wrap. The fixed costs are in 409.6 µs blocks, converted to this rate's block.
        let blk = |n: u32| (n as f64 * 409.6e-6 / plan.rate.block_s()).round() as u32;
        let fi = plan.rate.if_hz();
        let mut over = 2 + plan.skip as u32; // retune, the block in flight, then settle blocks
        let mut polls = rng.range(2, 8) as u16;
        if plan.rate.iq16() {
            // corvus-pluto's fast mode (measured on a Pluto+ over Ethernet): a fastlock switch settles in ~1 us,
            // and each step costs ~0.23 ms beyond its block, mostly the board's network sending; ~0.5 ms more
            // where it writes a gain table (1.3, 4 GHz). Settle blocks are dropped as asked; no stale ones.
            let edge = prev_rf.is_some_and(|p| (p <= 1.3e9) != (rf <= 1.3e9) || (p <= 4e9) != (rf <= 4e9));
            let ms = 0.23 + if edge { 0.5 } else { 0.0 };
            over = (ms * 1e-3 / plan.rate.block_s()).round() as u32 + plan.skip as u32;
            polls = 0;
        } else if complex {
            over = if side == 0 { 2 } else { 0 };
            polls = side;
        } else if let Some(p) = prev_rf {
            if rf < 60e6 {
                over += blk(rng.range(3, 6));
                polls += 15;
            }
            if vco_divider(p, fi) != vco_divider(rf, fi) {
                if vco {
                    over += 1;
                    polls += rng.range(5, 25) as u16;
                } else if rf < p - 50e6 {
                    over += blk(18);
                    polls += 70;
                } else {
                    over += blk(21);
                    polls += 90;
                }
            } else if rf < p - 50e6 {
                over += blk(18);
                polls += 70;
            }
        }
        prev_rf = Some(rf);
        dma = dma.wrapping_add(over);
        if side == 0 {
            tune_dma = dma;
        }
        let t = tune_dma as f64 * plan.rate.block_s();
        for b in 0..plan.dwell {
            let start = batch.len();
            batch.resize(start + BLOCK_WORDS, 0);
            if plan.rate.iq16() {
                synth_iq(&mut batch[start..start + BLOCK_WORDS], rf, plan.rate, t, &rx, &bands, &mut spec, &mut scratch, &*ifft, &mut rng);
            } else if complex {
                let lo = if side == 0 { rf + crate::dsp::HACKRF_SLICE_HZ } else { rf - crate::dsp::HACKRF_SLICE_HZ };
                synth_iq(&mut batch[start..], lo, plan.rate, t, &rx, &bands, &mut spec, &mut scratch, &*ifft, &mut rng);
            } else {
                synth(&mut batch[start..], rf, plan.rate, t, &rx, &bands, &mut spec, &mut scratch, &*ifft, &mut rng);
            }
            // u64 then u32: the tags wrap above 4.29 GHz like the engine expects (an f64 cast would saturate)
            let tag = Tag { step: idx, block: b as u16, sweep, freq_hz: rf as u64 as u32, dma_count: tune_dma.wrapping_add(b as u32), lock_polls: polls, lock_timed_out: false };
            tag_block(&mut batch[start..], &tag);
            if side == 0 {
                dma = dma.wrapping_add(1);
            }
        }
        let due = Duration::from_secs_f64(dma as f64 * plan.rate.block_s());
        let el = base.elapsed();
        if batch.len() >= BLOCK_WORDS * 16 || due > el {
            sink.push(std::mem::take(&mut batch));
            batch.reserve(BLOCK_WORDS * 16);
        }
        if due > el + Duration::from_millis(3) {
            std::thread::sleep(due - el);
        } else if el > due + Duration::from_millis(250) {
            // fell behind (slow machine or debugger): don't try to catch up in a burst
            base = Instant::now() - due;
        }
    }
}

#[derive(Clone, Copy)]
enum Shape {
    /// Gaussian-ish carrier of this occupied width.
    Bump,
    /// Flat block (TV, LTE) with an optional CW pilot this far above its lower edge.
    Flat(Option<f64>),
}

struct Sig {
    f: f64,
    bw: f64,
    db: f32,
    shape: Shape,
    /// (period s, duty, phase)
    burst: Option<(f64, f64, f64)>,
}

impl Sig {
    fn reach(&self) -> f64 {
        self.bw * 0.75 + 5e3
    }
}

fn band_plan() -> Vec<Sig> {
    let mut v = Vec::new();
    let mut add = |f_mhz: f64, bw_khz: f64, db: f32, shape: Shape, burst: Option<(f64, f64, f64)>| {
        v.push(Sig { f: f_mhz * 1e6, bw: bw_khz * 1e3, db, shape, burst });
    };
    // FM broadcast
    for (f, db) in [
        (88.1, -58.0), (88.9, -66.0), (89.7, -49.0), (90.9, -71.0), (91.5, -55.0), (92.3, -62.0), (93.3, -47.0),
        (94.1, -69.0), (95.5, -52.0), (96.5, -60.0), (97.3, -45.0), (98.1, -64.0), (99.5, -57.0), (100.3, -50.0),
        (101.1, -67.0), (102.5, -53.0), (103.7, -61.0), (104.5, -48.0), (105.3, -70.0), (106.1, -56.0), (107.5, -63.0),
    ]
    {
        add(f, 190.0, db, Shape::Bump, None);
    }
    // CB, birdies, low VHF
    add(27.185, 8.0, -78.0, Shape::Bump, Some((5.0, 0.3, 0.1)));
    for f in [48.0, 96.0, 144.0, 192.0, 240.0] {
        add(f, 1.0, -88.0, Shape::Bump, None);
    }
    add(40.68, 20.0, -84.0, Shape::Bump, None);
    // airband
    for (k, f) in [118.3, 119.1, 120.9, 121.5, 124.35, 125.8, 127.25, 128.55, 132.0, 134.1, 135.85].iter().enumerate() {
        add(*f, 6.0, -66.0 - (k % 4) as f32 * 4.0, Shape::Bump, Some((2.5 + k as f64 * 0.7, 0.35, k as f64 * 0.37)));
    }
    // VHF high: APRS, marine, pagers, NOAA
    add(144.39, 12.0, -62.0, Shape::Bump, Some((8.0, 0.12, 0.2)));
    add(146.52, 12.0, -70.0, Shape::Bump, Some((11.0, 0.25, 0.6)));
    add(152.24, 18.0, -60.0, Shape::Bump, Some((3.0, 0.5, 0.3)));
    add(156.8, 12.0, -68.0, Shape::Bump, Some((9.0, 0.3, 0.8)));
    add(157.45, 18.0, -64.0, Shape::Bump, Some((4.0, 0.4, 0.5)));
    add(162.4, 12.0, -76.0, Shape::Bump, None);
    add(162.55, 12.0, -72.0, Shape::Bump, None);
    // TV VHF / UHF (ATSC: flat 5.38 MHz with a pilot 0.31 MHz above the lower channel edge)
    for (lo, db) in [(176.0, -82.0), (194.0, -86.0), (470.0, -80.0), (506.0, -74.0), (518.0, -84.0), (554.0, -79.0), (566.0, -88.0), (590.0, -77.0), (602.0, -85.0), (632.0, -83.0)] {
        add(lo + 3.0, 5380.0, db, Shape::Flat(Some(0.0)), None);
    }
    // radiosonde, 433 ISM, land mobile
    add(403.5, 8.0, -78.0, Shape::Bump, Some((1.0, 0.6, 0.0)));
    add(433.92, 40.0, -54.0, Shape::Bump, Some((4.0, 0.1, 0.4)));
    add(434.2, 40.0, -66.0, Shape::Bump, Some((7.0, 0.08, 0.1)));
    for (k, f) in [451.3, 453.65, 460.1, 462.5625, 462.7125, 467.3].iter().enumerate() {
        add(*f, 12.0, -70.0 - k as f32 * 2.0, Shape::Bump, Some((3.0 + k as f64, 0.3, k as f64 * 0.21)));
    }
    // LTE and public safety
    for (c, bw, db) in [(627.0, 10e3, -74.0), (739.0, 5e3, -73.0), (751.0, 10e3, -66.0), (731.5, 10e3, -71.0), (881.5, 10e3, -63.0), (874.0, 5e3, -70.0), (889.0, 5e3, -76.0)] {
        add(c, bw, db, Shape::Flat(None), None);
    }
    for f in [770.10625, 771.38125, 772.68125, 851.0125, 852.5, 853.7125, 855.79, 857.2375, 859.4625, 866.5] {
        add(f, 12.5, -66.0, Shape::Bump, None);
    }
    // 900 ISM hoppers, pagers, GSM-ish carriers
    for k in 0..12 {
        add(903.0 + k as f64 * 2.1, 125.0, -70.0, Shape::Bump, Some((1.7 + k as f64 * 0.13, 0.08, k as f64 * 0.29)));
    }
    add(929.6125, 25.0, -58.0, Shape::Bump, Some((6.0, 0.4, 0.0)));
    for k in 0..10 {
        add(935.2 + k as f64 * 2.4, 200.0, -72.0 - (k % 3) as f32 * 5.0, Shape::Bump, None);
    }
    // ADS-B / Mode S, DME, L-band
    add(1090.0, 2000.0, -64.0, Shape::Bump, Some((0.4, 0.55, 0.0)));
    add(1030.0, 2000.0, -80.0, Shape::Bump, Some((0.9, 0.3, 0.2)));
    for k in 0..6 {
        add(1537.5 + k as f64 * 1.3, 20.0, -84.0, Shape::Bump, None);
    }
    for k in 0..5 {
        add(1617.0 + k as f64 * 1.9, 40.0, -78.0, Shape::Bump, Some((0.09 * (k + 3) as f64, 0.2, k as f64 * 0.1)));
    }
    add(1296.2, 8.0, -86.0, Shape::Bump, None);
    // above the R82x boards' range, for the HackRF: PCS, 2.4 GHz WiFi bursts, 5.8 GHz
    add(1960.0, 5000.0, -70.0, Shape::Flat(None), None);
    for (k, f) in [2412.0, 2437.0, 2462.0].iter().enumerate() {
        add(*f, 18000.0, -62.0 - 4.0 * k as f32, Shape::Flat(None), Some((0.11 + 0.05 * k as f64, 0.35, 0.3 * k as f64)));
    }
    add(5785.0, 18000.0, -74.0, Shape::Flat(None), Some((0.2, 0.5, 0.1)));
    v
}

/// Gain model: signal gain (dB) relative to linearity preset 10.
fn gain_db(g: &Gain) -> f32 {
    match *g {
        Gain::Linearity(x) => (x as f32 - 10.0) * 2.6,
        Gain::Sensitivity(x) => (x as f32 - 10.0) * 2.8 + 3.0,
        Gain::Manual { lna, mixer, vga } => (lna as f32 * 1.1 + mixer as f32 * 1.0 + vga as f32 * 0.9 - 30.0) * 0.95,
    }
}

/// Passband ripple like the measured one (edges up ~1.7 dB, a small fs/4 spur), and the IF filter skirt.
fn passband_db(off: f64, rate: Rate) -> f32 {
    let a = off.abs();
    let edge = rate.max_window_hz() / 2.0 + 0.1e6;
    let x = a / (edge - 0.75e6);
    let mut db = 1.8 * x.powi(10) - 0.35 * (off / 1.1e6).cos() + 0.25 * (off / 0.37e6).sin();
    if a > edge {
        db -= (a - edge) / 0.12e6 * 6.0;
    }
    db as f32
}

#[allow(clippy::too_many_arguments)]
fn synth(out: &mut [u16], rf: f64, rate: Rate, t: f64, rx: &RxSettings, bands: &[Sig], spec: &mut [Complex32], scratch: &mut [Complex32], ifft: &dyn rustfft::Fft<f32>, rng: &mut Rng) {
    let df = rate.adc_hz() / BLOCK_WORDS as f64;
    let fi = rate.if_hz();
    let g = gain_db(&rx.gain);
    let port_db = if rx.port == 0 { 0.0 } else { -38.0 };
    let tuner_floor = 10f32.powf((-104.0 + g * 0.85) / 10.0);
    let adc_floor = 10f32.powf(-117.0 / 10.0);
    let active: Vec<(&Sig, f32)> = bands
        .iter()
        .filter(|s| (s.f - rf).abs() < fi + 0.2e6 + s.reach())
        .filter_map(|s| {
            let on = match s.burst {
                None => true,
                Some((p, duty, ph)) => (t / p + ph).fract() < duty,
            };
            // slow fading
            let fade = 1.2 * ((t * 0.6 + s.f * 1e-7).sin() as f32);
            on.then(|| (s, 10f32.powf((s.db + g + port_db + fade) / 10.0)))
        })
        .collect();
    let half = BLOCK_WORDS / 2;
    spec[0] = Complex32::default();
    spec[half] = Complex32::default();
    for k in 1..half {
        let off = fi - k as f64 * df;
        let f = rf + off;
        let mut p = tuner_floor;
        for (s, lin) in &active {
            let d = f - s.f;
            match s.shape {
                Shape::Bump => {
                    let sigma = (s.bw / 4.0).max(df * 0.6);
                    let z = d / sigma;
                    if z.abs() < 4.0 {
                        p += lin * (-0.5 * z * z).exp() as f32;
                    }
                }
                Shape::Flat(pilot) => {
                    let h = s.bw / 2.0;
                    if d.abs() < h {
                        let edge = ((h - d.abs()) / (s.bw * 0.02)).min(1.0) as f32;
                        p += lin * edge;
                    }
                    if let Some(po) = pilot {
                        let dp = d - (-h + po);
                        if dp.abs() < df {
                            p += lin * 40.0;
                        }
                    }
                }
            }
        }
        // the fs/4 spur sits at offset 0
        if off.abs() < df * 0.5 {
            p += tuner_floor * 4.0;
        }
        let p = p * 10f32.powf(passband_db(off, rate) / 10.0) + adc_floor;
        let a = 2048.0 * (p / 6.0).sqrt() * std::f32::consts::FRAC_1_SQRT_2;
        let c = Complex32::new(a * rng.gauss(), a * rng.gauss());
        spec[k] = c;
        spec[BLOCK_WORDS - k] = c.conj();
    }
    ifft.process_with_scratch(spec, scratch);
    for (o, c) in out.iter_mut().zip(spec.iter()) {
        *o = (2048.0 + c.re).round().clamp(0.0, 4095.0) as u16;
    }
}

/// The HackRF's 15 MHz baseband filter around its LO (flat to about ±6.5 MHz) and its DC spike.
fn hackrf_passband_db(off: f64) -> f32 {
    let a = off.abs();
    let mut db = 0.3 * (off / 1.7e6).sin() - 0.4 * (a / 7.5e6).powi(6);
    if a > 6.8e6 {
        db -= (a - 6.8e6) / 0.25e6 * 3.0;
    }
    db as f32
}

/// The Pluto at 61.44 MSPS (56 MHz RF bandwidth): flat in the middle, the analog filter past ±27 MHz.
fn pluto_passband_db(off: f64, rate: Rate) -> f32 {
    let edge = 0.44 * rate.adc_hz();
    let a = off.abs();
    let mut db = 0.25 * (off / 3.1e6).sin();
    if a > edge {
        db -= (a - edge) / 1.0e6 * 2.5;
    }
    db as f32
}

/// The AD936x's noise-shaped ADC floor: it rises towards the band edges (+5 dB at ±30 MHz, 61.44 MSPS).
fn pluto_floor_shape(off: f64, rate: Rate) -> f32 {
    let x = off / (rate.adc_hz() / 2.0);
    10f32.powf((5.4 * x * x) as f32 / 10.0)
}

/// One complex block around `lo`. HackRF: 8-bit IQ, I in the low byte and Q in the high byte of each word.
/// Pluto: 12-bit IQ, I and Q in a word each (4096 samples per block).
#[allow(clippy::too_many_arguments)]
fn synth_iq(out: &mut [u16], lo: f64, rate: Rate, t: f64, rx: &RxSettings, bands: &[Sig], spec: &mut [Complex32], scratch: &mut [Complex32], ifft: &dyn rustfft::Fft<f32>, rng: &mut Rng) {
    let n = BLOCK_WORDS;
    let pluto = rate.iq16();
    let df = rate.adc_hz() / n as f64;
    let g = gain_db(&rx.gain);
    let (floor, adc_floor, half) = if pluto {
        // 12-bit ADC: a much lower quantisation floor than the HackRF's 8 bits
        (10f32.powf((-104.0 + g * 0.75) / 10.0), 10f32.powf(-78.0 / 10.0) / n as f32, rate.adc_hz() / 2.0)
    } else {
        (10f32.powf((-88.0 + g * 0.6) / 10.0), 10f32.powf(-52.0 / 10.0) / n as f32, 10.2e6)
    };
    let active: Vec<(&Sig, f32)> = bands
        .iter()
        .filter(|s| (s.f - lo).abs() < half + s.reach())
        .filter_map(|s| {
            let on = match s.burst {
                None => true,
                Some((p, duty, ph)) => (t / p + ph).fract() < duty,
            };
            let fade = 1.2 * ((t * 0.6 + s.f * 1e-7).sin() as f32);
            on.then(|| (s, 10f32.powf((s.db + g + fade) / 10.0)))
        })
        .collect();
    for (k, c) in spec.iter_mut().enumerate() {
        let off = if k < n / 2 { k as f64 } else { k as f64 - n as f64 } * df;
        let f = lo + off;
        let mut p = floor;
        for (s, lin) in &active {
            let d = f - s.f;
            match s.shape {
                Shape::Bump => {
                    let sigma = (s.bw / 4.0).max(df * 0.6);
                    let z = d / sigma;
                    if z.abs() < 4.0 {
                        p += lin * (-0.5 * z * z).exp() as f32;
                    }
                }
                Shape::Flat(pilot) => {
                    let h = s.bw / 2.0;
                    if d.abs() < h {
                        p += lin * ((h - d.abs()) / (s.bw * 0.02)).min(1.0) as f32;
                    }
                    if let Some(po) = pilot
                        && (d - (-h + po)).abs() < df
                    {
                        p += lin * 40.0;
                    }
                }
            }
        }
        if off.abs() < df * 1.5 {
            p += floor * if pluto { 6.0 } else { 300.0 }; // the DC spike (small after the AD936x's DC tracking)
        }
        let (p, fs) = if pluto {
            (p * 10f32.powf(pluto_passband_db(off, rate) / 10.0) + adc_floor * pluto_floor_shape(off, rate), 2048.0)
        } else {
            (p * 10f32.powf(hackrf_passband_db(off) / 10.0) + adc_floor, 128.0)
        };
        let a = fs * (p / 3.0).sqrt() * std::f32::consts::FRAC_1_SQRT_2;
        *c = Complex32::new(a * rng.gauss(), a * rng.gauss());
    }
    ifft.process_with_scratch(spec, scratch);
    if pluto {
        for (o, c) in out.as_chunks_mut::<2>().0.iter_mut().zip(spec.iter()) {
            o[0] = c.re.round().clamp(-2047.0, 2047.0) as i16 as u16;
            o[1] = c.im.round().clamp(-2047.0, 2047.0) as i16 as u16;
        }
        return;
    }
    for (o, c) in out.iter_mut().zip(spec.iter()) {
        let i = c.re.round().clamp(-127.0, 127.0) as i8;
        let q = c.im.round().clamp(-127.0, 127.0) as i8;
        *o = (i as u8 as u16) | ((q as u8 as u16) << 8);
    }
}
