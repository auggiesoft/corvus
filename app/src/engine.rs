//! The worker: raw buffers in, a stitched spectrum out.
//!
//! Every valid block is FFT'd and its in-window bins written straight into the stitched array at
//! its step's slot, so the display updates step by step as the firmware sweeps. A pass ends when
//! the block's sweep counter changes (works for both visiting orders); that emits a waterfall row
//! and feeds the passband-flatness learner.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::dsp::{self, Dsp};
use crate::health::{self, Health};
use crate::hydra::{BLOCK_WORDS, Block, Sink, parse_block};
use crate::plan::{Order, Plan, Rate};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DspSettings {
    pub nfft: usize,
    /// Averaging time constant in seconds (0 = the average just follows the live trace).
    pub avg_tau: f32,
    /// Average in linear power (true mean power) rather than in dB (a video average, smoother and
    /// about 2.5 dB lower on noise).
    pub avg_power: bool,
    /// What the peak trace keeps.
    pub peak: PeakMode,
    pub eq_on: bool,
    /// Waterfall rows per second at most (fast narrow plans merge passes by max).
    pub wf_rate: f32,
    /// Live trace smoothing time constant in seconds (0 = the live trace is each pass as measured): readings
    /// near the floor are averaged, bursts above it pass at once (see `update_bins`).
    pub smooth_tau: f32,
    /// The least a reading must rise over its bin's floor to pass as a burst (dB); the engine raises it where
    /// the floor is noisier (`Live::burst_db`).
    pub burst_db: f32,
}

/// The peak trace: a plain hold, a hold that falls back, or a running percentile of each bin.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PeakMode {
    /// Highest level seen since the last clear.
    Hold,
    /// Rises at once, holds for `PEAK_HOLD_S`, then falls at this many dB per second.
    Decay(f32),
    /// The level each bin exceeds `1 - p` of the time (p in 0..1), tracked over about `tau` seconds.
    Percentile { p: f32, tau: f32 },
}

/// How long a decaying peak stays at its top before it starts to fall (the guide's meter hold, D4).
pub const PEAK_HOLD_S: f32 = 1.0;

/// One update of a decaying peak: `t` seconds after the last, with a new reading `v`. Holds for
/// `PEAK_HOLD_S` after it was set, then falls at `rate` dB/s; a reading above it sets it again.
/// Returns the new peak and its age.
fn decay_step(pk: f32, age: f32, v: f32, rate: f32, t: f32) -> (f32, f32) {
    let age = age + t;
    let fallen = pk - rate * (age - PEAK_HOLD_S).clamp(0.0, t);
    if v >= fallen { (v, 0.0) } else { (fallen, age) }
}

