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
use crate::hydra::{BLOCK_WORDS, Block, Sink, parse_block};
use crate::plan::{Order, Plan, Rate};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DspSettings {
    pub nfft: usize,
    /// Video averaging weight per pass (0 = off).
    pub avg_alpha: f32,
    pub eq_on: bool,
    /// Waterfall rows per second at most (fast narrow plans merge passes by max).
    pub wf_rate: f32,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EqState {
    Off,
    /// No correction known for this resolution yet.
    Unknown,
    Learning(u32),
    Learned(u32),
}

#[derive(Clone, Debug, Default)]
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

pub struct Live {
    pub generation: u64,
    pub restarts: u64,
    pub plan: Plan,
    pub settings: DspSettings,
    /// Bins per step and their offsets from the step centre.
    pub m: usize,
    pub offs: Vec<f64>,
    grid_a: usize,
    pub cur: Vec<f32>,
    pub avg: Vec<f32>,
    pub peak: Vec<f32>,
    /// Per-bin correction (dB) subtracted from every step; zeros when off or unknown.
    pub eq: Vec<f32>,
    pub eq_state: EqState,
    pub step_t: Vec<f32>,
    pub head: Option<usize>,
    pub head_t: f32,
    pub rows: VecDeque<Vec<f32>>,
    pub wf_cols: usize,
    pub stats: Stats,
    clear_peak: bool,
    reset_eq: bool,
    /// Load and save the learned passband in the cache: on for the radio, off for the simulator and tests.
    persist_eq: bool,
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
            grid_a: 0,
            cur: Vec::new(),
            avg: Vec::new(),
            peak: Vec::new(),
            eq: Vec::new(),
            eq_state: EqState::Unknown,
            step_t: Vec::new(),
            head: None,
            head_t: f32::NEG_INFINITY,
            rows: VecDeque::new(),
            wf_cols: 0,
            stats: Stats::default(),
            clear_peak: false,
            reset_eq: false,
            persist_eq: false,
        };
        l.relayout();
        l
    }

    fn relayout(&mut self) {
        let grid: Vec<f64> = dsp::grid(self.settings.nfft, self.plan.rate).iter().map(|g| g.0).collect();
        let r = dsp::window(&grid, self.plan.window_hz);
        self.grid_a = r.start;
        self.m = r.len();
        self.offs = grid[r].to_vec();
        let len = self.m * self.plan.n();
        self.cur = vec![f32::NAN; len];
        self.avg = vec![f32::NAN; len];
        self.peak = vec![f32::NAN; len];
        self.eq = vec![0.0; self.m];
        self.step_t = vec![f32::NEG_INFINITY; self.plan.n()];
        self.head = None;
        self.rows.clear();
        self.wf_cols = len.min(WF_COLS_MAX);
        self.generation += 1;
    }

    pub fn freq(&self, idx: usize) -> f64 {
        self.plan.centre(idx / self.m) + self.offs[idx % self.m]
    }

    /// First index whose frequency is >= f.
    pub fn index_of(&self, f: f64) -> usize {
        let p = &self.plan;
        let lo = p.f0 as f64 - p.window_hz / 2.0;
        let s = ((f - lo) / p.step_hz as f64).floor();
        if s < 0.0 {
            return 0;
        }
        let s = s as usize;
        if s >= p.n() {
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
        let mut out = String::from("freq_hz,dbfs,peak_dbfs\n");
        for i in self.index_of(self.plan.start_hz)..self.index_of(self.plan.stop_hz) {
            let v = self.value(Trace::Live, i);
            if v.is_finite() {
                out.push_str(&format!("{:.0},{:.2},{:.2}\n", self.freq(i), v, self.value(Trace::Peak, i)));
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
        Sink { tx: self.tx.clone(), host_drops: self.host_drops.clone() }
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
    generation: u64,
    restarts: u64,
    plan: Plan,
    settings: DspSettings,
    range: std::ops::Range<usize>,
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
    scratch: Vec<f32>,
    done: Vec<Done>,
    order: OrderModel,
    last_key: Option<(u32, u16)>,
    last_sweep: Option<u32>,
    /// Learned passband shape per (rate, FFT size), on that grid, with the passes behind it.
    eq_store: HashMap<(Rate, usize), (Vec<f32>, u32)>,
    persist_eq: bool,
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
        let (plan, settings, persist_eq) = {
            let l = live.lock();
            (l.plan.clone(), l.settings, l.persist_eq)
        };
        let eq_store = if persist_eq { load_eq_store() } else { HashMap::new() };
        Worker {
            dsp: Dsp::new(settings.nfft, plan.rate),
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
            acc: Vec::new(),
            acc_n: 0,
            acc_key: None,
            acc_done: false,
            settling: true,
            settle_dropped: 0,
            acc_lock: 0,
            acc_dma: 0,
            scratch: Vec::new(),
            done: Vec::new(),
            last_key: None,
            last_sweep: None,
            eq_store,
            persist_eq,
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
            self.reset_sequence(true);
        }
        if l.persist_eq != self.persist_eq {
            if self.persist_eq {
                self.save_eq();
            }
            self.persist_eq = l.persist_eq;
            self.eq_store = if self.persist_eq { load_eq_store() } else { HashMap::new() };
            self.publish_eq(&mut l);
        }
        if l.reset_eq {
            l.reset_eq = false;
            self.eq_store.remove(&(l.plan.rate, l.settings.nfft));
            if self.persist_eq {
                let _ = std::fs::remove_file(eq_path(l.plan.rate, l.settings.nfft));
            }
            self.publish_eq(&mut l);
        }
        if l.clear_peak {
            l.clear_peak = false;
            let cur = l.cur.clone();
            l.peak = cur;
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
            self.range = l.grid_a..l.grid_a + l.m;
            self.acc = vec![0.0; l.m];
            self.scratch = vec![0.0; l.m];
            self.order = OrderModel::new(&self.plan);
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
            let want = p.f0.wrapping_add(p.step_hz.wrapping_mul(t.step as u32));
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
                self.acc_lock = t.lock_polls;
                self.acc_dma = t.dma_count;
            }
            if self.acc_done {
                continue;
            }
            self.dsp.power(w, self.range.clone(), &mut self.scratch);
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
        let a = self.settings.avg_alpha;
        let clear = l.stats.steps == 0;
        for j in 0..m {
            let v = d.db[j];
            l.cur[i0 + j] = v;
            let av = &mut l.avg[i0 + j];
            *av = if av.is_nan() || a <= 0.0 { v } else { *av + a * (v - *av) };
            let pk = &mut l.peak[i0 + j];
            if pk.is_nan() || v > *pk || clear {
                *pk = v;
            }
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
            return Some((l.cur.clone(), l.m));
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
        let mut l = self.live.lock();
        if l.generation == self.generation {
            self.publish_eq(&mut l);
        }
        let now = self.now();
        if now - self.eq_saved_t > 30.0 {
            self.eq_saved_t = now;
            self.save_eq();
        }
    }

    fn publish_eq(&self, l: &mut Live) {
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
            return;
        }
        let eq = &entry.unwrap().0[self.range.clone()];
        // Keep the overall level: remove the correction's own median.
        let mut tmp = eq.to_vec();
        let med = median(&mut tmp);
        l.eq = eq.iter().map(|v| v - med).collect();
    }

    fn save_eq(&self) {
        if !self.persist_eq {
            return;
        }
        for ((rate, nfft), (eq, passes)) in &self.eq_store {
            if *passes > 0 && eq.iter().any(|v| !v.is_nan()) {
                let path = eq_path(*rate, *nfft);
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
        let mut l = self.live.lock();
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

fn eq_path(rate: Rate, nfft: usize) -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("."));
    // 10 MSPS keeps the name it had before 12 MSPS existed
    let name = match rate {
        Rate::M10 => format!("passband-eq-{nfft}.f32"),
        Rate::M12 => format!("passband-eq-12msps-{nfft}.f32"),
    };
    base.join("corvus-sweep").join(name)
}

/// Every learned passband in the cache, counted as 10 passes so new data can still move it.
fn load_eq_store() -> HashMap<(Rate, usize), (Vec<f32>, u32)> {
    let mut store = HashMap::new();
    for rate in [Rate::M10, Rate::M12] {
        for nfft in dsp::NFFTS {
            if let Some(v) = load_eq(rate, nfft) {
                store.insert((rate, nfft), (v, 10));
            }
        }
    }
    store
}

fn load_eq(rate: Rate, nfft: usize) -> Option<Vec<f32>> {
    let bytes = std::fs::read(eq_path(rate, nfft)).ok()?;
    let v: Vec<f32> = bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
    (v.len() == dsp::grid(nfft, rate).len()).then_some(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hydra::{Tag, tag_block};

    #[test]
    fn index_and_freq_agree() {
        for rate in [Rate::M10, Rate::M12] {
            let plan = Plan::new(24.0, 1800.0, rate.default_step_mhz(), 1, 0, 2, Order::VcoContinuous, rate);
            let l = Live::new(plan, DspSettings { nfft: 8192, avg_alpha: 0.0, eq_on: false, wf_rate: 30.0 });
            for f in [24e6, 100e6, 433.92e6, 1799.9e6] {
                let i = l.index_of(f);
                assert!(l.freq(i) >= f && l.freq(i) - f < 3e3, "{rate:?} {f} -> {}", l.freq(i));
            }
        }
    }

    #[test]
    fn worker_stitches_steps_and_counts_passes() {
        let plan = Plan::new(400.0, 440.0, 8.0, 1, 0, 2, Order::Ascending, Rate::M10);
        let eng = Engine::new(plan.clone(), DspSettings { nfft: 2048, avg_alpha: 0.0, eq_on: false, wf_rate: 1000.0 }, false);
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
        let eng = Engine::new(fastest.clone(), DspSettings { nfft: 2048, avg_alpha: 0.0, eq_on: false, wf_rate: 20.0 }, false);
        let sim = Sim::start(eng.sink(), fastest.clone(), RxSettings { port: 0, gain: Gain::Linearity(10) });
        std::thread::sleep(Duration::from_millis(500));
        for p in [&quality, &fastest, &bursts, &fastest, &quality, &bursts] {
            eng.set_plan(p.clone());
            sim.sweep(p);
            std::thread::sleep(Duration::from_millis(400));
        }
        // a new bin width re-lays out the engine mid-pass, without restarting the radio
        for nfft in [1024, 4096, 2048, 512] {
            eng.set_settings(DspSettings { nfft, avg_alpha: 0.0, eq_on: false, wf_rate: 20.0 });
            std::thread::sleep(Duration::from_millis(330));
        }
        let l = eng.live.lock();
        assert!(l.stats.steps > 500, "the sweep kept running ({} steps)", l.stats.steps);
        assert_eq!((l.stats.gaps, l.stats.misfiled, l.stats.torn), (0, 0, 0));
    }
}