/// One update of a streaming quantile: up by `p * eta` when the reading is above, down by
/// `(1 - p) * eta` when below, so it settles where a share `p` of readings fall under it. `eta` lets it
/// climb about 30 dB in one window of `tau` seconds.
fn quantile_step(est: f32, v: f32, p: f32, tau: f32, t: f32) -> f32 {
    let eta = (30.0 * t / tau.max(0.1)).min(10.0);
    if v > est { (est + eta * p).min(v) } else { est - eta * (1.0 - p) }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trace {
    Live,
    Average,
    Peak,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Detector {
    Peak,
    Mean,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum EqState {
    Off,
    /// No correction known for this resolution yet.
    Unknown,
    Learning(u32),
    Learned(u32),
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Stats {
    pub blocks: u64,
    pub bytes: u64,
    pub valid: u64,
    pub torn: u64,
    pub untagged: u64,
    pub misfiled: u64,
    pub gaps: u64,
    pub lock_timeouts: u64,
    pub host_drops: u64,
    pub steps: u64,
    pub passes: u64,
    /// Device-clock rate over the last second.
    pub steps_per_s: f32,
    pub usb_mb_s: f32,
    pub lock_median: u16,
    pub lock_max: u16,
    /// steps/s sampled every 250 ms (last 60 s).
    pub history: VecDeque<f32>,
}

pub const WF_COLS_MAX: usize = 4096;
const ROWS_CAP: usize = 512;
const LEARN_MIN_STEPS: usize = 12;
/// Two looks: a look this close to its step's LO (in bins) is not used where the other look is clear (the LO
/// leakage still wanders 3–6 bins out at 30 kHz).
const DC_BINS: usize = 6;
/// Two looks: a look whose mirror in the same step is this much stronger may be that signal's image.
const IMAGE_DB: f32 = 20.0;
/// Smoothing: a reading `BURST_SDS` spreads plus `BURST_PLUS_DB` over its floor passes as a burst. Noise in
/// dB is skewed (a long tail below, a short one above) and less so the more looks are averaged; this keeps
/// noise passing for fewer than 1 in 10^5 readings from one look (spread 5.3 dB, 13.5 dB needed) to eight
/// (1.6 dB, 5.5 dB needed).
const BURST_SDS: f32 = 2.4;
const BURST_PLUS_DB: f32 = 2.2;
/// Smoothing: added to the burst threshold where a two-look plan has only one look (by each LO, beside
/// strong signals): half the looks, a rougher floor.
const ROUGH_DB: f32 = 3.0;
/// Smoothing: a bin's first readings after a start, a gain or input change set its floor by plain averaging,
/// with nothing passed as a burst: a floor begun low (the first blocks can read well under it) would
/// otherwise take every reading for a burst and climb only at the slow rate.
const WARM_UP: u8 = 8;
/// Smoothing: after a burst the trace falls back quickly until it is this close to its floor.
const RELEASE_DB: f32 = 0.5;

pub struct Live {
    pub generation: u64,
    pub restarts: u64,
    pub plan: Plan,
    pub settings: DspSettings,
    /// Bins per step (with two looks, per segment) and their offsets from the step centre (the LO that ends
    /// the segment).
    pub m: usize,
    pub offs: Vec<f64>,
    /// Bins each step measures (`m`, or with two looks both halves of the step's window).
    mw: usize,
    grid_a: usize,
    /// The live trace (smoothed when `smooth_tau` > 0), and each bin's latest reading as measured.
    pub cur: Vec<f32>,
    pub raw: Vec<f32>,
    /// Smoothing: each bin's floor (a slow average of its quiet readings), the readings' spread about it
    /// (dB, robust, over all bins) and the burst threshold that gives.
    floor: Vec<f32>,
    /// Smoothing: readings behind each bin's floor, up to `WARM_UP` (until then it is a plain average).
    seen: Vec<u8>,
    pub noise_sd: f32,
    pub burst_db: f32,
    pub avg: Vec<f32>,
    pub peak: Vec<f32>,
    /// Seconds since each bin's decaying peak was last set (for the hold before it falls).
    peak_age: Vec<f32>,
    /// Per-bin correction (dB) subtracted from every step; zeros when off or unknown.
    pub eq: Vec<f32>,
    pub eq_state: EqState,
    pub step_t: Vec<f32>,
    pub head: Option<usize>,
    pub head_t: f32,
    pub rows: VecDeque<Vec<f32>>,
    pub wf_cols: usize,
    pub stats: Stats,
    /// Headroom, clipping and (Pluto) image rejection over the latest visit of each step.
    pub health: health::Summary,
    clear_peak: bool,
    reset_eq: bool,
    /// Load and save the learned passband in the cache: on for the radio, off for the simulator and tests.
    persist_eq: bool,
    /// File-name prefix of this board's learned passband (each board's passband differs).
    eq_prefix: String,
}

impl Live {
    fn new(plan: Plan, settings: DspSettings) -> Live {
        let mut l = Live {
            generation: 0,
            restarts: 0,
            plan,
            settings,
            m: 0,
            offs: Vec::new(),
            mw: 0,
            grid_a: 0,
            cur: Vec::new(),
            raw: Vec::new(),
            floor: Vec::new(),
            seen: Vec::new(),
            noise_sd: f32::NAN,
            burst_db: f32::NAN,
            avg: Vec::new(),
            peak: Vec::new(),
            peak_age: Vec::new(),
            eq: Vec::new(),
            eq_state: EqState::Unknown,
            step_t: Vec::new(),
            head: None,
            head_t: f32::NEG_INFINITY,
            rows: VecDeque::new(),
            wf_cols: 0,
            stats: Stats::default(),
            health: health::Summary::default(),
            clear_peak: false,
            reset_eq: false,
            persist_eq: false,
            eq_prefix: crate::hydra::board().cache_prefix().to_string(),
        };
        l.relayout();
        l
    }

    fn relayout(&mut self) {
        let grid: Vec<f64> = dsp::grid(self.settings.nfft, self.plan.rate).iter().map(|g| g.0).collect();
        let r = dsp::window(&grid, self.plan.measure_hz());
        self.grid_a = r.start;
        self.mw = r.len();
        // two looks: each segment is the half of a step's window below its LO
        let r = if self.plan.overlap { r.start..grid.partition_point(|&o| o < 0.0) } else { r };
        self.m = r.len();
        self.offs = grid[r].to_vec();
        let len = self.m * self.plan.segments();
        self.cur = vec![f32::NAN; len];
        self.raw = vec![f32::NAN; len];
        self.floor = vec![f32::NAN; len];
        self.seen = vec![0; len];
        self.avg = vec![f32::NAN; len];
        self.peak = vec![f32::NAN; len];
        self.peak_age = vec![0.0; len];
        self.eq = vec![0.0; self.m];
        self.step_t = vec![f32::NEG_INFINITY; self.plan.n()];
        self.head = None;
        self.rows.clear();
        self.wf_cols = len.min(WF_COLS_MAX);
        self.generation += 1;
    }

    fn reset_floors(&mut self) {
        self.floor.iter_mut().for_each(|f| *f = f32::NAN);
        self.seen.iter_mut().for_each(|n| *n = 0);
        self.noise_sd = f32::NAN;
    }

    pub fn freq(&self, idx: usize) -> f64 {
        self.plan.centre(idx / self.m) + self.offs[idx % self.m]
    }

    /// First index whose frequency is >= f.
    pub fn index_of(&self, f: f64) -> usize {
        let p = &self.plan;
        let lo = p.cover().0;
        let s = ((f - lo) / p.step_hz as f64).floor();
        if s < 0.0 {
            return 0;
        }
        let s = s as usize;
        if s >= p.segments() {
            return self.cur.len();
        }
        s * self.m + self.offs.partition_point(|&o| o < f - p.centre(s))
    }

    fn array(&self, t: Trace) -> &[f32] {
        match t {
            Trace::Live => &self.cur,
            Trace::Average => &self.avg,
            Trace::Peak => &self.peak,
        }
    }

    /// Value at idx with the flatness correction applied.
    #[inline]
    pub fn value(&self, t: Trace, idx: usize) -> f32 {
        self.array(t)[idx] - self.eq[idx % self.m]
    }

    /// The trace between f_lo and f_hi (clipped to the requested range), reduced to about `cols`
    /// points. Returns (Hz, dB) with NaN where nothing has been measured.
    pub fn trace(&self, t: Trace, f_lo: f64, f_hi: f64, cols: usize, det: Detector) -> (Vec<f64>, Vec<f32>) {
        let lo = self.index_of(f_lo.max(self.plan.start_hz));
        let hi = self.index_of(f_hi.min(self.plan.stop_hz));
        let mut xs = Vec::new();
        let mut ys = Vec::new();
        if hi <= lo || cols == 0 {
            return (xs, ys);
        }
        if hi - lo <= cols * 2 {
            for i in lo..hi {
                xs.push(self.freq(i));
                ys.push(self.value(t, i));
            }
            return (xs, ys);
        }
        let n = hi - lo;
        for c in 0..cols {
            let a = lo + n * c / cols;
            let b = (lo + n * (c + 1) / cols).max(a + 1);
            xs.push(0.5 * (self.freq(a) + self.freq(b - 1)));
            ys.push(self.reduce(t, a, b, det));
        }
        (xs, ys)
    }

    fn reduce(&self, t: Trace, a: usize, b: usize, det: Detector) -> f32 {
        let arr = self.array(t);
        let m = self.m;
        let mut acc = match det {
            Detector::Peak => f32::NEG_INFINITY,
            Detector::Mean => 0.0,
        };
        let mut cnt = 0u32;
        let mut i = a;
        while i < b {
            // walk one step's slice at a time to avoid a modulo per bin
            let j0 = i % m;
            let end = (i + (m - j0)).min(b);
            for (v, e) in arr[i..end].iter().zip(&self.eq[j0..j0 + (end - i)]) {
                if v.is_nan() {
                    continue;
                }
                let x = v - e;
                match det {
                    Detector::Peak => acc = acc.max(x),
                    Detector::Mean => acc += x,
                }
                cnt += 1;
            }
            i = end;
        }
        match (cnt, det) {
            (0, _) => f32::NAN,
            (_, Detector::Peak) => acc,
            (_, Detector::Mean) => acc / cnt as f32,
        }
    }

    /// The stitched spectrum within the requested range as CSV (freq, live, peak hold).
    pub fn to_csv(&self) -> String {
        let mut out = String::from("freq_hz,dbfs,peak_dbfs,raw_dbfs\n");
        for i in self.index_of(self.plan.start_hz)..self.index_of(self.plan.stop_hz) {
            let v = self.value(Trace::Live, i);
            if v.is_finite() {
                out.push_str(&format!("{:.0},{:.2},{:.2},{:.2}\n", self.freq(i), v, self.value(Trace::Peak, i), self.raw[i] - self.eq[i % self.m]));
            }
        }
        out
    }

    /// Per-step passband ripple (peak-to-peak dB of the per-offset median across steps, inner 98 %
    /// of the step) without and with the flatness correction: how visible the seams are.
    pub fn ripple(&self) -> Option<(f32, f32)> {
        let (n, m) = (self.plan.n(), self.m);
        if n < 8 || m < 16 {
            return None;
        }
        let mut raw = vec![Vec::with_capacity(n); m];
        let mut tmp = Vec::with_capacity(m);
        for i in 0..n {
            let row = &self.cur[i * m..(i + 1) * m];
            if row.iter().any(|v| !v.is_finite()) {
                continue;
            }
            tmp.clear();
            tmp.extend_from_slice(row);
            let k = tmp.len() / 2;
            let med = *tmp.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1;
            for (j, v) in row.iter().enumerate() {
                raw[j].push(v - med);
            }
        }
        let shape: Vec<f32> = raw
            .iter_mut()
            .map(|c| {
                let k = c.len() / 2;
                *c.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1
            })
            .collect();
        // smooth over ~1 % of the step so bin noise in a single snapshot doesn't count as ripple
        let edge = m / 100;
        let w = (m / 100).max(1);
        let pp = |f: &dyn Fn(usize) -> f32| {
            let v: Vec<f32> = (0..m).map(f).collect();
            let sm: Vec<f32> = (edge..m - edge).map(|j| { let (a, b) = (j.saturating_sub(w), (j + w + 1).min(m)); v[a..b].iter().sum::<f32>() / (b - a) as f32 }).collect();
            sm.iter().copied().fold(f32::MIN, f32::max) - sm.iter().copied().fold(f32::MAX, f32::min)
        };
        Some((pp(&|j| shape[j]), pp(&|j| shape[j] - self.eq[j])))
    }

    fn waterfall_row(&self) -> Vec<f32> {
        let (lo, hi) = (0, self.cur.len());
        let cols = self.wf_cols;
        (0..cols).map(|c| self.reduce(Trace::Live, lo + (hi - lo) * c / cols, (lo + (hi - lo) * (c + 1) / cols).max(lo + 1), Detector::Peak)).collect()
    }
}

/// Write one step's (or segment's) new values from `i0` into the live, average and peak traces. `dt`:
/// seconds since these bins were last written, on the device clock.
///
/// Smoothing the live trace (`smooth_tau` > 0): noise wanders about each bin's floor by a known spread,
/// a transmission jumps well clear of it. So a reading within the burst threshold of its floor is
/// averaged (in dB, over `smooth_tau`), one above it is shown as it is, and once it ends the bin falls back
/// over a quarter of `smooth_tau`. The floor is the dB average of the other readings over `smooth_tau`; a burst
/// moves it only over ten times that, so a carrier that stays becomes the floor and is smoothed too (and
/// takes ~3 `smooth_tau` to fade once it stops: noise's long tail below makes a faster way down misfire). The
/// threshold is `burst_db` or what the readings' spread about their floors needs, whichever is more: the
/// spread is ~2.4 dB with two looks at 30 kHz (8 dB) but ~5.6 dB for one look at the finest bins (~16 dB).
///
/// `rough`: bins with fewer looks than the rest (empty if none), which need `ROUGH_DB` more to pass.
fn update_bins(l: &mut Live, s: DspSettings, i0: usize, vals: &[f32], rough: &[bool], dt: Option<f32>) {
    let alpha = match dt {
        Some(t) if s.avg_tau > 0.0 => 1.0 - (-t / s.avg_tau).exp(),
        _ => 1.0,
    };
    let clear = l.stats.steps == 0;
    let smooth = s.smooth_tau > 0.0;
    if smooth {
        let n = vals.len();
        let mut dev: Vec<f32> = vals.iter().zip(&l.floor[i0..i0 + n]).filter(|(v, f)| v.is_finite() && f.is_finite()).map(|(v, f)| (v - f).abs()).collect();
        if dev.len() >= 16 {
            let k = dev.len() / 2;
            let sd = 1.4826 * *dev.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1;
            l.noise_sd = if l.noise_sd.is_finite() { l.noise_sd + 0.02 * (sd - l.noise_sd) } else { sd };
        }
        l.burst_db = if l.noise_sd.is_finite() { s.burst_db.max(BURST_SDS * l.noise_sd + BURST_PLUS_DB) } else { s.burst_db };
    }
    let thr = l.burst_db;
    let k = |tau: f32| dt.map_or(1.0, |t| 1.0 - (-t / tau).exp());
    let (a, fast, slow) = (k(s.smooth_tau), k(s.smooth_tau / 4.0), k(s.smooth_tau * 10.0));
    for (j, &v) in vals.iter().enumerate() {
        let i = i0 + j;
        l.raw[i] = v;
        if !smooth {
            l.cur[i] = v;
        } else {
            let (fl, sm, n) = (l.floor[i], l.cur[i], l.seen[i]);
            let (fl, sm) = if !fl.is_finite() || !sm.is_finite() || !v.is_finite() || n == 0 {
                l.seen[i] = v.is_finite() as u8;
                (v, v)
            } else if n < WARM_UP || a >= 1.0 {
                l.seen[i] = n + 1;
                let fl = fl + (v - fl) / (n as f32 + 1.0).min(1.0 / a.max(1e-6));
                (fl, fl)
            } else if v - fl > thr + if rough.get(j) == Some(&true) { ROUGH_DB } else { 0.0 } {
                // a burst: shown at once; the floor creeps up only if it stays
                (fl + slow * (v - fl), v)
            } else {
                let fl = fl + a * (v - fl);
                // falling back to the floor after a burst (when quiet, the trace and the floor are the same
                // average), else averaging the noise
                (fl, if sm - fl > RELEASE_DB { fl + (1.0 - fast) * (sm - fl) } else { sm + a * (v - sm) })
            };
            l.floor[i] = fl;
            l.cur[i] = sm;
        }
        let av = &mut l.avg[i];
        *av = if av.is_nan() || alpha >= 1.0 {
            v
        } else if s.avg_power {
            let (pa, pv) = (10f32.powf(*av * 0.1), 10f32.powf(v * 0.1));
            10.0 * (pa + alpha * (pv - pa)).max(1e-30).log10()
        } else {
            *av + alpha * (v - *av)
        };
        let pk = l.peak[i];
        l.peak[i] = if pk.is_nan() || clear {
            l.peak_age[i] = 0.0;
            v
        } else {
            match (s.peak, dt) {
                (PeakMode::Decay(rate), Some(t)) => {
                    let (pk, age) = decay_step(pk, l.peak_age[i], v, rate, t);
                    l.peak_age[i] = age;
                    pk
                }
                (PeakMode::Percentile { p, tau }, Some(t)) => quantile_step(pk, v, p, tau, t),
                (PeakMode::Hold, _) | (_, None) => pk.max(v),
            }
        };
    }
}

pub struct Engine {
    pub live: Arc<Mutex<Live>>,
    tx: SyncSender<Vec<u16>>,
    pub host_drops: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
    pub epoch: Instant,
}

impl Engine {
    /// `persist_eq`: load and save the learned passband in the cache. Off for the simulator and tests, so their
    /// synthetic passband never replaces the one learned from the radio.
    pub fn new(plan: Plan, settings: DspSettings, persist_eq: bool) -> Engine {
        let mut l = Live::new(plan, settings);
        l.persist_eq = persist_eq;
        let live = Arc::new(Mutex::new(l));
        let (tx, rx) = sync_channel(512);
        let stop = Arc::new(AtomicBool::new(false));
        let host_drops = Arc::new(AtomicU64::new(0));
        let epoch = Instant::now();
        let mut w = Worker::new(live.clone(), rx, stop.clone(), host_drops.clone(), epoch);
        let thread = std::thread::Builder::new().name("corvus-dsp".into()).spawn(move || w.run()).expect("spawn worker");
        Engine { live, tx, host_drops, stop, thread: Some(thread), epoch }
    }

    pub fn sink(&self) -> Sink {
        Sink { tx: self.tx.clone(), host_drops: self.host_drops.clone(), host: None }
    }

    pub fn now(&self) -> f32 {
        self.epoch.elapsed().as_secs_f32()
    }

    pub fn set_plan(&self, plan: Plan) {
        let mut l = self.live.lock();
        l.plan = plan;
        l.relayout();
    }

    pub fn set_settings(&self, s: DspSettings) {
        let mut l = self.live.lock();
        let relayout = l.settings.nfft != s.nfft;
        l.settings = s;
        if relayout {
            l.relayout();
        }
    }

    /// The sweep was restarted (gain/port change, resume): the step sequence starts over.
    pub fn note_restart(&self) {
        self.live.lock().restarts += 1;
    }

    /// Switch passband persistence when the source changes (radio: on, simulator: off). The worker saves what the
    /// radio taught before going off, and reloads from the cache when coming back on.
    pub fn set_persist_eq(&self, on: bool) {
        self.live.lock().persist_eq = on;
    }

    /// Another board: the worker saves what it learned under the old board's name and loads the new one's.
    pub fn set_eq_prefix(&self, prefix: &str) {
        self.live.lock().eq_prefix = prefix.to_string();
    }

    pub fn clear_peak(&self) {
        self.live.lock().clear_peak = true;
    }

    pub fn reset_eq(&self) {
        self.live.lock().reset_eq = true;
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Done {
    step: u16,
    sweep: u32,
    db: Vec<f32>,
    lock_polls: u16,
    dma: u32,
}

/// The firmware's visiting order, to spot sequence gaps (port of `Order` in scripts/hydrasdr_sweep.py).
struct OrderModel {
    vco: bool,
    seq: Vec<u16>,
    pos: Vec<usize>,
    s0: Option<u32>,
    /// Whether the pass direction has been confirmed. After a reset that isn't a restart (a new bin
    /// width), the first pass seen may be a backward one; the first mismatch flips the guess.
    sure: bool,
}

impl OrderModel {
    fn new(plan: &Plan) -> OrderModel {
        let seq = plan.sequence();
        let mut pos = vec![0; seq.len()];
        for (i, &s) in seq.iter().enumerate() {
            pos[s as usize] = i;
        }
        OrderModel { vco: plan.order == Order::VcoContinuous, seq, pos, s0: None, sure: false }
    }

    fn next(&mut self, (swp, step): (u32, u16)) -> (u32, u16) {
        let s0 = *self.s0.get_or_insert(swp);
        let (i, n) = (self.pos[step as usize], self.seq.len());
        if !self.vco {
            return if i + 1 < n { (swp, self.seq[i + 1]) } else { (swp + 1, self.seq[0]) };
        }
        if swp.wrapping_sub(s0) % 2 == 0 {
            if i + 1 < n { (swp, self.seq[i + 1]) } else { (swp + 1, step) }
        } else if i > 0 {
            (swp, self.seq[i - 1])
        } else {
            (swp + 1, step)
        }
    }
}

struct Worker {
    live: Arc<Mutex<Live>>,
    rx: Receiver<Vec<u16>>,
    stop: Arc<AtomicBool>,
    host_drops: Arc<AtomicU64>,
    epoch: Instant,
    dsp: Dsp,
    health: Health,
    generation: u64,
    restarts: u64,
    plan: Plan,
    settings: DspSettings,
    range: std::ops::Range<usize>,
    /// Two looks: each step's latest measurement (dB, as measured) for the passband and the image check,
    /// the two looks at every segment (0: the lower half of the step that ends it, 1: the upper half of the
    /// one before), whether each is clear of that step's DC and of a strong mirror, the passband correction
    /// over a step's window, and each segment's block counter at its previous update.
    raw: Vec<f32>,
    looks: [Vec<f32>; 2],
    clear: [Vec<bool>; 2],
    eq_w: Vec<f32>,
    seg_dma: Vec<Option<u32>>,
    acc: Vec<f32>,
    acc_n: u32,
    acc_key: Option<(u32, u16)>,
    acc_done: bool,
    /// Since the last restart, the new run's first block (block 0 of the first step in visiting
    /// order) hasn't arrived. Until it does, other blocks are the old plan's, still in flight, and are
    /// dropped without counting as misfiled or as a gap. Gives up after 256 blocks.
    settling: bool,
    settle_dropped: u32,
    acc_lock: u16,
    acc_dma: u32,
    /// Each step's block counter at its previous visit, for the time between visits (device clock).
    step_dma: Vec<Option<u32>>,
    scratch: Vec<f32>,
    done: Vec<Done>,
    order: OrderModel,
    last_key: Option<(u32, u16)>,
    last_sweep: Option<u32>,
    /// Learned passband shape per (rate, FFT size), on that grid, with the passes behind it.
    eq_store: HashMap<(Rate, usize), (Vec<f32>, u32)>,
    persist_eq: bool,
    eq_prefix: String,
    eq_t: f32,
    eq_saved_t: f32,
    row_acc: Option<Vec<f32>>,
    row_t: f32,
    rate_win: VecDeque<(u32, u64)>,
    usb_win: VecDeque<(f32, u64)>,
    locks: VecDeque<u16>,
    stats_t: f32,
    last_step_t: f32,
}

impl Worker {
    fn new(live: Arc<Mutex<Live>>, rx: Receiver<Vec<u16>>, stop: Arc<AtomicBool>, host_drops: Arc<AtomicU64>, epoch: Instant) -> Worker {
        let (plan, settings, persist_eq, eq_prefix) = {
            let l = live.lock();
            (l.plan.clone(), l.settings, l.persist_eq, l.eq_prefix.clone())
        };
        let eq_store = if persist_eq { load_eq_store(&eq_prefix) } else { HashMap::new() };
        Worker {
            dsp: Dsp::new(settings.nfft, plan.rate),
            health: Health::new(&plan),
            order: OrderModel::new(&plan),
            live,
            rx,
            stop,
            host_drops,
            epoch,
            generation: u64::MAX,
            restarts: 0,
            plan,
            settings,
            range: 0..0,
            raw: Vec::new(),
            looks: [Vec::new(), Vec::new()],
            clear: [Vec::new(), Vec::new()],
            eq_w: Vec::new(),
            seg_dma: Vec::new(),
            acc: Vec::new(),
            acc_n: 0,
            acc_key: None,
            acc_done: false,
            settling: true,
            settle_dropped: 0,
            acc_lock: 0,
            acc_dma: 0,
            step_dma: Vec::new(),
            scratch: Vec::new(),
            done: Vec::new(),
            last_key: None,
            last_sweep: None,
            eq_store,
            persist_eq,
            eq_prefix,
            eq_t: 0.0,
            eq_saved_t: 0.0,
            row_acc: None,
            row_t: 0.0,
            rate_win: VecDeque::new(),
            usb_win: VecDeque::new(),
            locks: VecDeque::new(),
            stats_t: 0.0,
            last_step_t: 0.0,
        }
    }

    fn now(&self) -> f32 {
        self.epoch.elapsed().as_secs_f32()
    }

    fn run(&mut self) {
        while !self.stop.load(Ordering::Relaxed) {
            match self.rx.recv_timeout(Duration::from_millis(100)) {
                Ok(buf) => self.process(&buf),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let now = self.now();
            if now - self.stats_t >= 0.25 {
                self.stats_t = now;
                self.update_stats(now);
            }
        }
        self.save_eq();
    }

    /// Pick up plan/settings changes made by the UI.
    fn sync(&mut self) {
        let live = self.live.clone();
        let mut l = live.lock();
        if l.restarts != self.restarts {
            self.restarts = l.restarts;
            // a restart is a gain, input or resume: the floors start again
            l.reset_floors();
            self.reset_sequence(true);
        }
        if l.persist_eq != self.persist_eq || l.eq_prefix != self.eq_prefix {
            if self.persist_eq {
                self.save_eq();
            }
            self.persist_eq = l.persist_eq;
            self.eq_prefix = l.eq_prefix.clone();
            self.eq_store = if self.persist_eq { load_eq_store(&self.eq_prefix) } else { HashMap::new() };
            self.publish_eq(&mut l);
        }
        if l.reset_eq {
            l.reset_eq = false;
            self.eq_store.remove(&(l.plan.rate, l.settings.nfft));
            if self.persist_eq {
                let _ = std::fs::remove_file(eq_path(&self.eq_prefix, l.plan.rate, l.settings.nfft));
            }
            self.publish_eq(&mut l);
        }
        // a new peak mode starts from the live trace, like Clear peaks
        if l.clear_peak || l.settings.peak != self.settings.peak {
            l.clear_peak = false;
            let cur = l.cur.clone();
            l.peak = cur;
            l.peak_age.iter_mut().for_each(|a| *a = 0.0);
        }
        // a new averaging time or kind restarts the average, so it shows at once
        if l.settings.avg_tau != self.settings.avg_tau || l.settings.avg_power != self.settings.avg_power {
            l.avg.iter_mut().for_each(|a| *a = f32::NAN);
        }
        // smoothing turned on or off: start the floors afresh
        if (l.settings.smooth_tau > 0.0) != (self.settings.smooth_tau > 0.0) {
            l.reset_floors();
        }
        let eq_toggled = l.settings.eq_on != self.settings.eq_on;
        self.settings = l.settings;
        if l.generation != self.generation {
            self.generation = l.generation;
            let restarted = self.plan != l.plan;
            self.plan = l.plan.clone();
            if self.dsp.nfft != self.settings.nfft || self.dsp.rate != self.plan.rate {
                self.dsp = Dsp::new(self.settings.nfft, self.plan.rate);
            }
            self.range = l.grid_a..l.grid_a + l.mw;
            self.acc = vec![0.0; l.mw];
            self.scratch = vec![0.0; l.mw];
            let (segs, m) = (self.plan.segments(), l.m);
            self.raw = if self.plan.overlap { vec![f32::NAN; self.plan.n() * l.mw] } else { Vec::new() };
            self.looks = [vec![f32::NAN; segs * m], vec![f32::NAN; segs * m]];
            self.clear = [vec![false; segs * m], vec![false; segs * m]];
            self.seg_dma = vec![None; segs];
            self.order = OrderModel::new(&self.plan);
            self.health = Health::new(&self.plan);
            self.step_dma = vec![None; self.plan.n()];
            self.row_acc = None;
            self.rate_win.clear();
            self.reset_sequence(restarted);
            self.publish_eq(&mut l);
        } else if eq_toggled {
            self.publish_eq(&mut l);
        }
    }

    /// Start tracking the step sequence afresh. `restarted`: the radio restarted the sweep, so blocks
    /// of the old run may still be in flight.
    fn reset_sequence(&mut self, restarted: bool) {
        self.settling = restarted;
        self.settle_dropped = 0;
        self.order.sure = false;
        self.acc_key = None;
        self.acc_done = false;
        self.last_key = None;
        self.last_sweep = None;
        self.order.s0 = None;
    }

    fn process(&mut self, buf: &[u16]) {
        self.sync();
        let nb = buf.len() / BLOCK_WORDS;
        let (mut torn, mut untagged, mut misfiled, mut timeouts, mut valid) = (0, 0, 0, 0, 0);
        for w in buf.chunks_exact(BLOCK_WORDS) {
            let t = match parse_block(w) {
                Block::Untagged => {
                    untagged += 1;
                    continue;
                }
                Block::Torn => {
                    torn += 1;
                    continue;
                }
                Block::Valid(t) => t,
            };
            let p = &self.plan;
            // u32 like the tags (the HackRF's 6 GHz wraps; both sides wrap the same way)
            let want = (p.f0 as u32).wrapping_add(p.step_hz.wrapping_mul(t.step as u32));
            let fits = t.step < p.n_steps && t.freq_hz == want;
            if self.settling && self.settle_dropped < 256 && !(fits && t.block == 0 && self.order.seq.first() == Some(&t.step)) {
                self.settle_dropped += 1;
                continue;
            }
            self.settling = false;
            if !fits {
                misfiled += 1;
                continue;
            }
            if t.lock_timed_out {
                timeouts += 1;
                continue;
            }
            valid += 1;
            let key = (t.sweep, t.step);
            if self.acc_key != Some(key) {
                if self.acc_key.is_some() && !self.acc_done && self.acc_n > 0 {
                    self.finalize();
                }
                self.acc_key = Some(key);
                self.acc.iter_mut().for_each(|a| *a = 0.0);
                self.acc_n = 0;
                self.acc_done = false;
                self.acc_lock = if self.plan.rate.complex() { 0 } else { t.lock_polls };
                self.acc_dma = t.dma_count;
            }
            if self.acc_done {
                continue;
            }
            self.health.block(t.step as usize, w, self.acc_n == 0);
            // HackRF blocks carry their slice (below or above the LO) in the lock field; see hackrf.rs
            let side = if self.plan.rate.complex() { (t.lock_polls & 1) as u8 } else { 0 };
            self.dsp.power(w, self.range.clone(), &mut self.scratch, side);
            for (a, s) in self.acc.iter_mut().zip(&self.scratch) {
                *a += s;
            }
            self.acc_n += 1;
            if t.block as u32 + 1 >= self.plan.dwell as u32 {
                self.finalize();
                self.acc_done = true;
            }
        }
        let now = self.now();
        let mut eq_snapshot = None;
        {
            let live = self.live.clone();
            let mut l = live.lock();
            let s = &mut l.stats;
            s.blocks += nb as u64;
            s.bytes += (buf.len() * 2) as u64;
            s.valid += valid;
            s.torn += torn;
            s.untagged += untagged;
            s.misfiled += misfiled;
            s.lock_timeouts += timeouts;
            if l.generation == self.generation {
                let done = std::mem::take(&mut self.done);
                for d in &done {
                    if let Some(snap) = self.apply(&mut l, d, now) {
                        eq_snapshot = Some(snap);
                    }
                }
                self.done = done;
                self.done.clear();
            } else {
                self.done.clear();
            }
        }
        if let Some((cur, m)) = eq_snapshot {
            self.learn_eq(&cur, m);
        }
    }

    fn finalize(&mut self) {
        let (sweep, step) = self.acc_key.unwrap();
        let n = self.acc_n.max(1) as f32;
        let db = self.acc.iter().map(|&a| 10.0 * (a / n + 1e-20).log10()).collect();
        self.done.push(Done { step, sweep, db, lock_polls: self.acc_lock, dma: self.acc_dma });
    }

    /// Write one finished step into the stitched arrays. Returns a copy of the live spectrum when
    /// it is time to learn the passband shape from it.
    fn apply(&mut self, l: &mut Live, d: &Done, now: f32) -> Option<(Vec<f32>, usize)> {
        let mut snap = None;
        if let Some(prev) = self.last_sweep
            && d.sweep != prev
        {
            snap = self.pass_complete(l, now);
        }
        self.last_sweep = Some(d.sweep);
        let key = (d.sweep, d.step);
        if let Some(lk) = self.last_key {
            let mut ok = self.order.next(lk) == key;
            if !ok && !self.order.sure && self.order.vco {
                // the first pass seen was walked the other way: flip the direction guess
                self.order.s0 = self.order.s0.map(|s| s.wrapping_add(1));
                ok = self.order.next(lk) == key;
                if !ok {
                    self.order.s0 = self.order.s0.map(|s| s.wrapping_sub(1));
                }
            }
            if ok {
                self.order.sure = true;
            } else {
                l.stats.gaps += 1;
            }
        }
        self.last_key = Some(key);
        let m = l.m;
        let i0 = d.step as usize * m;
        // seconds since this step was last measured, on the device clock (several visits can arrive in
        // one batch on a narrow plan, so the host clock won't do)
        let dt = match self.step_dma.get_mut(d.step as usize) {
            Some(slot) => {
                let dt = slot.map(|p| (d.dma.wrapping_sub(p) as f64 * self.plan.rate.block_s()) as f32);
                *slot = Some(d.dma);
                dt.filter(|t| *t > 0.0 && *t < 60.0)
            }
            None => None,
        };
        if self.plan.overlap {
            self.apply_looks(l, d);
        } else {
            update_bins(l, self.settings, i0, &d.db[..m], &[], dt);
        }
        l.step_t[d.step as usize] = now;
        l.head = Some(d.step as usize);
        l.head_t = now;
        l.stats.steps += 1;
        self.last_step_t = now;
        self.rate_win.push_back((d.dma, l.stats.steps));
        while let (Some(f), Some(b)) = (self.rate_win.front(), self.rate_win.back()) {
            if b.0.wrapping_sub(f.0) as f64 * self.plan.rate.block_s() > 1.0 { self.rate_win.pop_front(); } else { break; }
        }
        self.locks.push_back(d.lock_polls);
        if self.locks.len() > 2048 {
            self.locks.pop_front();
        }
        snap
    }

    /// Two looks: file a step's lower half as the first look at the segment that ends at its LO and its
    /// upper half as the second look at the next, then rebuild both segments from their two looks.
    ///
    /// A look is clear unless it sits on the step's DC (the bins bridged over the LO leakage) or its mirror
    /// in the same step is `IMAGE_DB` stronger (it may hold that signal's image, 30–48 dB down on the Pluto).
    /// Two clear looks are averaged in power (a steadier floor; a burst caught by one reads 3 dB low); one
    /// clear look is used alone; with neither, the lower wins (an image or spur rarely sits in both).
    fn apply_looks(&mut self, l: &mut Live, d: &Done) {
        let (m, mw, step) = (l.m, l.mw, d.step as usize);
        let raw = &d.db;
        self.raw[step * mw..(step + 1) * mw].copy_from_slice(raw);
        // offset 0 is bin m of the window (the lower half is m bins); the mirror of bin b is bin 2m - b
        let mirror = |b: usize| -> f32 {
            let c = (2 * m) as isize - b as isize;
            (c - 1..=c + 1).filter(|&k| k >= 0 && (k as usize) < mw).map(|k| raw[k as usize]).fold(f32::NEG_INFINITY, f32::max)
        };
        for half in 0..2 {
            let seg = step + half;
            let base = seg * m;
            for j in 0..m {
                let b = j + half * m;
                let (v, ok) = match raw.get(b) {
                    Some(&r) => (r - self.eq_w.get(b).copied().unwrap_or(0.0), (b as isize - m as isize).unsigned_abs() > DC_BINS && mirror(b) - r < IMAGE_DB),
                    None => (f32::NAN, false),
                };
                self.looks[half][base + j] = v;
                self.clear[half][base + j] = ok;
            }
            let mut rough = vec![false; m];
            let vals: Vec<f32> = (base..base + m)
                .map(|i| {
                    let (a, b) = (self.looks[0][i], self.looks[1][i]);
                    let (ca, cb) = (self.clear[0][i] && a.is_finite(), self.clear[1][i] && b.is_finite());
                    rough[i - base] = !(ca && cb);
                    match (ca, cb) {
                        (true, true) => 10.0 * ((10f32.powf(a * 0.1) + 10f32.powf(b * 0.1)) * 0.5).log10(),
                        (true, false) => a,
                        (false, true) => b,
                        // f32::min skips a NaN look
                        (false, false) => a.min(b),
                    }
                })
                .collect();
            let dt = match self.seg_dma.get_mut(seg) {
                Some(slot) => {
                    let dt = slot.map(|p| (d.dma.wrapping_sub(p) as f64 * self.plan.rate.block_s()) as f32);
                    *slot = Some(d.dma);
                    dt.filter(|t| *t > 0.0 && *t < 60.0)
                }
                None => None,
            };
            update_bins(l, self.settings, base, &vals, &rough, dt);
        }
    }

    fn pass_complete(&mut self, l: &mut Live, now: f32) -> Option<(Vec<f32>, usize)> {
        l.stats.passes += 1;
        let row = l.waterfall_row();
        match &mut self.row_acc {
            Some(acc) => acc.iter_mut().zip(&row).for_each(|(a, r)| *a = a.max(*r)),
            None => self.row_acc = Some(row),
        }
        if now - self.row_t >= 1.0 / self.settings.wf_rate.max(0.5) {
            self.row_t = now;
            l.rows.push_back(self.row_acc.take().unwrap());
            while l.rows.len() > ROWS_CAP {
                l.rows.pop_front();
            }
        }
        if self.plan.n() >= LEARN_MIN_STEPS && now - self.eq_t >= 0.5 {
            self.eq_t = now;
            // the passband is a step's: with two looks, learn it from the steps as measured
            return Some(if self.plan.overlap { (self.raw.clone(), l.mw) } else { (l.cur.clone(), l.m) });
        }
        None
    }

    /// Passband shape = per-bin median, over all steps, of each step's level relative to its own
    /// median. Signals occupy different offsets in different steps, so the median rejects them.
    fn learn_eq(&mut self, cur: &[f32], m: usize) {
        let n = cur.len() / m;
        let mut rel: Vec<f32> = Vec::with_capacity(cur.len());
        let mut tmp = Vec::with_capacity(m);
        let mut rows = 0;
        for i in 0..n {
            let row = &cur[i * m..(i + 1) * m];
            if row.iter().any(|v| v.is_nan()) {
                continue;
            }
            tmp.clear();
            tmp.extend_from_slice(row);
            let med = median(&mut tmp);
            rel.extend(row.iter().map(|v| v - med));
            rows += 1;
        }
        if rows < LEARN_MIN_STEPS {
            return;
        }
        let mut col = Vec::with_capacity(rows);
        let mut shape = vec![0.0; m];
        for (j, s) in shape.iter_mut().enumerate() {
            col.clear();
            col.extend((0..rows).map(|i| rel[i * m + j]));
            *s = median(&mut col);
        }
        let glen = self.dsp.grid_len();
        let (eq, passes) = self.eq_store.entry((self.plan.rate, self.settings.nfft)).or_insert_with(|| (vec![f32::NAN; glen], 0));
        for (j, s) in shape.iter().enumerate() {
            let e = &mut eq[self.range.start + j];
            *e = if e.is_nan() { *s } else { *e + 0.15 * (s - *e) };
        }
        *passes += 1;
        let live = self.live.clone();
        let mut l = live.lock();
        if l.generation == self.generation {
            self.publish_eq(&mut l);
        }
        let now = self.now();
        if now - self.eq_saved_t > 30.0 {
            self.eq_saved_t = now;
            self.save_eq();
        }
    }

    fn publish_eq(&mut self, l: &mut Live) {
        let learning = self.plan.n() >= LEARN_MIN_STEPS;
        let entry = self.eq_store.get(&(self.plan.rate, self.settings.nfft));
        let known = entry.map(|(eq, _)| eq[self.range.clone()].iter().all(|v| !v.is_nan())).unwrap_or(false);
        l.eq_state = match (self.settings.eq_on, entry, known) {
            (false, _, _) => EqState::Off,
            (true, Some((_, p)), true) if learning && *p < 20 => EqState::Learning(*p),
            (true, Some((_, p)), true) => EqState::Learned(*p),
            (true, Some((_, p)), false) if learning => EqState::Learning(*p),
            _ if learning => EqState::Learning(0),
            _ => EqState::Unknown,
        };
        let m = l.m;
        if !self.settings.eq_on || !known {
            l.eq = vec![0.0; m];
            self.eq_w = Vec::new();
            return;
        }
        let eq = &entry.unwrap().0[self.range.clone()];
        // Keep the overall level: remove the correction's own median.
        let mut tmp = eq.to_vec();
        let med = median(&mut tmp);
        let eq: Vec<f32> = eq.iter().map(|v| v - med).collect();
        // two looks: corrected per step before the looks are combined, so the display gets none
        if self.plan.overlap {
            l.eq = vec![0.0; m];
            self.eq_w = eq;
        } else {
            l.eq = eq;
            self.eq_w = Vec::new();
        }
    }

    fn save_eq(&self) {
        if !self.persist_eq {
            return;
        }
        for ((rate, nfft), (eq, passes)) in &self.eq_store {
            if *passes > 0 && eq.iter().any(|v| !v.is_nan()) {
                let path = eq_path(&self.eq_prefix, *rate, *nfft);
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                let bytes: Vec<u8> = eq.iter().flat_map(|v| v.to_le_bytes()).collect();
                if let Err(e) = std::fs::write(&path, bytes) {
                    log::warn!("saving {}: {e}", path.display());
                }
            }
        }
    }

    fn update_stats(&mut self, now: f32) {
        let health = self.health.summary();
        let mut l = self.live.lock();
        l.health = health;
        let s = &mut l.stats;
        s.host_drops = self.host_drops.load(Ordering::Relaxed);
        s.steps_per_s = match (self.rate_win.front(), self.rate_win.back()) {
            (Some(f), Some(b)) if b.0 != f.0 && now - self.last_step_t < 1.0 => {
                ((b.1 - f.1) as f64 / (b.0.wrapping_sub(f.0) as f64 * self.plan.rate.block_s())) as f32
            }
            _ => 0.0,
        };
        self.usb_win.push_back((now, s.bytes));
        while self.usb_win.len() > 5 {
            self.usb_win.pop_front();
        }
        if let (Some(f), Some(b)) = (self.usb_win.front(), self.usb_win.back())
            && b.0 > f.0
        {
            s.usb_mb_s = ((b.1 - f.1) as f64 / (b.0 - f.0) as f64 / 1e6) as f32;
        }
        if !self.locks.is_empty() {
            let mut v: Vec<u16> = self.locks.iter().copied().collect();
            v.sort_unstable();
            s.lock_median = v[v.len() / 2];
            s.lock_max = *v.last().unwrap();
        }
        let r = s.steps_per_s;
        s.history.push_back(r);
        while s.history.len() > 240 {
            s.history.pop_front();
        }
    }
}

fn median(v: &mut [f32]) -> f32 {
    let k = v.len() / 2;
    *v.select_nth_unstable_by(k, |a, b| a.total_cmp(b)).1
}

fn eq_path(prefix: &str, rate: Rate, nfft: usize) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from)) // Windows
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    // 10 MSPS keeps the name it had before 12 MSPS existed
    let name = match rate {
        Rate::M10 => format!("passband-eq-{nfft}.f32"),
        Rate::M12 => format!("passband-eq-12msps-{nfft}.f32"),
        Rate::M6 => format!("passband-eq-6msps-{nfft}.f32"),
        Rate::H20 => format!("passband-eq-20msps-iq-{nfft}.f32"),
        Rate::P61 => format!("passband-eq-61msps-iq-{nfft}.f32"),
        Rate::P31 => format!("passband-eq-31msps-iq-{nfft}.f32"),
        Rate::P15 => format!("passband-eq-15msps-iq-{nfft}.f32"),
        Rate::P7 => format!("passband-eq-7msps-iq-{nfft}.f32"),
        Rate::P3 => format!("passband-eq-3msps-iq-{nfft}.f32"),
    };
    base.join("corvus-sweep").join(format!("{prefix}{name}"))
}

/// Every learned passband in the cache, counted as 10 passes so new data can still move it.
fn load_eq_store(prefix: &str) -> HashMap<(Rate, usize), (Vec<f32>, u32)> {
    let mut store = HashMap::new();
    for rate in [Rate::M10, Rate::M12, Rate::M6, Rate::H20, Rate::P61, Rate::P31, Rate::P15, Rate::P7, Rate::P3] {
        for nfft in dsp::NFFTS {
            if let Some(v) = load_eq(prefix, rate, nfft) {
                store.insert((rate, nfft), (v, 10));
            }
        }
    }
    store
}

fn load_eq(prefix: &str, rate: Rate, nfft: usize) -> Option<Vec<f32>> {
    let bytes = std::fs::read(eq_path(prefix, rate, nfft)).ok()?;
    let v: Vec<f32> = bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    (v.len() == dsp::grid(nfft, rate).len()).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hydra::{TAG_WORDS, Tag, tag_block};

    #[test]
    fn index_and_freq_agree() {
        for rate in [Rate::M10, Rate::M12] {
            let plan = Plan::new(24.0, 1800.0, rate.default_step_mhz(), 1, 0, 2, Order::VcoContinuous, rate);
            let l = Live::new(plan, DspSettings { nfft: 8192, avg_tau: 0.0, avg_power: false, peak: PeakMode::Hold, eq_on: false, wf_rate: 30.0, smooth_tau: 0.0, burst_db: 8.0 });
            for f in [24e6, 100e6, 433.92e6, 1799.9e6] {
                let i = l.index_of(f);
                assert!(l.freq(i) >= f && l.freq(i) - f < 3e3, "{rate:?} {f} -> {}", l.freq(i));
            }
        }
    }

    #[test]
    fn worker_stitches_steps_and_counts_passes() {
        let plan = Plan::new(400.0, 440.0, 8.0, 1, 0, 2, Order::Ascending, Rate::M10);
        let eng = Engine::new(plan.clone(), DspSettings { nfft: 2048, avg_tau: 0.0, avg_power: false, peak: PeakMode::Hold, eq_on: false, wf_rate: 1000.0, smooth_tau: 0.0, burst_db: 8.0 }, false);
        let sink = eng.sink();
        let mut cnt = 0;
        for sweep in 1..=3u32 {
            let mut buf = vec![2048u16; BLOCK_WORDS * plan.n()];
            for (k, w) in buf.chunks_exact_mut(BLOCK_WORDS).enumerate() {
                cnt += 3;
                for (n, x) in w.iter_mut().enumerate() {
                    *x = (2048.0 + 300.0 * (n as f64 * 1.3).sin()) as u16;
                }
                tag_block(w, &Tag { step: k as u16, block: 0, sweep, freq_hz: plan.centre(k) as u32, dma_count: cnt, lock_polls: 3, lock_timed_out: false });
            }
            sink.push(buf);
        }
        std::thread::sleep(Duration::from_millis(400));
        let l = eng.live.lock();
        assert_eq!(l.stats.steps, 15);
        assert_eq!(l.stats.passes, 2);
        assert_eq!(l.stats.gaps, 0);
        assert!(l.cur.iter().all(|v| v.is_finite()));
        assert!(!l.rows.is_empty()); // passes this close together merge into one row
    }

    /// Blocks still in flight from the old plan must not count as misfiled or as gaps after a
    /// deliberate change, whether the grid changes (Fastest <-> Quality) or not (Fastest <-> Bursts),
    /// and a bin-width change mid-pass (possibly mid backward pass) must not either.
    #[test]
    fn plan_changes_raise_no_integrity_issues() {
        use crate::hydra::{Gain, RxSettings};
        use crate::sim::Sim;
        let fastest = Plan::new(24.0, 1800.0, 10.0, 1, 0, 2, Order::VcoContinuous, Rate::M12);
        let quality = Plan::new(24.0, 1800.0, 8.0, 4, 1, 3, Order::VcoContinuous, Rate::M12);
        let bursts = Plan::new(24.0, 1800.0, 10.0, 4, 0, 2, Order::VcoContinuous, Rate::M12);
        let eng = Engine::new(fastest.clone(), DspSettings { nfft: 2048, avg_tau: 0.0, avg_power: false, peak: PeakMode::Hold, eq_on: false, wf_rate: 20.0, smooth_tau: 0.0, burst_db: 8.0 }, false);
        let sim = Sim::start(eng.sink(), fastest.clone(), RxSettings { port: 0, gain: Gain::Linearity(10) });
        std::thread::sleep(Duration::from_millis(500));
        for p in [&quality, &fastest, &bursts, &fastest, &quality, &bursts] {
            eng.set_plan(p.clone());
            sim.sweep(p);
            std::thread::sleep(Duration::from_millis(400));
        }
        // a new bin width re-lays out the engine mid-pass, without restarting the radio
        for nfft in [1024, 4096, 2048, 512] {
            eng.set_settings(DspSettings { nfft, avg_tau: 0.0, avg_power: false, peak: PeakMode::Hold, eq_on: false, wf_rate: 20.0, smooth_tau: 0.0, burst_db: 8.0 });
            std::thread::sleep(Duration::from_millis(330));
        }
        let l = eng.live.lock();
        assert!(l.stats.steps > 500, "the sweep kept running ({} steps)", l.stats.steps);
        assert_eq!((l.stats.gaps, l.stats.misfiled, l.stats.torn), (0, 0, 0));
    }

    #[test]
    fn decaying_peak_holds_then_falls() {
        // set at -40, then quiet readings at -90 every 0.2 s, falling at 6 dB/s after a 1 s hold
        let (mut pk, mut age) = (-40.0f32, 0.0f32);
        let mut at = Vec::new();
        for _ in 0..15 {
            (pk, age) = decay_step(pk, age, -90.0, 6.0, 0.2);
            at.push(pk);
        }
        assert!((at[3] + 40.0).abs() < 1e-4, "still held at 0.8 s: {}", at[3]);
        assert!((at[14] - (-40.0 - 6.0 * 2.0)).abs() < 1e-3, "fallen 12 dB after 3 s: {}", at[14]);
        // a new reading above it sets it again, and the hold restarts
        (pk, age) = decay_step(pk, age, -45.0, 6.0, 0.2);
        assert_eq!((pk, age), (-45.0, 0.0));
        // it never falls below what it's reading
        let (pk, _) = decay_step(-50.0, 30.0, -52.0, 30.0, 1.0);
        assert_eq!(pk, -52.0);
    }

    #[test]
    fn percentile_settles_at_the_quantile() {
        // readings spread evenly over -100..-80 dB: the 90th percentile is -82
        let mut seed = 12345u32;
        let mut est = -100.0f32;
        let mut tail = Vec::new();
        for k in 0..20000 {
            seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
            let v = -100.0 + 20.0 * (seed >> 8) as f32 / (1u32 << 24) as f32;
            est = quantile_step(est, v, 0.9, 10.0, 0.2);
            if k > 10000 {
                tail.push(est);
            }
        }
        let mean = tail.iter().sum::<f32>() / tail.len() as f32;
        assert!((mean + 82.0).abs() < 0.6, "P90 estimate {mean}");
    }

    #[test]
    fn hackrf_interleaved_slices_stitch_without_gaps() {
        use crate::hydra::{Gain, RxSettings};
        use crate::sim::Sim;
        // 1-6000 MHz crosses 4.29 GHz, where the 32-bit tag frequencies wrap
        let wide = Plan::new(1.0, 6000.0, 5.0, 1, 0, 0, Order::Interleaved, Rate::H20);
        let narrow = Plan::new(2400.0, 2500.0, 5.0, 2, 0, 0, Order::Interleaved, Rate::H20);
        let eng = Engine::new(wide.clone(), DspSettings { nfft: 1024, avg_tau: 0.0, avg_power: false, peak: PeakMode::Hold, eq_on: false, wf_rate: 20.0, smooth_tau: 0.0, burst_db: 8.0 }, false);
        let sim = Sim::start(eng.sink(), wide.clone(), RxSettings { port: 0, gain: Gain::Linearity(10) });
        std::thread::sleep(Duration::from_millis(1200));
        for p in [&narrow, &wide] {
            eng.set_plan(p.clone());
            sim.sweep(p);
            std::thread::sleep(Duration::from_millis(900));
        }
        let l = eng.live.lock();
        assert!(l.stats.steps > 1500, "the sweep kept running ({} steps)", l.stats.steps);
        assert!(l.stats.passes >= 1, "a 1-6000 MHz pass completed");
        assert_eq!((l.stats.gaps, l.stats.misfiled, l.stats.torn), (0, 0, 0));
    }

    /// Pluto blocks with a strong tone (whose image, 35 dB down, lands at the mirror about each LO) and a
    /// tone exactly on one LO (removed with the block mean and the DC bridge). One look shows the image and
    /// loses the tone on the LO; two looks show the tone and not the image.
    #[test]
    fn two_looks_hide_images_and_fill_the_lo() {
        fn run(plan: &Plan) -> Live {
            let eng = Engine::new(plan.clone(), DspSettings { nfft: 2048, avg_tau: 0.0, avg_power: false, peak: PeakMode::Hold, eq_on: false, wf_rate: 20.0, smooth_tau: 0.0, burst_db: 8.0 }, false);
            let sink = eng.sink();
            let fs = plan.rate.adc_hz();
            let n = crate::dsp::n_samples(plan.rate);
            let mut rng = 0x9E37_79B9_7F4A_7C15u64;
            let mut gauss = move || {
                let mut u = || {
                    rng ^= rng << 13;
                    rng ^= rng >> 7;
                    rng ^= rng << 17;
                    (rng >> 11) as f64 / (1u64 << 53) as f64
                };
                (-2.0 * u().max(1e-12).ln()).sqrt() * (std::f64::consts::TAU * u()).cos()
            };
            let (strong, on_lo) = (951.3e6, 960e6);
            let img = 10f64.powf(-35.0 / 20.0);
            let mut cnt = 0u32;
            let mut seq = plan.sequence();
            for sweep in 1..=3u32 {
                let mut buf = vec![0u16; BLOCK_WORDS * plan.n()];
                for (w, &k) in buf.chunks_exact_mut(BLOCK_WORDS).zip(&seq) {
                    let lo = plan.centre(k as usize);
                    for t in 0..n {
                        let ph = |f: f64| std::f64::consts::TAU * (f - lo) * t as f64 / fs;
                        // the strong tone, its image (the conjugate, at the mirror) and the tone on 960 MHz
                        let (a, b) = (ph(strong), ph(on_lo) + 0.3);
                        let i = 1000.0 * a.cos() + 1000.0 * img * (a + 0.7).cos() + 100.0 * b.cos() + 2.0 * gauss();
                        let q = 1000.0 * a.sin() - 1000.0 * img * (a + 0.7).sin() + 100.0 * b.sin() + 2.0 * gauss();
                        w[TAG_WORDS + 2 * t] = i.round() as i16 as u16;
                        w[TAG_WORDS + 2 * t + 1] = q.round() as i16 as u16;
                    }
                    cnt += 3;
                    tag_block(w, &Tag { step: k, block: 0, sweep, freq_hz: lo as u32, dma_count: cnt, lock_polls: 0, lock_timed_out: false });
                }
                sink.push(buf);
                seq.reverse();
            }
            std::thread::sleep(Duration::from_millis(500));
            let l = eng.live.lock();
            assert_eq!(l.stats.steps, 3 * plan.n() as u64);
            Live { cur: l.cur.clone(), plan: l.plan.clone(), m: l.m, offs: l.offs.clone(), eq: l.eq.clone(), ..Live::new(l.plan.clone(), l.settings) }
        }
        let near = |l: &Live, f: f64| (l.index_of(f - 60e3)..l.index_of(f + 60e3)).map(|i| l.value(Trace::Live, i)).fold(f32::NEG_INFINITY, f32::max);
        let one = Plan::new(900.0, 1000.0, 40.0, 1, 0, 0, Order::VcoContinuous, Rate::P61);
        let two = one.clone().overlapped();
        assert!(two.overlap && two.n() == 6 && two.step_hz == 20_000_000, "{two:?}");
        let (l1, l2) = (run(&one), run(&two));
        // the strong tone reads -6 dBFS either way; its image about the 960 MHz LO is at 968.7 MHz
        for l in [&l1, &l2] {
            assert!((near(l, 951.3e6) + 6.2).abs() < 2.0, "strong tone {}", near(l, 951.3e6));
        }
        let floor = near(&l2, 985e6);
        assert!(floor < -70.0, "floor {floor}");
        assert!(near(&l1, 968.7e6) > -50.0, "one look shows the image ({})", near(&l1, 968.7e6));
        assert!(near(&l2, 968.7e6) < floor + 8.0, "two looks hide it ({} over a floor of {floor})", near(&l2, 968.7e6));
        assert!(near(&l2, 928.7e6) < floor + 8.0, "and the one about 940 MHz ({})", near(&l2, 928.7e6));
        // the tone on the LO (-26 dBFS): lost with one look, seen by the next step with two
        assert!(near(&l1, 960e6) < -50.0, "one look loses the tone on the LO ({})", near(&l1, 960e6));
        assert!(near(&l2, 960e6) > -32.0, "two looks see it ({})", near(&l2, 960e6));
    }

    /// Smoothing: the noise's wander over time drops by more than half, a 15 dB burst on one pass (20 dB
    /// over the rough floor) shows
    /// at its level and is gone (within 1 dB) a second later, and noise alone rarely passes as a burst,
    /// whether it is steady (k = 8 looks, two looks at 30 kHz) or rough (k = 1, one look at the finest bins).
    #[test]
    fn smoothing_steadies_noise_and_passes_bursts() {
        for (k, burst_db) in [(8u32, 15.0), (1, 20.0)] {
            let plan = Plan::new(400.0, 440.0, 8.0, 1, 0, 2, Order::Ascending, Rate::M10);
            let s = DspSettings { nfft: 2048, avg_tau: 0.0, avg_power: false, peak: PeakMode::Hold, eq_on: false, wf_rate: 20.0, smooth_tau: 1.0, burst_db: 8.0 };
            let mut l = Live::new(plan, s);
            let m = l.m;
            let mut rng = 0x2545_F491_4F6C_DD1Du64;
            let mut u = move || {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                ((rng >> 11) as f64 / (1u64 << 53) as f64).max(1e-12)
            };
            let (dt, passes, burst_at) = (0.1f32, 400, 300);
            let (mut raw_t, mut cur_t) = (vec![Vec::new(); m], vec![Vec::new(); m]);
            let mut passed = 0usize;
            for pass in 0..passes {
                let vals: Vec<f32> = (0..m)
                    .map(|j| {
                        let x: f64 = (0..k).map(|_| -u().ln()).sum::<f64>() / k as f64;
                        let burst = if pass == burst_at && (100..110).contains(&j) { 10f64.powf(burst_db / 10.0) } else { 0.0 };
                        (10.0 * (x + burst).log10()) as f32 - 90.0
                    })
                    .collect();
                update_bins(&mut l, s, 0, &vals, &[], if pass == 0 { None } else { Some(dt) });
                if pass == burst_at {
                    for j in 100..110 {
                        assert_eq!(l.cur[j], vals[j], "k={k}: the burst shows at its level (threshold {:.1}, spread {:.2})", l.burst_db, l.noise_sd);
                    }
                }
                if pass == burst_at + 10 {
                    for j in 100..110 {
                        // back to its floor, give or take the floor's own wander (larger on a rough floor)
                        assert!(l.cur[j] < l.floor[j] + 0.5 + 0.25 * l.noise_sd, "k={k}: a second later it is back to its floor ({} against {})", l.cur[j], l.floor[j]);
                    }
                }
                if pass >= 100 && pass < burst_at {
                    for j in 0..m {
                        raw_t[j].push(vals[j]);
                        cur_t[j].push(l.cur[j]);
                        passed += (l.cur[j] == vals[j]) as usize;
                    }
                }
            }
            let sd = |v: &Vec<f32>| {
                let mean = v.iter().sum::<f32>() / v.len() as f32;
                (v.iter().map(|x| (x - mean).powi(2)).sum::<f32>() / v.len() as f32).sqrt()
            };
            let (r, c) = ((0..m).map(|j| sd(&raw_t[j])).sum::<f32>() / m as f32, (0..m).map(|j| sd(&cur_t[j])).sum::<f32>() / m as f32);
            let rate = passed as f64 / (m * 200) as f64;
        assert!(c < 0.5 * r, "k={k}: wander {r:.2} dB raw, {c:.2} smoothed");
            assert!(rate < 2e-4, "k={k}: noise passed as a burst {rate:.1e} of the time (threshold {:.1} dB)", l.burst_db);
        }
    }
}
