//! The window: header, speed HUD, controls, and the stitched spectrum over its waterfall.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Align2, Color32, Key, Mesh, Modifiers, Painter, PointerButton, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::dsp::{self, NFFTS};
use crate::engine::{Detector, DspSettings, Engine, EqState, PeakMode, Stats, Trace};
use crate::hydra::{self, Device, Gain, RxSettings};
use crate::plan::{self, Order, PRESETS, Plan, Rate};
use crate::sim::Sim;
use crate::theme::{self, ACCENT, AVERAGE, BG, CAUTION, CHIP, Density, EDGE, GRATICULE, HOUSE, INK, INK2, INK3, LINE, Level, PANEL, PEAK, Palette, SCALE_INK, Slide, TOUCH, TRACE, WELL};
use crate::waterfall::{ROWS, Waterfall};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub start_mhz: f64,
    pub stop_mhz: f64,
    pub step_mhz: f64,
    pub rate: Rate,
    /// The live view: the board parked on one frequency, one step `live_span_mhz` wide (see `set_live`).
    pub live: bool,
    pub live_span_mhz: f64,
    /// The sweep settings to go back to from the live view: start, stop, step, rate.
    pub live_back: Option<(f64, f64, f64, Rate)>,
    pub order: Order,
    pub dwell: u8,
    pub lock_reads: u8,
    pub skip: u8,
    pub port: u8,
    pub gain: Gain,
    pub nfft: usize,
    pub detector: Detector,
    pub show_avg: bool,
    /// Averaging time (s) and kind (linear power or dB).
    pub avg_secs: f32,
    pub avg_power: bool,
    /// The peak trace: hold, decay (fall in dB/s) or a running percentile (percent, window in s).
    pub peak_kind: PeakKind,
    pub peak_fall: f32,
    pub peak_pct: u8,
    pub peak_win: f32,
    pub show_peak: bool,
    pub eq_on: bool,
    pub beam: bool,
    pub vco_marks: bool,
    pub labels: usize,
    pub spec_top: f32,
    pub spec_bot: f32,
    pub wf_lo: f32,
    pub wf_hi: f32,
    pub palette: Palette,
    pub wf_rows: usize,
    pub wf_rate: f32,
    /// Redraw cap in frames per second (rule P2); the data stays complete.
    pub fps: u32,
    pub density: Density,
    /// In-app text scale (rule T4).
    pub text_scale: f32,
    /// Graticule and trace brightness, in percent (rule D9).
    pub grid_dim: f32,
    pub data_dim: f32,
    /// Share of the plot height the spectrum takes; the waterfall gets the rest (drag the separator).
    pub split: f32,
    /// The hover readout follows the cursor (else it sits in the plot's corner).
    pub readout_follow: bool,
    /// The overview strip above the spectrum: the whole sweep, with the zoomed view as a box.
    pub overview: bool,
    /// The waterfall pane shows the 3D landscape instead of the flat waterfall.
    pub wf3d: bool,
    pub cam3d: crate::view3d::Camera,
    /// 3D smoothing: 0 off, 1 light, 2 strong (a Gaussian over the heights, see `view3d::smooth`).
    pub smooth3d: u8,
    /// Gesture hints over the plot (each retires once its gesture has been used; see `Hint`).
    pub hints: bool,
    pub hints_learned: u8,
}

/// What the peak trace keeps (see `engine::PeakMode`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PeakKind {
    Hold,
    Decay,
    Percentile,
}

/// Gestures with a hint that goes away once used (bits of `Config::hints_learned`).
struct Hint;
impl Hint {
    const SELECT: u8 = 1;
    const PAN: u8 = 2;
    const ZOOM: u8 = 4;
    const ORBIT: u8 = 8;
}

impl Default for Config {
    fn default() -> Self {
        Config {
            start_mhz: 24.0,
            stop_mhz: 1800.0,
            step_mhz: Rate::default().default_step_mhz(),
            rate: Rate::default(),
            live: false,
            live_span_mhz: 25.0,
            live_back: None,
            order: Order::VcoContinuous,
            dwell: 1,
            lock_reads: 2,
            skip: 0,
            port: 0,
            gain: Gain::Linearity(10),
            nfft: 2048,
            detector: Detector::Peak,
            show_avg: false,
            avg_secs: 10.0,
            avg_power: false,
            peak_kind: PeakKind::Hold,
            peak_fall: 6.0,
            peak_pct: 90,
            peak_win: 10.0,
            show_peak: true,
            eq_on: true,
            beam: true,
            vco_marks: true,
            labels: 4,
            spec_top: -20.0,
            spec_bot: -120.0,
            wf_lo: -105.0,
            wf_hi: -55.0,
            palette: Palette::Inferno,
            wf_rows: 160,
            wf_rate: 20.0,
            fps: 60,
            density: Density::Comfortable,
            text_scale: 1.0,
            grid_dim: 70.0,
            data_dim: 100.0,
            split: 0.48,
            readout_follow: true,
            overview: true,
            wf3d: false,
            cam3d: crate::view3d::Camera::default(),
            smooth3d: 1,
            hints: true,
            hints_learned: 0,
        }
    }
}

impl Config {
    /// A gesture hint still worth showing.
    fn hint(&self, h: u8) -> bool {
        self.hints && self.hints_learned & h == 0
    }

    fn learned(&mut self, h: u8) {
        self.hints_learned |= h;
    }

    fn plan(&self) -> Plan {
        Plan::new(self.start_mhz, self.stop_mhz, self.step_mhz, self.dwell, self.skip, self.lock_reads, self.order, self.rate)
    }

    /// Change the sample rate, moving the step to the new rate's default if it was on the old
    /// default or isn't offered at the new rate.
    pub fn set_rate(&mut self, rate: Rate) {
        let rate = board_rate(rate);
        let old = self.rate;
        self.rate = rate;
        let offered = rate.steps().iter().any(|&(v, _)| (v - self.step_mhz).abs() < 1e-9);
        if (self.step_mhz - old.default_step_mhz()).abs() < 1e-9 || !offered {
            self.step_mhz = rate.default_step_mhz();
        }
    }

    /// Go live (or move the live view): one step `live_span_mhz` wide centred on `centre_mhz`, at the lowest
    /// sample rate that covers the span, so its bins are the finest. The sweep settings are kept to go back to.
    pub fn set_live(&mut self, centre_mhz: f64) {
        if !self.live {
            self.live_back = Some((self.start_mhz, self.stop_mhz, self.step_mhz, self.rate));
            self.live = true;
        }
        let span = self.live_span_mhz;
        let rate = live_rate(span);
        self.rate = rate;
        // a step at least as wide as the span: the plan is one step
        self.step_mhz = rate.steps().last().map_or(span, |s| s.0).max(span);
        let (cmin, cmax) = rate.centre_range(span * 1e6);
        let c = (centre_mhz * 1e6).clamp(cmin, cmax) / 1e6;
        self.start_mhz = ((c - span / 2.0) * 1000.0).round() / 1000.0;
        self.stop_mhz = ((c + span / 2.0) * 1000.0).round() / 1000.0;
    }

    /// Back from the live view to the sweep it came from.
    pub fn leave_live(&mut self) {
        if let Some((a, b, s, r)) = self.live_back.take() {
            (self.start_mhz, self.stop_mhz, self.step_mhz, self.rate) = (a, b, s, r);
        }
        self.live = false;
    }

    pub fn live_centre_mhz(&self) -> f64 {
        (self.start_mhz + self.stop_mhz) / 2.0
    }

    /// Fit saved or profile settings to the board: its rates (the Airspys have one each) and ports.
    pub fn fit_board(&mut self) {
        if self.live && !crate::hydra::board().has_live() {
            self.leave_live();
        }
        if self.rate != board_rate(self.rate) {
            self.set_rate(board_rate(self.rate));
        }
        if !crate::hydra::board().ports().iter().any(|&(p, _)| p == self.port) {
            self.port = 0;
        }
    }

    /// After a typed start or stop crosses the other end, keep the old span instead of letting the
    /// plan collapse the range to its 200 kHz minimum.
    fn keep_span(&mut self, start0: f64, stop0: f64) {
        let span = (stop0 - start0).max(0.2);
        if self.start_mhz != start0 && self.start_mhz > self.stop_mhz - 0.2 {
            self.stop_mhz = (self.start_mhz + span).min(self.rate.f_range().1 / 1e6);
        } else if self.stop_mhz != stop0 && self.stop_mhz < self.start_mhz + 0.2 {
            self.start_mhz = (self.stop_mhz - span).max(self.rate.f_range().0 / 1e6);
        }
    }

    /// Next or previous step size offered at this rate.
    fn bump_step(&mut self, dir: i32) {
        let steps = self.rate.steps();
        let i = steps.iter().position(|&(v, _)| (v - self.step_mhz).abs() < 1e-9).unwrap_or(steps.len() - 1);
        self.step_mhz = steps[(i as i32 + dir).clamp(0, steps.len() as i32 - 1) as usize].0;
    }

    /// The scan profile these settings match, if any.
    pub fn profile(&self) -> Option<usize> {
        PROFILES.iter().position(|p| p.matches(self))
    }

    fn rx(&self) -> RxSettings {
        RxSettings { port: self.port, gain: self.gain }
    }

    fn dsp(&self) -> DspSettings {
        let peak = match self.peak_kind {
            PeakKind::Hold => PeakMode::Hold,
            PeakKind::Decay => PeakMode::Decay(self.peak_fall),
            PeakKind::Percentile => PeakMode::Percentile { p: self.peak_pct as f32 / 100.0, tau: self.peak_win },
        };
        DspSettings { nfft: self.nfft, avg_tau: if self.show_avg { self.avg_secs } else { 0.0 }, avg_power: self.avg_power, peak, eq_on: self.eq_on, wf_rate: self.wf_rate }
    }

    /// The peak trace's name as the Traces row shows it.
    fn peak_name(&self) -> String {
        match self.peak_kind {
            PeakKind::Hold => "Peak".into(),
            PeakKind::Decay => "Peak ↓".into(),
            PeakKind::Percentile => format!("P{}", self.peak_pct),
        }
    }
}

/// A scan profile: the sweep engine settings for one job, plus the display settings that job needs.
/// Range, receiver and the rest of the display are left alone.
pub struct Profile {
    pub name: &'static str,
    pub key: &'static str,
    pub hint: &'static str,
    rate: Rate,
    step_mhz: f64,
    dwell: u8,
    lock_reads: u8,
    skip: u8,
    nfft: Option<usize>,
    peak_hold: bool,
}

/// A step costs 2 blocks (retune and lock, then the block in flight; firmware `sweep_poll`) plus
/// settle + dwell. Pass rates in the hints are for 24–1800 MHz.
pub const PROFILES: &[Profile] = &[
    Profile {
        name: "Fastest",
        key: "F2",
        hint: "As many passes as the radio can make with every step lock-checked: 12 MSPS, 10 MHz steps, one block per step. About 5 full passes a second.\nTurning the lock check off is faster still, but some steps then read up to 22 dB low.",
        rate: Rate::M12,
        step_mhz: 10.0,
        dwell: 1,
        lock_reads: 2,
        skip: 0,
        nfft: None,
        peak_hold: false,
    },
    Profile {
        name: "Quality",
        key: "F3",
        hint: "Flat, steady levels: 8 MHz steps use only the flattest middle of each step, clear of the mirror at the edges. Each step averages 4 blocks after an extra block of settling, and lock needs 3 good reads. About 2 passes a second.",
        rate: Rate::M12,
        step_mhz: 8.0,
        dwell: 4,
        lock_reads: 3,
        skip: 1,
        nfft: None,
        peak_hold: false,
    },
    Profile {
        name: "Weak",
        key: "F4",
        hint: "Weak narrow signals: the finest bins (2.9 kHz) put less noise in each bin, and 8 blocks per step smooth the floor so a faint carrier stands out. About 1.3 passes a second.",
        rate: Rate::M12,
        step_mhz: 8.0,
        dwell: 8,
        lock_reads: 2,
        skip: 0,
        nfft: Some(8192),
        peak_hold: false,
    },
    Profile {
        name: "Bursts",
        key: "F5",
        hint: "Short or intermittent signals (LTE, ISM remotes, trunked radio): 4 blocks per step keep the receiver listening about two thirds of the time instead of a third, and peak hold (cleared) keeps anything that keys up. A burst in one of the 4 blocks reads about 6 dB low, as blocks are averaged. About 2.7 passes a second.",
        rate: Rate::M12,
        step_mhz: 10.0,
        dwell: 4,
        lock_reads: 2,
        skip: 0,
        nfft: None,
        peak_hold: true,
    },
];

impl Profile {
    pub fn by_name(name: &str) -> Option<&'static Profile> {
        PROFILES.iter().find(|p| p.name.eq_ignore_ascii_case(name))
    }

    /// The profile's rate and step on this board: on the R2 (10 MSPS only) a 10 MHz step becomes 8 MHz,
    /// the widest step that stays flat at 10 MSPS; on the Mini (6 MSPS) steps become its widest.
    fn target(&self) -> (Rate, f64) {
        fit_rate_step(self.rate, self.step_mhz, crate::hydra::board().rates())
    }

    pub fn apply(&self, c: &mut Config) {
        (c.rate, c.step_mhz) = self.target();
        c.order = Order::VcoContinuous;
        c.dwell = self.dwell;
        c.lock_reads = self.lock_reads;
        c.skip = self.skip;
        if let Some(n) = self.nfft {
            c.nfft = n;
        }
        if self.peak_hold {
            c.show_peak = true;
            c.peak_kind = PeakKind::Hold;
            c.detector = Detector::Peak;
        }
    }

    fn matches(&self, c: &Config) -> bool {
        let (rate, step_mhz) = self.target();
        c.rate == rate
            && (c.step_mhz - step_mhz).abs() < 1e-9
            && c.order == Order::VcoContinuous
            && c.dwell == self.dwell
            && c.lock_reads == self.lock_reads
            && c.skip == self.skip
            && self.nfft.is_none_or(|n| c.nfft == n)
            && (!self.peak_hold || (c.show_peak && c.peak_kind == PeakKind::Hold && c.detector == Detector::Peak))
    }
}

/// The settings a profile changes, to go back to.
#[derive(Clone, Copy)]
struct EngineSnap {
    rate: Rate,
    step_mhz: f64,
    order: Order,
    dwell: u8,
    lock_reads: u8,
    skip: u8,
    nfft: usize,
    show_peak: bool,
    peak_kind: PeakKind,
    detector: Detector,
}

impl EngineSnap {
    fn of(c: &Config) -> EngineSnap {
        EngineSnap { rate: c.rate, step_mhz: c.step_mhz, order: c.order, dwell: c.dwell, lock_reads: c.lock_reads, skip: c.skip, nfft: c.nfft, show_peak: c.show_peak, peak_kind: c.peak_kind, detector: c.detector }
    }

    fn restore(&self, c: &mut Config) {
        (c.rate, c.step_mhz, c.order, c.dwell, c.lock_reads, c.skip, c.nfft, c.show_peak, c.peak_kind, c.detector) =
            (self.rate, self.step_mhz, self.order, self.dwell, self.lock_reads, self.skip, self.nfft, self.show_peak, self.peak_kind, self.detector);
    }
}

pub struct Options {
    pub sim: bool,
    pub lib: PathBuf,
    pub serial: u64,
    pub screenshot: Option<(PathBuf, f32)>,
    pub overrides: Vec<Box<dyn Fn(&mut Config)>>,
}

/// What the cached 3D heights were built from: waterfall stamp, view (×1e5), grid size, floor (×10),
/// smoothing.
type V3Key = ((usize, usize, u64, usize), i32, i32, usize, usize, i32, u8);

enum Src {
    Device(Device),
    Sim(Sim),
    Offline(String),
}

impl Src {
    fn sweep(&mut self, p: &Plan) -> Result<(), String> {
        match self {
            Src::Device(d) => d.sweep(p),
            Src::Sim(s) => {
                s.sweep(p);
                Ok(())
            }
            Src::Offline(_) => Ok(()),
        }
    }
    fn set_rx(&mut self, rx: RxSettings) -> Result<(), String> {
        match self {
            Src::Device(d) => d.set_rx(rx),
            Src::Sim(s) => {
                s.set_rx(rx);
                Ok(())
            }
            Src::Offline(_) => Ok(()),
        }
    }
    fn pause(&mut self) {
        match self {
            Src::Device(d) => d.pause(),
            Src::Sim(s) => s.pause(),
            Src::Offline(_) => {}
        }
    }
    fn resume(&mut self) -> Result<(), String> {
        match self {
            Src::Device(d) => d.resume(),
            Src::Sim(s) => {
                s.resume();
                Ok(())
            }
            Src::Offline(_) => Ok(()),
        }
    }
}

/// What a primary drag on the plot does when it's let go.
#[derive(Clone, Copy, PartialEq)]
enum DragMode {
    /// Offer Zoom or Sweep.
    Ask,
    /// Shift: sweep the span straight away.
    Sweep,
    /// Ctrl: zoom to it straight away.
    Zoom,
}

/// Which part of the overview's view box a drag holds.
#[derive(Clone, Copy, PartialEq)]
enum OvGrab {
    Move,
    Left,
    Right,
}

#[derive(Default)]
struct Shown {
    live: (Vec<f64>, Vec<f32>),
    avg: Option<(Vec<f64>, Vec<f32>)>,
    peak: Option<(Vec<f64>, Vec<f32>)>,
    hover: Option<(f64, f32)>,
    /// The whole sweep at the overview strip's width.
    overview: Option<(Vec<f64>, Vec<f32>)>,
}

pub struct App {
    engine: Engine,
    src: Src,
    cfg: Config,
    applied: Plan,
    applied_rx: RxSettings,
    plan_dirty: Option<Instant>,
    rx_dirty: Option<Instant>,
    running: bool,
    view: (f64, f64),
    wf: Waterfall,
    stats: Stats,
    eq_state: EqState,
    step_t: Vec<f32>,
    head: Option<usize>,
    labels: Vec<(f64, f32)>,
    labels_t: Instant,
    selection: Option<(f64, f64)>,
    hover_x: Option<f32>,
    /// Pointer position over the spectrum or waterfall, and which one (true: waterfall).
    hover_pos: Option<(Pos2, bool)>,
    /// A dragged span waiting for Zoom or Sweep, and where the pointer let go.
    pending_sel: Option<(f64, f64, Pos2)>,
    /// The pointer is on (or dragging) the spectrum/waterfall separator.
    sep_hot: bool,
    /// Zoomed all the way out, dragging the axis or the overview box moves the swept range itself: the
    /// offset so far (Hz). The view follows it as a preview; letting go retunes.
    range_drag: Option<f64>,
    /// What the current primary drag does when released.
    drag_mode: DragMode,
    /// Overview strip drag: what's being moved, and the view when it started.
    ov_drag: Option<(OvGrab, (f64, f64))>,
    shown: Shown,
    /// Acknowledges the user's own action for 2.6 s (rule A1).
    toast: Option<(String, Instant)>,
    /// The last command the radio refused; a status-bar condition until it clears.
    error: Option<String>,
    /// The range before the last range change, for "↶ back".
    prev_range: Option<(f64, f64)>,
    /// Sweep settings before the last profile was applied, named for the restore button.
    prev_engine: Option<(EngineSnap, &'static str)>,
    /// Engine times of receiver or processing changes, drawn as retune lines on the waterfall.
    retunes: Vec<f32>,
    /// Engine times of sweep mode changes (plan or bin width), with what changed: hard breaks in the
    /// waterfall, whose history is re-mapped onto the new axis rather than cleared.
    breaks: Vec<(f32, String)>,
    /// The plan and FFT size the waterfall's newest rows were made with.
    wf_mode: Option<(Plan, usize)>,
    /// Integrity problems seen so far, and the engine time the count last grew.
    bad_last: (u64, f32),
    hidden_peaks: Vec<(f64, f32)>,
    frame_ms: f32,
    /// Seconds per full pass, and whether that's slow enough for the eye to follow the sweep head.
    pass_s: f32,
    followable: bool,
    styled: Option<(Density, f32)>,
    auto_pending: bool,
    switches: Vec<f64>,
    lib: PathBuf,
    /// The board and library given at startup (`--lib` only applies to that board).
    start_lib: (crate::hydra::Board, PathBuf),
    /// The 3D pipeline is set up (a wgpu renderer with a depth buffer).
    view3d_ok: bool,
    /// What the 3D view's heights were last built from, and the palette last uploaded.
    v3_key: Option<V3Key>,
    v3_palette: Option<Palette>,
    /// The device picker under the device chip: open, where, and what was found on USB.
    picker: Option<(Pos2, Vec<crate::hydra::Found>)>,
    serial: u64,
    screenshot: Option<(PathBuf, f32)>,
    shot_requested: Option<(PathBuf, bool)>,
    frame_times: std::collections::VecDeque<f32>,
    started: Instant,
    /// G was pressed: put the cursor in the Start field next frame.
    focus_start: bool,
    show_keys: bool,
}

impl App {
    pub fn new(cc: &eframe::CreationContext, opts: Options) -> App {
        theme::install_fonts(&cc.egui_ctx);
        let mut cfg: Config = cc.storage.and_then(|s| eframe::get_value(s, eframe::APP_KEY)).unwrap_or_default();
        for o in &opts.overrides {
            o(&mut cfg);
        }
        cfg.fit_board();
        if !NFFTS.contains(&cfg.nfft) {
            cfg.nfft = 2048;
        }
        // label levels are Full / Focus / Minimal now
        cfg.labels = match cfg.labels {
            0 => 0,
            1..=5 => 4,
            _ => 12,
        };
        if ![60, 30, 20, 15].contains(&cfg.fps) {
            cfg.fps = 60;
        }
        let plan = cfg.plan();
        let engine = Engine::new(plan.clone(), cfg.dsp(), !opts.sim);
        let rx = cfg.rx();
        let src = if opts.sim {
            Src::Sim(Sim::start(engine.sink(), plan.clone(), rx))
        } else {
            open_device(&opts.lib, opts.serial, &engine, &plan, rx)
        };
        let view = (plan.start_hz, plan.stop_hz);
        let mut app = App {
            engine,
            src,
            applied: plan,
            applied_rx: rx,
            plan_dirty: None,
            rx_dirty: None,
            running: true,
            view,
            wf: Waterfall::new(),
            stats: Stats::default(),
            eq_state: EqState::Unknown,
            step_t: Vec::new(),
            head: None,
            labels: Vec::new(),
            labels_t: Instant::now(),
            selection: None,
            hover_x: None,
            hover_pos: None,
            pending_sel: None,
            sep_hot: false,
            range_drag: None,
            drag_mode: DragMode::Ask,
            ov_drag: None,
            shown: Shown::default(),
            toast: None,
            error: None,
            prev_range: None,
            prev_engine: None,
            retunes: Vec::new(),
            breaks: Vec::new(),
            wf_mode: None,
            bad_last: (0, f32::NEG_INFINITY),
            hidden_peaks: Vec::new(),
            frame_ms: 0.0,
            pass_s: f32::INFINITY,
            followable: true,
            styled: None,
            auto_pending: true,
            switches: plan::divider_switches(cfg.rate),
            view3d_ok: cc.wgpu_render_state.as_ref().is_some_and(|rs| crate::view3d::init(rs, eframe::egui_wgpu::depth_format_from_bits(24, 0))),
            v3_key: None,
            v3_palette: None,
            start_lib: (crate::hydra::board(), opts.lib.clone()),
            picker: None,
            lib: opts.lib,
            serial: opts.serial,
            screenshot: opts.screenshot,
            shot_requested: None,
            frame_times: Default::default(),
            started: Instant::now(),
            focus_start: false,
            show_keys: false,
            cfg,
        };
        // the firmware said which Airspy it is (the board was switched while opening): fit and sweep
        if !crate::hydra::board().rates().contains(&app.applied.rate) {
            app.engine.set_eq_prefix(crate::hydra::board().cache_prefix());
            app.cfg.fit_board();
            app.replan();
            if let Src::Device(d) = &mut app.src
                && let Err(e) = d.sweep(&app.applied)
            {
                app.src = Src::Offline(e);
            }
        }
        app
    }

    /// Acknowledge the user's own action for 2.6 s. A failure is a condition instead: it stays in
    /// the status bar until it clears (rule A1).
    fn toast(&mut self, msg: impl Into<String>, err: bool) {
        let msg = msg.into();
        if err {
            log::warn!("{msg}");
            self.error = Some(msg);
        } else {
            self.toast = Some((msg, Instant::now()));
        }
    }

    fn apply_plan(&mut self) {
        let p = self.cfg.plan();
        self.cfg.start_mhz = p.start_hz / 1e6;
        self.cfg.stop_mhz = p.stop_hz / 1e6;
        if p == self.applied {
            return;
        }
        let range_changed = p.start_hz != self.applied.start_hz || p.stop_hz != self.applied.stop_hz;
        if p.rate != self.applied.rate {
            self.switches = plan::divider_switches(p.rate);
        }
        self.engine.set_plan(p.clone());
        match self.src.sweep(&p) {
            Ok(()) => self.error = None,
            Err(e) => self.toast(e, true),
        }
        self.running = true;
        if range_changed {
            self.prev_range = Some((self.applied.start_hz, self.applied.stop_hz));
            self.view = (p.start_hz, p.stop_hz);
            self.labels.clear();
        }
        self.applied = p;
    }

    fn apply_rx(&mut self) {
        let rx = self.cfg.rx();
        if rx == self.applied_rx {
            return;
        }
        self.engine.note_restart();
        match self.src.set_rx(rx) {
            Ok(()) => self.error = None,
            Err(e) => self.toast(e, true),
        }
        self.retunes.push(self.engine.now());
        self.applied_rx = rx;
    }

    fn set_range(&mut self, lo_hz: f64, hi_hz: f64) {
        // a range is a sweep: leave the live view (back to its sweep settings), then take the range
        if self.cfg.live {
            self.cfg.leave_live();
        }
        self.cfg.start_mhz = (lo_hz / 1e6 * 1000.0).round() / 1000.0;
        self.cfg.stop_mhz = (hi_hz / 1e6 * 1000.0).round() / 1000.0;
        self.apply_plan();
    }

    fn apply_profile(&mut self, i: usize) {
        let pr = &PROFILES[i];
        if self.cfg.profile() != Some(i) {
            self.prev_engine = Some((EngineSnap::of(&self.cfg), self.engine_name()));
        }
        pr.apply(&mut self.cfg);
        if self.cfg.live {
            // keep the live view: a profile's rate and step are for sweeping
            let c = self.cfg.live_centre_mhz();
            self.cfg.set_live(c);
        }
        if pr.peak_hold {
            self.engine.clear_peak();
        }
        self.toast(format!("{} profile", pr.name), false);
    }

    /// Undo the last profile (a second press returns to it).
    fn back_engine(&mut self) {
        if let Some((snap, _)) = self.prev_engine {
            let now = (EngineSnap::of(&self.cfg), self.engine_name());
            snap.restore(&mut self.cfg);
            self.prev_engine = Some(now);
        }
    }

    fn engine_name(&self) -> &'static str {
        self.cfg.profile().map_or("previous sweep settings", |i| PROFILES[i].name)
    }

    /// Go back to the range before the last change (a second press returns).
    fn back_range(&mut self) {
        if let Some((a, b)) = self.prev_range {
            self.set_range(a, b);
        }
    }

    fn toggle_run(&mut self) {
        if self.running {
            self.src.pause();
            self.running = false;
        } else {
            self.engine.note_restart();
            match self.src.resume() {
                Ok(()) => self.running = true,
                Err(e) => self.toast(e, true),
            }
        }
    }

    fn auto_levels(&mut self) {
        let ys: Vec<f32> = self.shown.live.1.iter().copied().filter(|v| v.is_finite()).collect();
        if ys.len() < 16 {
            return;
        }
        let mut s = ys.clone();
        s.sort_by(f32::total_cmp);
        let floor = s[s.len() / 10];
        let top = s[s.len() - 1];
        self.cfg.spec_bot = ((floor - 18.0) / 5.0).floor() * 5.0;
        self.cfg.spec_top = (((top + 8.0).max(floor + 45.0)) / 5.0).ceil() * 5.0;
        self.cfg.wf_lo = (floor - 3.0).round();
        self.cfg.wf_hi = (floor + 42.0).min(top).max(floor + 20.0).round();
    }

    fn retry_device(&mut self) {
        self.src = Src::Offline(String::new());
        self.src = open_device(&self.lib, self.serial, &self.engine, &self.applied, self.applied_rx);
        if let Src::Device(_) = self.src {
            self.engine.set_persist_eq(true);
            self.engine.note_restart();
            self.running = true;
            self.toast(format!("{} connected", crate::hydra::board().name()), false);
        }
    }

    /// Close what's open and open another board (or the simulator, with `None`): switch the board, its
    /// learned passband and the settings it allows, then start the sweep. An Airspy turns out to be an
    /// R2 or a Mini only once opened, so the settings are fitted again afterwards.
    fn switch_device(&mut self, target: Option<(crate::hydra::Board, u64)>) {
        self.src = Src::Offline(String::new());
        let Some((board, serial)) = target else {
            self.cfg.fit_board();
            self.replan();
            self.use_sim();
            self.toast("Simulator", false);
            return;
        };
        crate::hydra::set_board(board);
        self.serial = serial;
        self.lib = if board == self.start_lib.0 { self.start_lib.1.clone() } else { crate::hydra::default_lib_path(board) };
        self.engine.set_eq_prefix(board.cache_prefix());
        self.cfg.fit_board();
        self.replan();
        self.src = open_device(&self.lib, self.serial, &self.engine, &self.applied, self.applied_rx);
        let now = crate::hydra::board();
        if now != board {
            // the firmware said which Airspy it is
            self.engine.set_eq_prefix(now.cache_prefix());
            self.cfg.fit_board();
            self.replan();
            if let Src::Device(d) = &mut self.src
                && let Err(e) = d.sweep(&self.applied)
            {
                self.src = Src::Offline(e);
            }
        }
        if let Src::Device(_) = self.src {
            self.engine.set_persist_eq(true);
            self.engine.note_restart();
            self.running = true;
            self.toast(format!("{} connected", now.name()), false);
        } else {
            self.engine.set_persist_eq(false);
        }
    }

    /// Make the plan from the settings and hand it to the engine (not to a device).
    fn replan(&mut self) {
        let p = self.cfg.plan();
        self.cfg.start_mhz = p.start_hz / 1e6;
        self.cfg.stop_mhz = p.stop_hz / 1e6;
        self.switches = plan::divider_switches(p.rate);
        self.engine.set_plan(p.clone());
        self.view = (p.start_hz, p.stop_hz);
        self.labels.clear();
        self.applied = p;
    }

    /// The device picker: boards found on USB and the simulator; the current one is lit.
    fn device_picker(&mut self, ui: &mut Ui) {
        let Some((pos, found)) = self.picker.clone() else { return };
        let mut pick: Option<Option<(crate::hydra::Board, u64)>> = None;
        let mut close = false;
        let area = egui::Area::new(ui.id().with("devpicker")).order(egui::Order::Foreground).fixed_pos(pos).show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(330.0);
                theme::section(ui, "Radios");
                if found.is_empty() {
                    theme::caption(ui, "Nothing found on USB. Plug a board in and refresh.");
                }
                let cur_board = crate::hydra::board();
                let live = matches!(self.src, Src::Device(_));
                for f in &found {
                    let same_family = f.board == cur_board || (f.board == crate::hydra::Board::Airspy && cur_board == crate::hydra::Board::Mini);
                    let current = live && same_family && (f.serial == self.serial || self.serial == 0);
                    let label = if f.board == crate::hydra::Board::Airspy && current { cur_board.name().to_string() } else { f.label.clone() };
                    let id = if f.board.networked() { crate::pluto::addr() } else { format!("{:016X}", f.serial) };
                    let text = RichText::new(format!("{label}   {id}")).font(theme::num(theme::SM));
                    if ui.add(egui::Button::selectable(current, text)).clicked() && !current {
                        pick = Some(Some((f.board, f.serial)));
                    }
                }
                ui.separator();
                let sim = matches!(self.src, Src::Sim(_));
                if ui.add(egui::Button::selectable(sim, format!("Simulator ({} timing)", cur_board.name()))).clicked() && !sim {
                    pick = Some(None);
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Refresh").clicked() {
                        self.picker = Some((pos, crate::hydra::list_devices()));
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
                theme::caption(ui, "Boards on Corvus firmware sweep on the board; stock firmware is swept from the PC, more slowly. A board another program is using won't open. A Pluto is reached over the network (--pluto HOST).");
            });
        });
        if let Some(t) = pick {
            self.picker = None;
            self.switch_device(t);
            return;
        }
        let outside = ui.input(|i| i.pointer.any_pressed()) && !area.response.contains_pointer();
        if close || outside || ui.input(|i| i.key_pressed(Key::Escape)) {
            self.picker = None;
        }
    }

    fn use_sim(&mut self) {
        self.engine.set_persist_eq(false);
        self.src = Src::Sim(Sim::start(self.engine.sink(), self.applied.clone(), self.applied_rx));
        self.engine.note_restart();
        self.running = true;
    }

    /// Pull what the panels need from the engine (one short lock).
    fn sync_engine(&mut self) {
        let live = self.engine.live.clone();
        let mut l = live.lock();
        let now = self.engine.now();
        if l.generation != self.wf.generation {
            self.wf.relayout(l.wf_cols, l.generation, l.plan.cover());
            let mode = (l.plan.clone(), l.settings.nfft);
            if let Some(old) = self.wf_mode.replace(mode.clone())
                && self.wf.filled > 0
                && let Some(what) = mode_change(&old, &mode, &self.cfg)
            {
                self.breaks.push((now, what));
            }
        }
        while let Some(row) = l.rows.pop_front() {
            self.wf.push(&row, now);
        }
        self.stats = l.stats.clone();
        self.eq_state = l.eq_state;
        self.step_t.clear();
        self.step_t.extend_from_slice(&l.step_t);
        self.head = l.head;
    }

    /// The top bar (rules/signature.md §3): wordmark, product, then the actions on the right.
    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal_centered(|ui| {
            ui.label(RichText::new("AUGGIESOFT").font(theme::label_font(15.0)).extra_letter_spacing(15.0 * 0.14).color(HOUSE));
            ui.add_space(8.0);
            ui.label(RichText::new("Corvus Sweep").size(theme::MD).color(INK));
            ui.add_space(10.0);
            self.device_chip(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("?").on_hover_text("Keyboard shortcuts (? or F1)").clicked() {
                    self.show_keys = !self.show_keys;
                }
                ui.menu_button("Settings", |ui| self.settings_menu(ui));
                if ui.button("Snapshot").on_hover_text("Save a PNG of the window to captures/ (Ctrl+S)").clicked() {
                    self.snapshot(ui.ctx());
                }
                if ui.button("Export CSV").on_hover_text("Save the stitched spectrum to captures/ (Ctrl+E)").clicked() {
                    self.export_csv();
                }
                if ui.button("Auto levels").on_hover_text("Fit the dB scales to the current spectrum (A)").clicked() {
                    self.auto_levels();
                }
                ui.add_space(6.0);
                let run = egui::Button::new(RichText::new(if self.running { "Pause" } else { "Run" }).size(theme::MD)).min_size(vec2(92.0, 0.0));
                if ui.add(run).on_hover_text("Space").clicked() {
                    self.toggle_run();
                }
            });
        });
    }

    /// A Pluto sweeping in corvus-pluto's fast mode.
    fn pluto_fast(&self) -> bool {
        matches!(&self.src, Src::Device(d) if d.pluto_mode().as_deref() == Some("fast"))
    }

    /// What's connected and how it sweeps: (dot colour, short text, details for the hover).
    fn device_status(&self) -> (Color32, String, String) {
        let board = crate::hydra::board().name();
        match &self.src {
            Src::Sim(_) => (theme::INFO, format!("Simulator · {board} timing"), "A synthetic band plan swept with the firmware's timing. No radio is open.".into()),
            Src::Offline(e) => (theme::ALARM, format!("{board} · not open"), if e.is_empty() { "Not connected.".into() } else { e.clone() }),
            Src::Device(d) => {
                let serial = if self.serial == 0 { "first one found".to_string() } else { format!("{:016X}", self.serial) };
                if let crate::hydra::Device::Pluto(p) = d {
                    let (mode, note) = p.mode.lock().clone();
                    let how = match mode.as_str() {
                        "fast" => "fast sweep (fastlock, direct SPI)".to_string(),
                        "driver" if !note.is_empty() => format!("driver sweep. Fast mode is off: {note}"),
                        "driver" => "driver sweep".to_string(),
                        _ => "corvus-pluto sweep".to_string(),
                    };
                    let short = match mode.as_str() {
                        "fast" => "fast sweep",
                        "driver" => "driver sweep",
                        _ => "corvus-pluto sweep",
                    };
                    return (theme::OK, format!("{board} · {short}"), format!("{}\n{}\n{how}", d.version(), d.lib_path().display()));
                }
                let how = match d {
                    crate::hydra::Device::HackRF(_) => "stock firmware sweep".to_string(),
                    _ if d.host_driven() => "stock firmware · PC-driven sweep".to_string(),
                    _ => format!("Corvus firmware sweep{}", d.fw_revision()),
                };
                let colour = if d.host_driven() { theme::CAUTION } else { theme::OK };
                (colour, format!("{board} · {how}"), format!("{}\nSerial {serial}\nLibrary {}", d.version(), d.lib_path().display()))
            }
        }
    }

    fn device_chip(&mut self, ui: &mut Ui) {
        let (dot, text, details) = self.device_status();
        let g = ui.painter().layout_no_wrap(text, theme::ui_font(theme::SM), INK);
        let size = vec2(g.size().x + 44.0, 24.0);
        let (r, resp) = ui.allocate_exact_size(size, Sense::click());
        let p = ui.painter();
        p.rect_filled(r, 12, if resp.hovered() { CHIP } else { PANEL });
        p.rect_stroke(r, 12, Stroke::new(1.0, EDGE), egui::StrokeKind::Inside);
        p.circle_filled(pos2(r.left() + 13.0, r.center().y), 4.0, dot);
        p.galley(pos2(r.left() + 23.0, r.center().y - g.size().y / 2.0), g, INK);
        // a caret: the chip opens the radio picker
        let c = pos2(r.right() - 13.0, r.center().y + 1.0);
        let ink = if resp.hovered() || self.picker.is_some() { INK } else { INK2 };
        p.add(egui::Shape::convex_polygon(vec![c + vec2(-4.0, -2.5), c + vec2(4.0, -2.5), c + vec2(0.0, 2.5)], ink, Stroke::NONE));
        let clicked = resp.clicked();
        resp.on_hover_text(format!("{details}\n\nClick to switch radios."));
        if clicked {
            self.picker = if self.picker.is_some() { None } else { Some((r.left_bottom() + vec2(0.0, 6.0), crate::hydra::list_devices())) };
        }
    }

    fn settings_menu(&mut self, ui: &mut Ui) {
        ui.set_min_width(340.0);
        theme::section(ui, "Display settings");
        theme::row(ui, "Density", |ui| theme::segmented(ui, &mut self.cfg.density, &[(Density::Comfortable, "Comfortable"), (Density::Compact, "Compact")]));
        theme::row(ui, "Text size", |ui| theme::segmented(ui, &mut self.cfg.text_scale, &[(0.9, "90%"), (1.0, "100%"), (1.15, "115%"), (1.3, "130%")]));
        theme::row(ui, "Frame rate", |ui| theme::segmented(ui, &mut self.cfg.fps, &[(60, "60"), (30, "30"), (20, "20"), (15, "15")]));
        theme::caption(ui, "Frames per second. Lower rates save power; the sweep and its data are unaffected.");
        ui.add_space(4.0);
        theme::slider_f32(ui, "Grid", &mut self.cfg.grid_dim, &Slide::int(10.0, 100.0, 70.0, "%"));
        theme::slider_f32(ui, "Traces", &mut self.cfg.data_dim, &Slide::int(30.0, 100.0, 100.0, "%"));
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.cfg.hints, "Gesture hints")
                .on_hover_text("Short notes over the plot on what dragging, scrolling and the separator do. Each one goes away once you've used it.");
            if ui.add_enabled(self.cfg.hints_learned != 0, egui::Button::new("Show them again")).clicked() {
                self.cfg.hints = true;
                self.cfg.hints_learned = 0;
            }
        });
    }

    /// What needs attention, as sentences, worst first. Informational lines don't change the tile.
    fn conditions(&self) -> Vec<Cond> {
        let mut out = Vec::new();
        let s = &self.stats;
        if let Src::Offline(e) = &self.src {
            let e = if e.is_empty() { "not connected".to_string() } else { e.clone() };
            out.push(Cond { level: Level::Alarm, text: format!("The {} isn't open: {e}.", crate::hydra::board().name()), acts: vec![(Act::Retry, "Try again"), (Act::Sim, "Run the simulator")] });
        }
        if let Some(e) = &self.error {
            out.push(Cond { level: Level::Alarm, text: format!("The radio didn't take the last setting: {e}."), acts: vec![(Act::Dismiss, "Dismiss")] });
        }
        if self.issues_recent() {
            out.push(Cond {
                level: Level::Caution,
                text: format!(
                    "Steps were lost or damaged in the last 10 s ({} torn, {} gaps, {} timeouts, {} dropped). A busy USB bus or CPU causes this: close other USB streams or lower the frame rate.",
                    s.torn,
                    s.gaps + s.misfiled,
                    s.lock_timeouts,
                    s.host_drops
                ),
                acts: vec![],
            });
        }
        if self.host_driven() {
            out.push(Cond {
                level: Level::Info,
                text: format!(
                    "The {} has stock firmware, so the PC retunes it step by step (tens of steps a second). The Corvus firmware sweeps on the board, about ten times faster: see firmware/README.md.",
                    crate::hydra::board().name()
                ),
                acts: vec![],
            });
        }
        if self.applied.lock_reads == 0 && !self.applied.rate.complex() && !self.host_driven() {
            out.push(Cond { level: Level::Caution, text: "The PLL lock check is off: about 1 in 5 steps come out wrong, by up to 22 dB.".into(), acts: vec![(Act::LockOn, "Turn it on")] });
        }
        if !self.running {
            out.push(Cond { level: Level::Info, text: "Paused. The last sweep stays on screen.".into(), acts: vec![(Act::Resume, "Run")] });
        }
        if let Src::Sim(_) = self.src {
            out.push(Cond { level: Level::Info, text: "Simulator: a synthetic band plan with the firmware's timing.".into(), acts: vec![(Act::Retry, connect_label())] });
        }
        let (_, slow) = self.applied.divider_transitions();
        if slow > 0 && self.applied.order == Order::VcoContinuous {
            let sw: Vec<String> = self.switches.iter().filter(|&&f| f > self.applied.start_hz && f < self.applied.stop_hz).map(|f| format!("{:.1}", f / 1e6)).collect();
            out.push(Cond { level: Level::Info, text: format!("This range straddles the tuner's VCO switch at {} MHz and is too small to route around it, which costs about 10 ms per pass.", sw.join(", ")), acts: vec![] });
        }
        if self.cfg.eq_on && self.eq_state == EqState::Unknown && self.applied.n() < 12 && !matches!(self.src, Src::Offline(_)) {
            out.push(Cond { level: Level::Info, text: "Flatten passband has nothing learned at this bin width yet: it needs one sweep of 12 steps or more (Full range works).".into(), acts: vec![] });
        }
        out.sort_by_key(|c| std::cmp::Reverse(c.level));
        out
    }

    /// The PC retunes each step (stock firmware, or `--host-sweep`).
    fn host_driven(&self) -> bool {
        matches!(&self.src, Src::Device(d) if d.host_driven())
    }

    fn issues_recent(&self) -> bool {
        self.bad_last.0 > 0 && self.engine.now() - self.bad_last.1 < 10.0
    }

    /// The status bar (rule A3): one traffic-light tile, then conditions as sentences.
    fn status_bar(&mut self, ui: &mut Ui) {
        let conds = self.conditions();
        let problems = conds.iter().filter(|c| c.level >= Level::Caution).count();
        let level = conds.iter().map(|c| c.level).filter(|&l| l >= Level::Caution).max().unwrap_or(Level::Ok);
        let mut act = None;
        ui.horizontal(|ui| {
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some((msg, t)) = &self.toast {
                    if t.elapsed().as_secs_f32() < theme::TOAST_S {
                        ui.label(RichText::new(msg.as_str()).size(theme::SM).color(INK));
                        ui.label(RichText::new("✓").size(theme::SM).color(INK2));
                        ui.ctx().request_repaint_after(Duration::from_millis(200));
                    } else {
                        self.toast = None;
                    }
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center).with_main_wrap(true), |ui| {
                    let word = match problems {
                        0 => "All good".to_string(),
                        1 => "1 problem".to_string(),
                        n => format!("{n} problems"),
                    };
                    egui::Frame::new().fill(PANEL).stroke(egui::Stroke::new(1.0, if level == Level::Alarm { theme::ALARM } else { LINE })).corner_radius(14).inner_margin(egui::Margin::symmetric(10, 3)).show(ui, |ui| {
                        ui.horizontal(|ui| {
                            let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                            theme::status_dot(ui.painter(), r.center(), level);
                            ui.label(RichText::new(word).size(theme::SM).strong().color(INK));
                        });
                    });
                    for c in &conds {
                        ui.add_space(6.0);
                        let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
                        theme::status_dot(ui.painter(), r.center(), c.level);
                        ui.label(RichText::new(c.text.as_str()).size(theme::SM).color(INK));
                        for &(a, label) in &c.acts {
                            if ui.small_button(label).clicked() {
                                act = Some(a);
                            }
                        }
                    }
                });
            });
        });
        match act {
            Some(Act::Retry) => self.retry_device(),
            Some(Act::Sim) => self.use_sim(),
            Some(Act::Dismiss) => self.error = None,
            Some(Act::LockOn) => self.cfg.lock_reads = 2,
            Some(Act::Resume) => self.toggle_run(),
            None => {}
        }
    }

    fn hud(&mut self, ui: &mut Ui) {
        let s = &self.stats;
        let p = &self.applied;
        let rate = s.steps_per_s;
        let n = p.n() as f32;
        let passes = if n > 0.0 { rate / n } else { 0.0 };
        let span_hz = p.stop_hz - p.start_hz;
        let (fast, slow) = p.divider_transitions();
        let (span_v, span_u) = if span_hz >= 1e6 { (format!("{:.0}", span_hz / 1e6), "MHz") } else { (format!("{:.0}", span_hz / 1e3), "kHz") };
        let bad = s.torn + s.gaps + s.misfiled + s.lock_timeouts + s.host_drops;
        let recent = self.issues_recent();
        let cards: Vec<Card> = vec![
            Card { label: "Sweep speed", value: format!("{rate:.0}"), unit: "steps/s", sub: if rate > 0.0 { format!("{:.2} ms per step · last 60 s", 1e3 / rate) } else { "—".into() }, flag: false, spark: true },
            Card { label: "Throughput", value: format!("{:.2}", rate as f64 * p.step_hz.min(p.window_hz as u32) as f64 / 1e9), unit: "GHz/s", sub: format!("{} MHz steps · dwell {}", fmt_trim(p.window_hz / 1e6, 2), p.dwell), flag: false, spark: false },
            Card {
                label: if self.cfg.live { "Updates" } else { "Full passes" },
                value: if passes >= 100.0 { format!("{passes:.0}") } else { format!("{passes:.2}") },
                unit: "/s",
                sub: if passes > 0.0 { format!("{:.0} ms per pass · {} so far", 1e3 / passes, s.passes) } else { format!("{} so far", s.passes) },
                flag: false,
                spark: false,
            },
            Card { label: "Span", value: span_v, unit: span_u, sub: format!("{} steps · {:.1} kHz bins", p.n_steps, dsp::bin_hz(self.cfg.nfft, p.rate) / 1e3), flag: false, spark: false },
            match p.order {
                Order::VcoContinuous if p.rate.iq16() => Card { label: "Visit order", value: "Back and forth".into(), unit: "", sub: "every pass reversed · no wrap".into(), flag: false, spark: false },
                Order::VcoContinuous => Card { label: "Visit order", value: "VCO".into(), unit: "continuous", sub: format!("{fast} fast joins · {slow} slow per pass"), flag: false, spark: false },
                Order::Ascending => Card { label: "Visit order", value: "Ascending".into(), unit: "", sub: format!("{slow} slow crossings per pass"), flag: false, spark: false },
                Order::Interleaved => Card { label: "Visit order", value: "Interleaved".into(), unit: "", sub: "two 5 MHz slices per tune".into(), flag: false, spark: false },
            },
            if p.rate.iq16() {
                // corvus-pluto's fast mode drops only the settle blocks; its driver path two stale ones as well
                let drop = if self.pluto_fast() { p.skip } else { 2 + p.skip };
                let sub = if self.pluto_fast() && drop == 0 {
                    "none: fastlock settles in ~1 µs".to_string()
                } else {
                    format!("dropped after each retune · {:.0} µs", drop as f64 * p.rate.block_s() * 1e6)
                };
                Card { label: "Settling", value: format!("{drop}"), unit: "blocks", sub, flag: false, spark: false }
            } else if p.rate.complex() {
                Card { label: "Settling", value: "Fixed".into(), unit: "", sub: "the HackRF firmware waits 0.8 ms per tune".into(), flag: false, spark: false }
            } else if self.host_driven() {
                Card { label: "Settling", value: "PC-timed".into(), unit: "", sub: "2 USB transfers skipped per step".into(), flag: false, spark: false }
            } else if p.lock_reads == 0 {
                Card { label: "PLL lock", value: "Off".into(), unit: "", sub: "steps aren't lock-checked".into(), flag: true, spark: false }
            } else {
                Card { label: "PLL lock", value: format!("{}", s.lock_median), unit: "polls", sub: format!("max {} · {} timeouts", s.lock_max, s.lock_timeouts), flag: false, spark: false }
            },
            Card {
                label: "Integrity",
                value: if bad == 0 { "Clean".into() } else { format!("{bad}") },
                unit: if bad == 0 { "" } else { "issues" },
                sub: if bad > 0 && !recent { "none in the last 10 s".into() } else { format!("{} torn · {} gaps · {} drops", s.torn, s.gaps + s.misfiled, s.host_drops) },
                flag: recent,
                spark: false,
            },
            Card { label: if crate::hydra::board().networked() { "Network" } else { "USB" }, value: format!("{:.1}", s.usb_mb_s), unit: "MB/s", sub: format!("{} blocks", s.blocks), flag: false, spark: false },
        ];
        let gap = 6.0;
        let w = ((ui.available_width() - gap * (cards.len() as f32 - 1.0)) / cards.len() as f32).max(110.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for c in &cards {
                let (r, _) = ui.allocate_exact_size(vec2(w, 74.0), Sense::hover());
                paint_card(ui.painter(), r, c, if c.spark { Some(&self.stats.history) } else { None });
            }
        });
    }

    fn sidebar(&mut self, ui: &mut Ui) {
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            let gap = self.cfg.density.gap();
            theme::card(ui, |ui| {
                let has_live = crate::hydra::board().has_live();
                ui.horizontal(|ui| {
                    theme::section(ui, if self.cfg.live { "Live" } else { "Range" });
                    if has_live {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let mut live = self.cfg.live;
                            if theme::segmented(ui, &mut live, &[(false, "Sweep"), (true, "Live")]) && live != self.cfg.live {
                                if live {
                                    let c = (self.view.0 + self.view.1) / 2e6;
                                    self.cfg.set_live(c);
                                } else {
                                    self.cfg.leave_live();
                                }
                            }
                        });
                    }
                });
                if self.cfg.live && has_live {
                    let (fmin, fmax) = (self.cfg.rate.f_range().0 / 1e6, self.cfg.rate.f_range().1 / 1e6);
                    let mut c = self.cfg.live_centre_mhz();
                    let c0 = c;
                    theme::row(ui, "Centre", |ui| {
                        ui.add(freq_field(&mut c, fmin..=fmax)).on_hover_text("Shift+arrows: a span down or up. L: back to the sweep");
                        ui.label(RichText::new("MHz").size(theme::SM).color(INK2));
                    });
                    let mut span = self.cfg.live_span_mhz;
                    let changed = theme::row(ui, "Span", |ui| theme::segmented(ui, &mut span, LIVE_SPANS));
                    if c != c0 || changed {
                        self.cfg.live_span_mhz = span;
                        self.cfg.set_live(c);
                    }
                    theme::caption(
                        ui,
                        format!(
                            "{} · {:.2} kHz bins (FFT of one block). The board stays on this frequency; every block is an update. L or Sweep goes back to the sweep.",
                            self.cfg.rate.label(),
                            self.cfg.rate.adc_hz() / dsp::n_samples(self.cfg.rate) as f64 / 1e3
                        ),
                    );
                    return;
                }
                // Typed values apply on Enter, Tab or clicking away (Esc cancels), not per keystroke.
                let (s0, e0) = (self.cfg.start_mhz, self.cfg.stop_mhz);
                let focus = std::mem::take(&mut self.focus_start);
                let (fmin, fmax) = (self.cfg.rate.f_range().0 / 1e6, self.cfg.rate.f_range().1 / 1e6);
                theme::row(ui, "Start", |ui| {
                    if focus {
                        let id = ui.next_auto_id();
                        ui.memory_mut(|m| m.request_focus(id));
                    }
                    ui.add(freq_field(&mut self.cfg.start_mhz, fmin..=fmax - 0.2)).on_hover_text("G to type a range");
                    ui.label(RichText::new("MHz").size(theme::SM).color(INK2));
                });
                theme::row(ui, "Stop", |ui| {
                    ui.add(freq_field(&mut self.cfg.stop_mhz, fmin + 0.2..=fmax));
                    ui.label(RichText::new("MHz").size(theme::SM).color(INK2));
                });
                self.cfg.keep_span(s0, e0);
                theme::row(ui, "Step", |ui| theme::segmented(ui, &mut self.cfg.step_mhz, self.cfg.rate.steps()));
                let current = PRESETS.iter().position(|pr| (self.cfg.start_mhz - pr.start_mhz).abs() < 1e-6 && (self.cfg.stop_mhz - pr.stop_mhz).abs() < 1e-6);
                let mut pick = None;
                theme::row(ui, "Band", |ui| {
                    egui::ComboBox::from_id_salt("band")
                        .width(ui.available_width() - 4.0)
                        .selected_text(current.map_or("Custom", |i| PRESETS[i].name))
                        .show_ui(ui, |ui| {
                            for (i, pr) in PRESETS.iter().enumerate() {
                                let key = if i < 9 { format!("{}", i + 1) } else { String::new() };
                                let text = format!("{:<14}{:>9} – {} MHz   {}", pr.name, fmt_trim(pr.start_mhz, 1), fmt_trim(pr.stop_mhz, 1), key);
                                if ui.selectable_label(current == Some(i), RichText::new(text).font(theme::num(theme::SM))).clicked() {
                                    pick = Some((pr.start_mhz, pr.stop_mhz));
                                }
                            }
                        });
                });
                if let Some((a, b)) = pick {
                    self.set_range(a * 1e6, b * 1e6);
                }
                ui.horizontal_wrapped(|ui| {
                    let zoomed = self.view.0 > self.applied.start_hz + 1e3 || self.view.1 < self.applied.stop_hz - 1e3;
                    if ui.add_enabled(zoomed, egui::Button::new("Sweep visible area")).on_hover_text("Zoom in first (scroll, or drag a span on the plot), or drag a span and pick Sweep. Key: S").on_disabled_hover_text("Zoom in first (scroll, or drag a span on the plot), or drag a span and pick Sweep").clicked() {
                        let (a, b) = self.view;
                        self.set_range(a, b);
                    }
                    if ui.button("Full range").on_hover_text("F").clicked() {
                        let (a, b) = self.cfg.rate.f_range();
                        self.set_range(a, b);
                    }
                    if let Some((a, b)) = self.prev_range
                        && theme::restore_button(ui, &format!("{}–{} MHz", fmt_trim(a / 1e6, 3), fmt_trim(b / 1e6, 3))).on_hover_text("Back to the previous range (Backspace)").clicked()
                    {
                        self.back_range();
                    }
                });
            });
            ui.add_space(gap);
            theme::card(ui, |ui| {
                // the way back from a profile sits on the header line, so the card stays one row taller at most
                let mut back = false;
                ui.horizontal(|ui| {
                    theme::section(ui, "Sweep engine");
                    if let Some((_, name)) = self.prev_engine {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            back = theme::restore_button(ui, name).on_hover_text("Back to the sweep settings before the last profile").clicked();
                        });
                    }
                });
                if back {
                    self.back_engine();
                }
                let current = self.cfg.profile();
                let mut pick = None;
                theme::row(ui, "Profile", |ui| {
                    ui.spacing_mut().item_spacing.x = 3.0;
                    for (i, pr) in PROFILES.iter().enumerate() {
                        if theme::chip(ui, current == Some(i), pr.name).on_hover_text(format!("{}\n\nKey: {}", pr.hint, pr.key)).clicked() {
                            pick = Some(i);
                        }
                    }
                });
                if let Some(i) = pick {
                    self.apply_profile(i);
                }
                theme::more(ui, "engine-more", "Rate, order, dwell and lock check", |ui| {
                    let mut rate = self.cfg.rate;
                    let rate_hint = if crate::hydra::board() == crate::hydra::Board::Pluto {
                        "61.44 MSPS: steps up to 50 MHz (the noise floor stays within 1 dB over 40), the fastest sweep.\n30.72 MSPS: steps up to 25 MHz, bins half as wide.\n15.36, 7.68, 3.84 MSPS: narrower steps, bins down to 0.94 kHz; mostly for the live view."
                    } else {
                        "10 MSPS: 20 MSPS real, IF 5 MHz, steps up to 9.3 MHz.\n12 MSPS: 24 MSPS real, tuner filter wide open, steps up to 10 MHz, about a third more GHz/s."
                    };
                    let rates = crate::hydra::board().rates();
                    if rates.len() > 1 {
                        let opts: Vec<(Rate, &str)> = rates.iter().map(|&r| (r, r.label())).collect();
                        let live = self.cfg.live;
                        if theme::row_hint(ui, "Rate", rate_hint, |ui| ui.add_enabled_ui(!live, |ui| theme::segmented(ui, &mut rate, &opts)).inner) && !live {
                            self.cfg.set_rate(rate);
                        }
                    } else {
                        let only = if self.cfg.rate.sliced() {
                            "The HackRF firmware's sweep rate: 20 MSPS complex (8-bit IQ). Each tune gives two 5 MHz slices, either side of the tuner's DC.".to_string()
                        } else {
                            format!(
                                "The {}'s fastest setting: {:.0} MSPS real, IF {:.0} MHz, steps up to {} MHz.",
                                crate::hydra::board().name(),
                                self.cfg.rate.adc_hz() / 1e6,
                                self.cfg.rate.if_hz() / 1e6,
                                fmt_trim(self.cfg.rate.max_window_hz() / 1e6, 1)
                            )
                        };
                        theme::row_hint(ui, "Rate", &only, |ui| {
                            ui.label(RichText::new(self.cfg.rate.label()).size(theme::SM).color(INK));
                            false
                        });
                    }
                    let complex = self.cfg.rate.complex();
                    let host = self.host_driven();
                    if !complex {
                        theme::row_hint(
                            ui,
                            "Order",
                            "VCO-continuous groups steps by the tuner's VCO divider, so divider switches cost about 0.5 ms instead of 10 ms.\nAscending goes low to high: each divider switch costs about 10 ms, plus 8 ms to wrap.",
                            |ui| theme::segmented(ui, &mut self.cfg.order, &[(Order::VcoContinuous, "VCO-continuous"), (Order::Ascending, "Ascending")]),
                        );
                    }
                    theme::slider_u8(ui, "Dwell", &mut self.cfg.dwell, &Slide::int(1.0, 8.0, 1.0, "blk"));
                    if self.cfg.rate.iq16() {
                        theme::slider_u8(ui, "Settle", &mut self.cfg.skip, &Slide::int(0.0, 8.0, 0.0, "blk"));
                        theme::caption(
                            ui,
                            format!(
                                "The board sweeps back and forth, so it never wraps. Dwell averages more blocks per step. Settle drops extra {:.0} µs blocks after each retune{}.",
                                self.cfg.rate.block_s() * 1e6,
                                if self.pluto_fast() { "; in fast mode none are needed (fastlock settles in ~1 µs)" } else { ", on top of the two the board drops through its driver" }
                            ),
                        );
                    } else if !complex && !host {
                        theme::slider_u8(ui, "Lock reads", &mut self.cfg.lock_reads, &Slide::int(0.0, 3.0, 2.0, ""));
                        theme::slider_u8(ui, "Settle", &mut self.cfg.skip, &Slide::int(0.0, 8.0, 0.0, "blk"));
                        theme::caption(ui, &format!("Dwell averages more blocks per step (less noise, slower). Settle discards extra {:.0} µs blocks after lock.", self.cfg.rate.block_s() * 1e6));
                    } else if complex {
                        theme::caption(ui, "Dwell averages more blocks per tune. The HackRF's firmware sets its own settling (two blocks per tune).");
                    } else {
                        theme::caption(ui, "Dwell averages more blocks per step. With stock firmware the PC times the settling.");
                    }
                });
            });
            ui.add_space(gap);
            theme::card(ui, |ui| {
                theme::section(ui, "Receiver");
                let ports = crate::hydra::board().ports();
                if !ports.is_empty() {
                    let label = if crate::hydra::board() == crate::hydra::Board::Pluto { "Input" } else { "Port" };
                    theme::row(ui, label, |ui| theme::segmented(ui, &mut self.cfg.port, ports));
                }
                let mut mode = match self.cfg.gain {
                    Gain::Linearity(_) => 0,
                    Gain::Sensitivity(_) => 1,
                    Gain::Manual { .. } => 2,
                };
                let pluto = crate::hydra::board() == crate::hydra::Board::Pluto;
                let gain_hint = if pluto {
                    "The Pluto has one manual RX gain (0–71 dB): every mode here sets it, Gain 10 is 34 dB. It changes mid-sweep without stopping; switching between RX1 and RX2 restarts the sweep."
                } else {
                    "Linearity 10 keeps FM and TV out of clipping. Gain and port changes stop the sweep for a moment (the firmware refuses tuner commands mid-sweep). The bias tee stays off."
                };
                if theme::row_hint(ui, "Gain mode", gain_hint, |ui| theme::segmented(ui, &mut mode, &[(0, "Linearity"), (1, "Sensitivity"), (2, "Manual")])) {
                    self.cfg.gain = match mode {
                        0 => Gain::Linearity(10),
                        1 => Gain::Sensitivity(10),
                        _ => Gain::Manual { lna: 8, mixer: 8, vga: 8 },
                    };
                }
                match &mut self.cfg.gain {
                    Gain::Linearity(g) | Gain::Sensitivity(g) => {
                        theme::slider_u8(ui, "Gain", g, &Slide::int(0.0, 21.0, 10.0, ""));
                    }
                    Gain::Manual { lna, mixer, vga } => {
                        theme::slider_u8(ui, "LNA", lna, &Slide::int(0.0, 14.0, 8.0, ""));
                        theme::slider_u8(ui, "Mixer", mixer, &Slide::int(0.0, 15.0, 8.0, ""));
                        theme::slider_u8(ui, "VGA", vga, &Slide::int(0.0, 15.0, 8.0, ""));
                    }
                }
                if pluto {
                    theme::caption(ui, format!("{} dB of RX gain on the AD936x.", crate::pluto::gain_db(self.cfg.gain)));
                }
            });
            ui.add_space(gap);
            theme::card(ui, |ui| {
                theme::section(ui, "Display");
                let opts: Vec<(usize, String)> = NFFTS.iter().rev().map(|&n| (n, format!("{:.1}", dsp::bin_hz(n, self.cfg.rate) / 1e3))).collect();
                let opts: Vec<(usize, &str)> = opts.iter().map(|(n, s)| (*n, s.as_str())).collect();
                theme::row(ui, "Bins (kHz)", |ui| theme::segmented(ui, &mut self.cfg.nfft, &opts));
                theme::row(ui, "Detector", |ui| theme::segmented(ui, &mut self.cfg.detector, &[(Detector::Peak, "Peak"), (Detector::Mean, "Mean")]));
                theme::row(ui, "Traces", |ui| {
                    swatch(ui, TRACE);
                    ui.label(RichText::new("Live").size(theme::SM));
                    swatch(ui, PEAK);
                    let name = self.cfg.peak_name();
                    ui.checkbox(&mut self.cfg.show_peak, name).on_hover_text("The peak trace (H). Hold, decay or percentile: under the disclosure below.");
                    swatch(ui, AVERAGE);
                    ui.checkbox(&mut self.cfg.show_avg, "Average").on_hover_text("V");
                });
                if self.cfg.show_avg {
                    theme::slider_f32(ui, "Averaging", &mut self.cfg.avg_secs, &Slide { min: 0.5, max: 60.0, default: 10.0, integer: false, log: true, decimals: 1, unit: "s" });
                    let passes = self.cfg.avg_secs / self.pass_s;
                    let kind = if self.cfg.avg_power { "power" } else { "dB" };
                    theme::caption(ui, if passes.is_finite() && passes > 0.0 {
                        format!("About {} passes at the current speed, averaged in {kind}.", fmt_trim(passes as f64, if passes < 10.0 { 1 } else { 0 }))
                    } else {
                        format!("Averaged in {kind}.")
                    });
                }
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.cfg.eq_on, "Flatten passband")
                        .on_hover_text("Learns the tuner's per-step passband ripple (the median across steps) and removes it, so steps join without scalloping. Key: E");
                    let txt = match self.eq_state {
                        EqState::Off => "off".to_string(),
                        EqState::Unknown => "needs one wide sweep".into(),
                        EqState::Learning(n) => format!("learning, {n} passes"),
                        EqState::Learned(n) => format!("learned from {n} passes"),
                    };
                    ui.label(RichText::new(txt).size(theme::SM).color(INK2));
                });
                theme::row(ui, "Peak labels", |ui| theme::segmented(ui, &mut self.cfg.labels, &[(12, "Full"), (4, "Focus"), (0, "Minimal")]));
                theme::more(ui, "display-more", "Peak and average modes, layers, passband", |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.cfg.beam, "Sweep beam")
                            .on_hover_text("Lights the steps as the radio visits them, when a full pass takes about a second or more. Faster sweeps would only flash, so the strip under the axis shows which steps are fresh instead. Key: B");
                        ui.checkbox(&mut self.cfg.vco_marks, "VCO switch marks");
                    });
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.cfg.readout_follow, "Readout follows cursor")
                            .on_hover_text("The frequency and level readout rides next to the pointer. Off: it stays in the spectrum's top-left corner.");
                        ui.add_enabled(self.view3d_ok, egui::Checkbox::new(&mut self.cfg.wf3d, "3D waterfall"))
                            .on_hover_text("The waterfall as a landscape: drag to turn, scroll to zoom, double-click to reset. Key: W");
                        ui.checkbox(&mut self.cfg.overview, "Overview strip")
                            .on_hover_text("The whole sweep above the spectrum, with the zoomed view as a box: drag the box to pan, its edges to zoom, click to centre, double-click to show everything.");
                    });
                    theme::row_hint(ui, "Peak trace", "Hold keeps the highest level since the last clear. Decay rises at once, holds 1 s, then falls at the set rate, so it shows where signals have been lately. Percentile tracks the level each bin is under that share of the time (P90: exceeded 10 % of the time), which ignores single spikes and shows how busy a channel is.", |ui| {
                        theme::segmented(ui, &mut self.cfg.peak_kind, &[(PeakKind::Hold, "Hold"), (PeakKind::Decay, "Decay"), (PeakKind::Percentile, "Percentile")])
                    });
                    match self.cfg.peak_kind {
                        PeakKind::Hold => {}
                        PeakKind::Decay => {
                            theme::slider_f32(ui, "Fall", &mut self.cfg.peak_fall, &Slide { min: 1.0, max: 30.0, default: 6.0, integer: false, log: true, decimals: 1, unit: "dB/s" });
                        }
                        PeakKind::Percentile => {
                            theme::row(ui, "Percentile", |ui| theme::segmented(ui, &mut self.cfg.peak_pct, &[(50, "50"), (75, "75"), (90, "90"), (95, "95")]));
                            theme::slider_f32(ui, "Window", &mut self.cfg.peak_win, &Slide { min: 1.0, max: 120.0, default: 10.0, integer: false, log: true, decimals: 0, unit: "s" });
                        }
                    }
                    theme::row_hint(ui, "Average in", "dB is a video average: smooth, and about 2.5 dB under the true mean on noise; bursts move it little. Power averages the actual power, so busy or bursty channels read at their mean power.", |ui| {
                        theme::segmented(ui, &mut self.cfg.avg_power, &[(false, "dB"), (true, "Power")])
                    });
                    if self.cfg.wf3d && self.view3d_ok {
                        theme::row_hint(ui, "3D smoothing", "A Gaussian filter over the landscape, averaged in power so narrow carriers keep most of their height while the noise floor calms down. The flat waterfall and the spectrum are never smoothed.", |ui| {
                            theme::segmented(ui, &mut self.cfg.smooth3d, &[(0, "Off"), (1, "Light"), (2, "Strong")])
                        });
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Clear peaks").on_hover_text("P").clicked() {
                            self.engine.clear_peak();
                        }
                        if matches!(self.eq_state, EqState::Learning(_) | EqState::Learned(_)) && theme::confirm_button(ui, "Relearn the passband", "Click again: forget the learned shape") {
                            self.engine.reset_eq();
                            self.toast("Relearning the passband", false);
                        }
                    });
                });
            });
            ui.add_space(gap);
            theme::card_more(ui, "levels", "Levels and colour map", |ui| {
                theme::row(ui, "Spectrum", |ui| {
                    ui.add(egui::DragValue::new(&mut self.cfg.spec_bot).speed(0.5).range(-160.0..=0.0));
                    ui.label(RichText::new("to").size(theme::SM).color(INK2));
                    ui.add(egui::DragValue::new(&mut self.cfg.spec_top).speed(0.5).range(-150.0..=20.0));
                    ui.label(RichText::new("dBFS").size(theme::SM).color(INK2));
                });
                theme::row(ui, "Waterfall", |ui| {
                    ui.add(egui::DragValue::new(&mut self.cfg.wf_lo).speed(0.5).range(-160.0..=0.0));
                    ui.label(RichText::new("to").size(theme::SM).color(INK2));
                    ui.add(egui::DragValue::new(&mut self.cfg.wf_hi).speed(0.5).range(-150.0..=20.0));
                    ui.label(RichText::new("dBFS").size(theme::SM).color(INK2));
                });
                theme::row(ui, "Colour map", |ui| {
                    ui.vertical(|ui| {
                        let mut pal = self.cfg.palette;
                        theme::segmented(ui, &mut pal, &Palette::ALL);
                        self.cfg.palette = pal;
                        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 8.0), Sense::hover());
                        paint_palette_bar(ui.painter(), r, self.cfg.palette);
                    });
                });
                theme::more(ui, "levels-more", "Waterfall history and row rate", |ui| {
                    theme::slider_usize(ui, "History", &mut self.cfg.wf_rows, &Slide::int(32.0, ROWS as f64, 160.0, "rows"));
                    theme::slider_f32(ui, "Max rate", &mut self.cfg.wf_rate, &Slide::int(1.0, 60.0, 20.0, "rows/s"));
                });
            });
        });
    }

    /// The footer: where the data comes from, and what drawing costs (rule P2).
    fn footer(&mut self, ui: &mut Ui) {
        ui.horizontal_centered(|ui| {
            let src = match &self.src {
                Src::Device(d) if crate::hydra::board().networked() => format!("{} · {}", d.version(), d.lib_path().display()),
                Src::Device(d) => format!("{} · {} · {} · rev {}", d.version(), if self.serial == 0 { format!("first {}", crate::hydra::board().name()) } else { format!("{:016X}", self.serial) }, d.lib_path().display(), d.fw_revision()),
                Src::Sim(_) => "Simulator".to_string(),
                Src::Offline(_) => format!("{} not open", crate::hydra::board().name()),
            };
            ui.label(RichText::new(src).size(theme::XS + 1.0).color(INK2));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let fps = if self.frame_times.len() > 1 {
                    (self.frame_times.len() - 1) as f32 / (self.frame_times.back().unwrap() - self.frame_times.front().unwrap()).max(1e-3)
                } else {
                    0.0
                };
                let budget = 1000.0 / self.cfg.fps as f32;
                ui.label(RichText::new(format!("{fps:.0} of {} fps · {:.1} ms per frame", self.cfg.fps, self.frame_ms)).font(theme::num(theme::XS)).color(INK2));
                let (r, _) = ui.allocate_exact_size(vec2(60.0, 6.0), Sense::hover());
                ui.painter().rect_filled(r, 3, CHIP);
                let k = (self.frame_ms / budget).clamp(0.0, 1.0);
                let c = if k > 0.8 { CAUTION } else { INK2 };
                ui.painter().rect_filled(Rect::from_min_size(r.min, vec2(r.width() * k, r.height())), 3, c);
                ui.label(RichText::new("load").size(theme::XS + 1.0).color(INK2));
            });
        });
    }

    fn central(&mut self, ui: &mut Ui) {
        let full = ui.available_rect_before_wrap();
        ui.allocate_rect(full, Sense::hover());
        let gutter = 50.0;
        let ov_h = if self.cfg.overview { 34.0 } else { 0.0 };
        let rail_h = if self.cfg.labels > 0 { 26.0 } else { 6.0 };
        let axis_h = 20.0;
        let strip_h = 20.0;
        let sep_h = 10.0;
        let avail = (full.height() - ov_h - rail_h - axis_h - strip_h - 2.0 - sep_h).max(200.0);
        let spec_h = (avail * self.cfg.split.clamp(0.15, 0.85)).max(80.0);
        let left = full.left() + gutter;
        let width = full.width() - gutter;
        let ov = Rect::from_min_size(pos2(left, full.top()), vec2(width, (ov_h - 6.0).max(0.0)));
        let rail = Rect::from_min_size(pos2(left, full.top() + ov_h), vec2(width, rail_h));
        let spec = Rect::from_min_size(pos2(left, rail.bottom()), vec2(width, spec_h));
        let axis = Rect::from_min_size(pos2(left, spec.bottom()), vec2(width, axis_h));
        let strip = Rect::from_min_size(pos2(left, axis.bottom() + 2.0), vec2(width, strip_h));
        let sep = Rect::from_min_size(pos2(left, strip.bottom()), vec2(width, sep_h));
        let wf = Rect::from_min_max(pos2(left, sep.bottom()), pos2(full.right(), full.bottom()));

        self.clamp_view();
        if self.cfg.overview {
            self.interact_overview(ui, ov);
        }
        let three = self.cfg.wf3d && self.view3d_ok;
        self.interact(ui, spec, (!three).then_some(wf), axis, Rect::from_min_max(pos2(full.left(), spec.top()), pos2(spec.left() - 4.0, spec.bottom())));
        if three {
            self.interact_3d(ui, wf);
        }
        self.interact_separator(ui, sep, avail);
        self.gather(spec.width().max(1.0) as usize, ov.width().max(1.0) as usize);

        let painter = ui.painter().clone();
        if self.cfg.overview {
            self.paint_overview(&painter, ov);
        }
        self.paint_spectrum(&painter, spec);
        self.paint_axis(&painter, axis);
        self.paint_strip(&painter, strip);
        self.paint_separator(&painter, sep);
        if three {
            self.paint_3d(&painter, wf);
        } else {
            self.paint_waterfall(ui.ctx(), &painter, wf, Rect::from_min_max(pos2(full.left(), wf.top()), pos2(wf.left() - 4.0, wf.bottom())));
        }
        self.paint_overlays(&painter, spec, wf, three);
        self.wf_mode_toggle(ui, wf);
        self.peak_rail(ui, rail, spec);
        self.paint_gesture_hint(&painter, spec, (!three).then_some(wf));
        self.paint_range_drag(&painter, spec, ov);
        self.paint_readout(&painter, spec, wf);
        self.selection_menu(ui);
        if let Src::Offline(err) = &self.src {
            let err = err.clone();
            self.offline_banner(ui, spec, &err);
        }
    }

    /// The separator between spectrum and waterfall: drag to give one more room (double-click: default).
    fn interact_separator(&mut self, ui: &mut Ui, sep: Rect, avail: f32) {
        // reaches a little into the strip and the waterfall, so the thin grip is easy to catch
        let resp = ui.interact(sep.expand2(vec2(0.0, 5.0)), ui.id().with("split"), Sense::click_and_drag());
        self.sep_hot = resp.hovered() || resp.dragged();
        if resp.dragged() {
            self.cfg.split = (self.cfg.split + resp.drag_delta().y / avail).clamp(0.15, 0.85);
        }
        if resp.double_clicked() {
            self.cfg.split = Config::default().split;
        }
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        resp.on_hover_text("Drag to share the height between the spectrum and the waterfall. Double-click to reset.");
    }

    fn paint_separator(&self, p: &Painter, sep: Rect) {
        let c = sep.center();
        let (w, ink) = if self.sep_hot { (30.0, TOUCH) } else { (18.0, INK3) };
        p.line_segment([pos2(c.x - w, c.y - 1.5), pos2(c.x + w, c.y - 1.5)], Stroke::new(1.0, ink));
        p.line_segment([pos2(c.x - w, c.y + 1.5), pos2(c.x + w, c.y + 1.5)], Stroke::new(1.0, ink));
    }

    /// Overview strip: drag the view box to pan, its edges to zoom, click elsewhere to centre the view
    /// there, double-click to show the whole sweep.
    fn interact_overview(&mut self, ui: &mut Ui, ov: Rect) {
        let (lo, hi) = (self.applied.start_hz, self.applied.stop_hz);
        let fx = |f: f64| ov.left() + ((f - lo) / (hi - lo)) as f32 * ov.width();
        let xf = |x: f32| lo + ((x - ov.left()) / ov.width()) as f64 * (hi - lo);
        let resp = ui.interact(ov, ui.id().with("overview"), Sense::click_and_drag());
        let (x0, x1) = (fx(self.view.0), fx(self.view.1));
        let grab_at = |x: f32| {
            if (x - x0).abs() <= 5.0 {
                Some(OvGrab::Left)
            } else if (x - x1).abs() <= 5.0 {
                Some(OvGrab::Right)
            } else if x > x0 && x < x1 {
                Some(OvGrab::Move)
            } else {
                None
            }
        };
        let hover = resp.hover_pos().map(|pos| grab_at(pos.x));
        if let Some(g) = hover {
            ui.ctx().set_cursor_icon(match g {
                Some(OvGrab::Left | OvGrab::Right) => egui::CursorIcon::ResizeHorizontal,
                Some(OvGrab::Move) => egui::CursorIcon::Grab,
                None => egui::CursorIcon::PointingHand,
            });
        }
        let resp = match hover {
            Some(Some(OvGrab::Left | OvGrab::Right)) => resp.on_hover_text("Drag the edge to zoom"),
            Some(Some(OvGrab::Move)) if self.view.1 - self.view.0 < hi - lo - 1e3 => resp.on_hover_text("Drag the box to pan. Double-click to show the whole sweep."),
            Some(Some(OvGrab::Move)) => resp.on_hover_text("The whole sweep is in view: drag the box to move the swept range (it retunes when you let go), or its edges to zoom in."),
            Some(None) => resp.on_hover_text("Click to centre the view here"),
            None => resp,
        };
        if resp.drag_started()
            && let Some(pos) = resp.interact_pointer_pos()
        {
            if self.full_view() && grab_at(pos.x) == Some(OvGrab::Move) {
                self.range_drag = Some(0.0);
            }
            let g = grab_at(pos.x).unwrap_or_else(|| {
                // outside the box: centre it on the pointer, then move it
                let half = (self.view.1 - self.view.0) / 2.0;
                let c = xf(pos.x);
                self.view = (c - half, c + half);
                OvGrab::Move
            });
            self.ov_drag = Some((g, self.view));
        }
        if resp.dragged()
            && let Some((g, _)) = self.ov_drag
        {
            let df = resp.drag_delta().x as f64 / ov.width() as f64 * (hi - lo);
            let min_span = dsp::bin_hz(self.cfg.nfft, self.applied.rate) * 24.0;
            match g {
                OvGrab::Move if self.range_drag.is_some() => self.drag_range(df),
                OvGrab::Move => self.view = (self.view.0 + df, self.view.1 + df),
                OvGrab::Left => self.view.0 = (self.view.0 + df).min(self.view.1 - min_span),
                OvGrab::Right => self.view.1 = (self.view.1 + df).max(self.view.0 + min_span),
            }
            if g == OvGrab::Move {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
        }
        if resp.drag_stopped() {
            self.ov_drag = None;
            self.finish_range_drag();
        }
        if resp.clicked()
            && let Some(pos) = resp.interact_pointer_pos()
            && grab_at(pos.x).is_none()
        {
            let half = (self.view.1 - self.view.0) / 2.0;
            let c = xf(pos.x);
            self.view = (c - half, c + half);
        }
        if resp.double_clicked() {
            self.view = (lo, hi);
        }
        self.clamp_view();
    }

    fn paint_overview(&self, p: &Painter, ov: Rect) {
        p.rect_filled(ov, 2, WELL);
        let (lo, hi) = (self.applied.start_hz, self.applied.stop_hz);
        let fx = |f: f64| ov.left() + ((f - lo) / (hi - lo)) as f32 * ov.width();
        let (t, b) = (self.cfg.spec_top, self.cfg.spec_bot);
        let yv = |db: f32| ov.bottom() - 2.0 - ((db - b) / (t - b).max(1.0)).clamp(0.0, 1.0) * (ov.height() - 4.0);
        if let Some((xs, ys)) = &self.shown.overview {
            let pts: Vec<Option<Pos2>> = xs.iter().zip(ys).map(|(&f, &v)| v.is_finite().then(|| pos2(fx(f), yv(v)))).collect();
            let pc = p.with_clip_rect(ov);
            fill_under(&pc, &pts, ov.bottom(), TRACE.gamma_multiply(0.22), TRACE.gamma_multiply(0.02));
            polyline(&pc, &pts, Stroke::new(1.0, TRACE.gamma_multiply(0.75)));
        }
        let (x0, x1) = (fx(self.view.0).max(ov.left()), fx(self.view.1).min(ov.right()));
        // dim what's outside the view, outline the view box
        p.rect_filled(Rect::from_min_max(ov.min, pos2(x0, ov.bottom())), 2, BG.gamma_multiply(0.55));
        p.rect_filled(Rect::from_min_max(pos2(x1, ov.top()), ov.max), 2, BG.gamma_multiply(0.55));
        let vb = Rect::from_min_max(pos2(x0, ov.top()), pos2(x1, ov.bottom()));
        p.rect_stroke(vb, 2, Stroke::new(1.0, ACCENT.gamma_multiply(0.85)), egui::StrokeKind::Inside);
        for x in [vb.left() + 1.0, vb.right() - 1.0] {
            p.line_segment([pos2(x, vb.center().y - 5.0), pos2(x, vb.center().y + 5.0)], Stroke::new(2.0, ACCENT));
        }
        // the sweep's ends on chips (they sit over the trace), and the box's span while it's dragged
        let end_chip = |x: f32, align: Align2, text: String| {
            let g = p.layout_no_wrap(text, theme::num(theme::XS), INK2);
            let at = align.anchor_size(pos2(x, ov.top() + 2.0), g.size() + vec2(6.0, 2.0));
            p.rect_filled(at, 2, WELL.gamma_multiply(0.85));
            p.galley(at.min + vec2(3.0, 1.0), g, INK2);
        };
        if self.ov_drag.is_some() && self.range_drag.is_none() {
            let dec = label_decimals(self.view.1 - self.view.0);
            let text = format!("{} – {} MHz", fmt_freq(self.view.0, dec), fmt_freq(self.view.1, dec));
            let g = p.layout_no_wrap(text, theme::num(theme::XS), INK);
            let w = g.size().x + 8.0;
            let cx = vb.center().x.clamp(ov.left() + w / 2.0, ov.right() - w / 2.0);
            let at = Rect::from_center_size(pos2(cx, ov.center().y), g.size() + vec2(8.0, 2.0));
            p.rect_filled(at, 2, WELL.gamma_multiply(0.9));
            p.galley(at.min + vec2(4.0, 1.0), g, INK);
        } else {
            end_chip(ov.left() + 2.0, Align2::LEFT_TOP, fmt_freq(lo, 1));
            end_chip(ov.right() - 2.0, Align2::RIGHT_TOP, format!("{} MHz", fmt_freq(hi, 1)));
        }
        p.rect_stroke(ov, 2, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
    }

    /// After a plain drag on the plot: offer Zoom or Sweep for the span (Esc cancels).
    fn selection_menu(&mut self, ui: &mut Ui) {
        let Some((lo, hi, pos)) = self.pending_sel else { return };
        let mut done = false;
        let area = egui::Area::new(ui.id().with("selmenu")).order(egui::Order::Foreground).fixed_pos(pos + vec2(10.0, 10.0)).show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(RichText::new(format!("{} – {} MHz", fmt_freq(lo, 3), fmt_freq(hi, 3))).font(theme::num(theme::SM)).color(INK));
                ui.label(RichText::new(format!("{} MHz wide", fmt_trim((hi - lo) / 1e6, 3))).size(theme::XS + 1.0).color(INK2));
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Zoom in").on_hover_text("Show just this span (Z or Enter)").clicked() {
                        self.view = (lo, hi);
                        done = true;
                    }
                    let can_sweep = hi - lo >= 0.2e6;
                    if ui.add_enabled(can_sweep, egui::Button::new("Sweep this")).on_hover_text("Sweep only this span (S)").on_disabled_hover_text("Spans under 200 kHz can't be swept").clicked() {
                        self.set_range(lo, hi);
                        done = true;
                    }
                    if ui.button("Cancel").on_hover_text("Esc").clicked() {
                        done = true;
                    }
                });
                ui.label(RichText::new("Keys: Z or Enter zoom · S sweep · Esc cancel").size(theme::XS).color(INK2));
                if self.cfg.hints {
                    ui.label(RichText::new("Next time: Ctrl-drag zooms and Shift-drag sweeps without this menu").size(theme::XS).color(INK2));
                }
            });
        });
        let (z, sw, esc) = ui.input(|i| (i.key_pressed(Key::Z) || i.key_pressed(Key::Enter), i.key_pressed(Key::S), i.key_pressed(Key::Escape)));
        if z {
            self.view = (lo, hi);
            done = true;
        } else if sw && hi - lo >= 0.2e6 {
            self.set_range(lo, hi);
            done = true;
        } else if esc {
            done = true;
        }
        // a click anywhere else dismisses it
        if ui.input(|i| i.pointer.any_pressed()) && !area.response.contains_pointer() && !done {
            done = true;
        }
        if done {
            self.pending_sel = None;
            self.selection = None;
        }
    }

    /// Peak labels live in a rail above the plot, joined to their peaks by leaders, so they never
    /// cover the trace (rule O1). Labels that don't fit are counted in a "+N" pick list (rule O5).
    fn peak_rail(&mut self, ui: &mut Ui, rail: Rect, spec: Rect) {
        self.hidden_peaks.clear();
        if self.cfg.labels == 0 || self.labels.is_empty() {
            return;
        }
        let p = ui.painter().clone();
        let dec = label_decimals(self.view.1 - self.view.0);
        let font = theme::num(12.0);
        let more_w = 44.0;
        let mut placed: Vec<Rect> = Vec::new();
        let mut order: Vec<(f64, f32)> = self.labels.clone();
        order.sort_by(|a, b| b.1.total_cmp(&a.1));
        for (f, v) in order {
            let x = self.x_of(spec, f);
            let y = self.y_of(spec, v);
            if x < spec.left() || x > spec.right() {
                continue;
            }
            let g = p.layout_no_wrap(fmt_freq(f, dec), font.clone(), INK);
            let size = vec2(g.size().x + 12.0, rail.height() - 6.0);
            let cx = x.clamp(rail.left() + size.x / 2.0, rail.right() - more_w - size.x / 2.0 - 4.0);
            let r = Rect::from_center_size(pos2(cx, rail.center().y - 1.0), size);
            if placed.iter().any(|o| o.expand2(vec2(3.0, 0.0)).intersects(r)) {
                self.hidden_peaks.push((f, v));
                continue;
            }
            placed.push(r);
            let alpha = self.cfg.data_dim / 100.0;
            p.line_segment([pos2(cx, r.bottom()), pos2(x, spec.top())], egui::Stroke::new(1.0, INK3));
            p.line_segment([pos2(x, spec.top()), pos2(x, (y - 4.0).max(spec.top()))], egui::Stroke::new(1.0, INK3.gamma_multiply(0.6 * alpha)));
            p.circle_filled(pos2(x, y.clamp(spec.top(), spec.bottom())), 2.5, INK.gamma_multiply(alpha));
            p.rect_filled(r, 4, CHIP);
            p.rect_stroke(r, 4, egui::Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
            p.galley(pos2(r.left() + 6.0, r.center().y - g.size().y / 2.0), g, INK);
        }
        if !self.hidden_peaks.is_empty() {
            let r = Rect::from_min_size(pos2(rail.right() - more_w, rail.top() + 2.0), vec2(more_w, rail.height() - 6.0));
            let resp = ui.interact(r, ui.id().with("more-peaks"), Sense::click());
            p.rect_filled(r, 4, if resp.hovered() { theme::BTN_HI } else { CHIP });
            p.rect_stroke(r, 4, egui::Stroke::new(1.0, if resp.hovered() { TOUCH } else { LINE }), egui::StrokeKind::Inside);
            p.text(r.center(), Align2::CENTER_CENTER, format!("+{}", self.hidden_peaks.len()), theme::num(12.0), INK);
            let resp = resp.on_hover_text("Peaks without room for a label");
            let hidden = self.hidden_peaks.clone();
            let mut go = None;
            egui::Popup::from_toggle_button_response(&resp).show(|ui| {
                ui.label(theme::label("More peaks", theme::XS));
                let mut hs = hidden;
                hs.sort_by(|a, b| a.0.total_cmp(&b.0));
                for (f, v) in hs {
                    let text = format!("{} MHz   {:.1} dBFS", fmt_freq(f, dec + 1), v).replace('-', "−");
                    if ui.selectable_label(false, RichText::new(text).font(theme::num(theme::SM))).on_hover_text("Centre the view here").clicked() {
                        go = Some(f);
                    }
                }
            });
            if let Some(f) = go {
                let half = (self.view.1 - self.view.0) / 2.0;
                self.view = (f - half, f + half);
            }
        }
    }

    /// While the pointer is over the spectrum or the flat waterfall: what the mouse can do there, minus
    /// the gestures already used. Sits in the pane's bottom-right corner on a chip (rule O2), and steps
    /// aside if the pointer comes near it.
    fn paint_gesture_hint(&self, p: &Painter, spec: Rect, wf: Option<Rect>) {
        let Some((pos, in_wf)) = self.hover_pos else { return };
        if self.selection.is_some() || self.pending_sel.is_some() {
            return;
        }
        let parts: Vec<&str> = [(Hint::SELECT, "drag to select a span"), (Hint::ZOOM, "scroll to zoom"), (Hint::PAN, "right-drag or drag the axis to pan")]
            .into_iter()
            .filter(|(h, _)| self.cfg.hint(*h))
            .map(|(_, t)| t)
            .collect();
        if parts.is_empty() {
            return;
        }
        let pane = if in_wf { wf.unwrap_or(spec) } else { spec };
        let mut text = parts.join(" · ");
        text[..1].make_ascii_uppercase();
        let g = p.layout_no_wrap(text, theme::ui_font(theme::XS + 1.0), INK2);
        let size = g.size() + vec2(14.0, 8.0);
        let mut r = Rect::from_min_size(pane.right_bottom() - size - vec2(8.0, 8.0), size);
        if r.expand(24.0).contains(pos) {
            r = Rect::from_min_size(pos2(pane.left() + 8.0, pane.bottom() - size.y - 8.0), size);
        }
        p.rect_filled(r, 4, WELL.gamma_multiply(0.88));
        p.rect_stroke(r, 4, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
        p.galley(r.min + vec2(7.0, 4.0), g, INK2);
    }

    /// While a range drag is on: the range it will sweep, on a chip at the top of the plot, and the
    /// moved box on the overview strip.
    fn paint_range_drag(&self, p: &Painter, spec: Rect, ov: Rect) {
        let Some(off) = self.range_drag else { return };
        let (lo, hi) = (self.applied.start_hz + off, self.applied.stop_hz + off);
        let head = p.layout_no_wrap(format!("Sweep {} – {} MHz", fmt_freq(lo, 1), fmt_freq(hi, 1)), theme::num(12.0), INK);
        let sub = p.layout_no_wrap("Let go to retune · Esc cancels".into(), theme::ui_font(theme::XS), INK2);
        let w = head.size().x.max(sub.size().x) + 14.0;
        let r = Rect::from_min_size(pos2(spec.center().x - w / 2.0, spec.top() + 8.0), vec2(w, head.size().y + sub.size().y + 10.0));
        p.rect_filled(r, 4, WELL.gamma_multiply(0.9));
        p.rect_stroke(r, 4, Stroke::new(1.0, ACCENT.gamma_multiply(0.8)), egui::StrokeKind::Inside);
        let hh = head.size().y;
        p.galley(r.min + vec2(7.0, 4.0), head, INK);
        p.galley(r.min + vec2(7.0, 6.0 + hh), sub, INK2);
        if self.cfg.overview && ov.width() > 0.0 {
            let (a, b) = (self.applied.start_hz, self.applied.stop_hz);
            let dx = (off / (b - a)) as f32 * ov.width();
            let moved = Rect::from_min_max(pos2(ov.left() + dx, ov.top()), pos2(ov.right() + dx, ov.bottom())).intersect(ov);
            p.rect_filled(ov, 2, BG.gamma_multiply(0.45));
            p.rect_stroke(moved, 2, Stroke::new(1.5, ACCENT), egui::StrokeKind::Inside);
        }
    }

    /// 3D view mouse: drag turns it, scroll moves closer or further, double-click resets the camera.
    fn interact_3d(&mut self, ui: &mut Ui, wf: Rect) {
        let resp = ui.interact(wf, ui.id().with("view3d"), Sense::click_and_drag());
        if resp.dragged() {
            self.cfg.learned(Hint::ORBIT);
            let d = resp.drag_delta();
            let c = &mut self.cfg.cam3d;
            c.yaw += d.x * 0.008;
            c.pitch = (c.pitch + d.y * 0.006).clamp(0.08, 1.45);
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        } else if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
            let s = ui.input(|i| i.smooth_scroll_delta.y);
            if s != 0.0 {
                self.cfg.cam3d.dist = (self.cfg.cam3d.dist * (-s * 0.002).exp()).clamp(1.3, 8.0);
            }
        }
        if resp.double_clicked() {
            self.cfg.cam3d = crate::view3d::Camera::default();
        }
    }

    /// The 3D waterfall in the waterfall pane: the newest rows of the history for the visible span,
    /// heights and colours by the waterfall levels, with frequency and time marked along the floor.
    fn paint_3d(&mut self, p: &Painter, wf: Rect) {
        use crate::view3d::{Camera, DEPTH, GX_MAX, GZ_MAX};
        p.rect_filled(wf, 0, WELL);
        p.rect_stroke(wf, 0, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
        self.wf.set_levels(self.cfg.palette, self.cfg.wf_lo, self.cfg.wf_hi);
        let gx = (wf.width() as usize).clamp(2, GX_MAX).min(self.wf.cols.max(2));
        let gz = self.wf.filled.min(self.cfg.wf_rows.clamp(8, ROWS)).min(GZ_MAX);
        if gz < 2 {
            p.text(wf.center(), Align2::CENTER_CENTER, "The 3D view fills in as passes arrive", theme::ui_font(theme::SM), INK2);
            return;
        }
        let (c0, c1) = self.applied.cover();
        let u0 = ((self.view.0 - c0) / (c1 - c0)) as f32;
        let u1 = ((self.view.1 - c0) / (c1 - c0)) as f32;
        let key = (self.wf.stamp(), (u0 * 1e5) as i32, (u1 * 1e5) as i32, gx, gz, (self.cfg.wf_lo * 10.0) as i32, self.cfg.smooth3d);
        let heights = (self.v3_key != Some(key)).then(|| {
            self.v3_key = Some(key);
            let mut h = self.wf.grid(u0, u1, gx, gz, self.cfg.wf_lo - 30.0);
            let (sx, sz) = match self.cfg.smooth3d {
                0 => (0.0, 0.0),
                1 => (1.2, 1.0),
                _ => (2.5, 2.0),
            };
            crate::view3d::smooth(&mut h, gx, gz, sx, sz);
            std::sync::Arc::new(h)
        });
        let lut = (self.v3_palette != Some(self.cfg.palette)).then(|| {
            self.v3_palette = Some(self.cfg.palette);
            std::sync::Arc::new(self.wf.palette_rgba())
        });
        let m = self.cfg.cam3d.matrix(wf.width() / wf.height().max(1.0));
        p.add(eframe::egui_wgpu::Callback::new_paint_callback(
            wf,
            crate::view3d::Callback { mvp: m, lo: self.cfg.wf_lo, hi: self.cfg.wf_hi, gx, gz, heights, lut },
        ));
        // floor outline and labels, projected with the same camera
        let pr = |x: f32, y: f32, z: f32| Camera::project(&m, wf, [x, y, z]);
        let pc = p.with_clip_rect(wf);
        let edge = Stroke::new(1.0, INK3.gamma_multiply(0.8));
        for (a, b) in [([-1.0, 0.0, 0.0], [1.0, 0.0, 0.0]), ([-1.0, 0.0, 0.0], [-1.0, 0.0, -DEPTH]), ([1.0, 0.0, 0.0], [1.0, 0.0, -DEPTH]), ([-1.0, 0.0, -DEPTH], [1.0, 0.0, -DEPTH])] {
            if let (Some(pa), Some(pb)) = (pr(a[0], a[1], a[2]), pr(b[0], b[1], b[2])) {
                pc.line_segment([pa, pb], edge);
            }
        }
        let span = self.view.1 - self.view.0;
        let dec = label_decimals(span);
        for (f, major) in freq_ticks(self.view.0, self.view.1) {
            let x = ((f - self.view.0) / span) as f32 * 2.0 - 1.0;
            if let (Some(a), Some(b)) = (pr(x, 0.0, 0.0), pr(x, 0.0, 0.04)) {
                pc.line_segment([a, b], edge);
                if major {
                    pc.text(b + vec2(0.0, 3.0), Align2::CENTER_TOP, fmt_freq(f, dec), theme::num(theme::XS), SCALE_INK);
                }
            }
        }
        let now = self.engine.now();
        for k in 0..5 {
            let i = k * (gz - 1) / 4;
            let t = self.wf.row_time(i);
            if !t.is_finite() {
                continue;
            }
            let z = -(i as f32) / (gz - 1) as f32 * DEPTH;
            if let Some(a) = pr(-1.04, 0.0, z) {
                let txt = if i == 0 { "now".to_string() } else { format!("−{:.0} s", now - t) };
                pc.text(a, Align2::RIGHT_CENTER, txt, theme::num(theme::XS), SCALE_INK);
            }
        }
        let hint = if self.cfg.hint(Hint::ORBIT) { "3D · drag to turn · scroll to zoom · double-click to reset" } else { "3D" };
        pc.text(wf.left_top() + vec2(8.0, 6.0), Align2::LEFT_TOP, hint, theme::ui_font(theme::XS + 1.0), INK2);
    }

    /// The 2D / 3D switch in the waterfall pane's top-right corner (key W).
    fn wf_mode_toggle(&mut self, ui: &mut Ui, wf: Rect) {
        let r = Rect::from_min_size(pos2(wf.right() - 70.0, wf.top() + 6.0), vec2(62.0, 22.0));
        let txt = if self.cfg.wf3d { "2D view" } else { "3D view" };
        let hint = if self.view3d_ok { "Switch the waterfall between flat and 3D (W)" } else { "3D needs the wgpu renderer with a depth buffer" };
        let resp = ui.add_enabled_ui(self.view3d_ok, |ui| ui.put(r, egui::Button::new(RichText::new(txt).size(theme::XS + 1.0)))).inner;
        let resp = if self.view3d_ok { resp.on_hover_text(hint) } else { resp.on_disabled_hover_text(hint) };
        if resp.clicked() && self.view3d_ok {
            self.cfg.wf3d = !self.cfg.wf3d;
        }
    }

    /// The hover readout: frequency and level (over the waterfall, also how long ago that row was).
    /// It follows the cursor, or sits in the plot's corner (Display setting).
    fn paint_readout(&self, p: &Painter, spec: Rect, wf: Rect) {
        let Some((pos, in_wf)) = self.hover_pos else { return };
        let dec = label_decimals(self.view.1 - self.view.0) + 1;
        let (f, db, age) = if in_wf {
            let rows = self.cfg.wf_rows.clamp(8, ROWS);
            let i = (((pos.y - wf.top()) / wf.height()) * rows as f32).floor().max(0.0) as usize;
            let f = self.f_of(wf, pos.x);
            let t = self.wf.row_time(i);
            let age = (i < self.wf.filled && t.is_finite()).then(|| self.engine.now() - t);
            (f, self.wf.value_at(i, f), age)
        } else {
            match self.shown.hover {
                Some((f, db)) => (f, Some(db), None),
                None => (self.f_of(spec, pos.x), None, None),
            }
        };
        let mut parts: Vec<(egui::Galley, f32)> = Vec::new();
        let big = theme::num_medium(15.0);
        let unit = theme::ui_font(12.0);
        parts.push(((*p.layout_no_wrap(fmt_freq(f, dec), big.clone(), INK)).clone(), 3.0));
        parts.push(((*p.layout_no_wrap("MHz".into(), unit.clone(), INK2)).clone(), 14.0));
        if let Some(db) = db {
            parts.push(((*p.layout_no_wrap(format!("{db:.1}").replace('-', "−"), big.clone(), INK)).clone(), 3.0));
            parts.push(((*p.layout_no_wrap("dBFS".into(), unit.clone(), INK2)).clone(), 14.0));
        }
        if let Some(a) = age {
            parts.push(((*p.layout_no_wrap(format!("{a:.1}").replace('-', "−"), big, INK)).clone(), 3.0));
            parts.push(((*p.layout_no_wrap("s ago".into(), unit, INK2)).clone(), 0.0));
        }
        let w: f32 = parts.iter().map(|(g, gap)| g.size().x + gap).sum::<f32>() + 20.0;
        let size = vec2(w, 28.0);
        let area = spec.union(wf);
        let min = if self.cfg.readout_follow {
            let mut m = pos + vec2(16.0, 18.0);
            if m.x + size.x > area.right() - 4.0 {
                m.x = pos.x - 16.0 - size.x;
            }
            if m.y + size.y > area.bottom() - 4.0 {
                m.y = pos.y - 18.0 - size.y;
            }
            pos2(m.x.max(area.left() + 4.0), m.y.max(area.top() + 4.0))
        } else {
            spec.min + vec2(8.0, 8.0)
        };
        let br = Rect::from_min_size(min, size);
        p.rect_filled(br, 5, WELL.gamma_multiply(0.92));
        p.rect_stroke(br, 5, egui::Stroke::new(1.0, TOUCH.gamma_multiply(0.5)), egui::StrokeKind::Inside);
        let mut x = br.left() + 10.0;
        let cy = br.center().y;
        for (g, gap) in parts {
            let gw = g.size().x;
            p.galley(pos2(x, cy - g.size().y / 2.0), std::sync::Arc::new(g), INK);
            x += gw + gap;
        }
        // a level line across the spectrum at the readout's level
        if !in_wf && let Some(db) = db {
            let y = self.y_of(spec, db);
            if y > spec.top() && y < spec.bottom() {
                p.line_segment([pos2(spec.left(), y), pos2(spec.right(), y)], Stroke::new(1.0, TOUCH.gamma_multiply(0.18)));
            }
        }
    }

    /// The view shows the whole swept range (nothing to pan).
    fn full_view(&self) -> bool {
        self.view.1 - self.view.0 >= (self.applied.stop_hz - self.applied.start_hz) * 0.999
    }

    /// Move the swept range by `df` while dragging, within what the radio can tune.
    fn drag_range(&mut self, df: f64) {
        let (lo, hi) = (self.applied.start_hz, self.applied.stop_hz);
        let (fmin, fmax) = self.cfg.rate.f_range();
        let off = (self.range_drag.unwrap_or(0.0) + df).clamp((fmin - lo).min(0.0), (fmax - hi).max(0.0));
        self.range_drag = Some(off);
        self.view = (lo + off, hi + off);
    }

    /// Letting go of a range drag: retune to the moved range (a nudge under one bin does nothing).
    fn finish_range_drag(&mut self) {
        let Some(off) = self.range_drag.take() else { return };
        if off.abs() >= dsp::bin_hz(self.cfg.nfft, self.applied.rate) {
            self.set_range(self.applied.start_hz + off, self.applied.stop_hz + off);
            self.cfg.learned(Hint::PAN);
        } else {
            self.view = (self.applied.start_hz, self.applied.stop_hz);
        }
    }

    fn clamp_view(&mut self) {
        // a range drag previews past the swept range on purpose
        if self.range_drag.is_some() {
            return;
        }
        let (lo, hi) = (self.applied.start_hz, self.applied.stop_hz);
        let min_span = (dsp::bin_hz(self.cfg.nfft, self.applied.rate) * 24.0).min(hi - lo);
        let span = (self.view.1 - self.view.0).clamp(min_span, hi - lo);
        let mut a = self.view.0.max(lo);
        if a + span > hi {
            a = hi - span;
        }
        self.view = (a, a + span);
    }

    fn x_of(&self, r: Rect, f: f64) -> f32 {
        r.left() + ((f - self.view.0) / (self.view.1 - self.view.0)) as f32 * r.width()
    }

    fn f_of(&self, r: Rect, x: f32) -> f64 {
        self.view.0 + ((x - r.left()) / r.width()) as f64 * (self.view.1 - self.view.0)
    }

    fn y_of(&self, r: Rect, db: f32) -> f32 {
        let (t, b) = (self.cfg.spec_top, self.cfg.spec_bot);
        r.top() + (t - db) / (t - b).max(1.0) * r.height()
    }

    /// Plot mouse: scroll zooms, a plain drag selects a span and offers Zoom or Sweep (Shift sweeps it at
    /// once, Ctrl zooms at once), the right or middle button pans, double-click shows the whole sweep.
    /// Dragging the frequency axis pans too.
    fn interact(&mut self, ui: &mut Ui, spec: Rect, wf: Option<Rect>, axis: Rect, gutter: Rect) {
        let (shift, ctrl) = ui.input(|i| (i.modifiers.shift, i.modifiers.command));
        self.hover_x = None;
        self.hover_pos = None;
        for (i, r) in std::iter::once(spec).chain(wf).enumerate() {
            let resp = ui.interact(r, ui.id().with(("plot", i)), Sense::click_and_drag());
            if let Some(p) = resp.hover_pos() {
                self.hover_x = Some(p.x);
                self.hover_pos = Some((p, i == 1));
                let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
                let k = (-scroll.y as f64 * 0.0025).exp() / zoom as f64;
                if k != 1.0 {
                    self.cfg.learned(Hint::ZOOM);
                    let f = self.f_of(r, p.x);
                    let span = (self.view.1 - self.view.0) * k;
                    let a = f - (f - self.view.0) * k;
                    self.view = (a, a + span);
                }
                if scroll.x != 0.0 {
                    self.cfg.learned(Hint::PAN);
                    let df = -scroll.x as f64 / r.width() as f64 * (self.view.1 - self.view.0);
                    self.view = (self.view.0 + df, self.view.1 + df);
                }
            }
            if resp.drag_started_by(PointerButton::Primary)
                && let Some(p) = resp.interact_pointer_pos()
            {
                let f = self.f_of(r, p.x);
                self.pending_sel = None;
                self.selection = Some((f, f));
                self.drag_mode = if shift { DragMode::Sweep } else if ctrl { DragMode::Zoom } else { DragMode::Ask };
            }
            if resp.dragged_by(PointerButton::Primary)
                && let (Some(sel), Some(p)) = (&mut self.selection, resp.interact_pointer_pos())
            {
                sel.1 = self.view.0 + ((p.x - r.left()) / r.width()) as f64 * (self.view.1 - self.view.0);
            }
            if resp.dragged_by(PointerButton::Secondary) || resp.dragged_by(PointerButton::Middle) {
                self.cfg.learned(Hint::PAN);
                let df = -resp.drag_delta().x as f64 / r.width() as f64 * (self.view.1 - self.view.0);
                self.view = (self.view.0 + df, self.view.1 + df);
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }
            if resp.drag_stopped_by(PointerButton::Primary)
                && let Some((a, b)) = self.selection
            {
                let (lo, hi) = (a.min(b), a.max(b));
                let wide_enough = (self.x_of(r, hi) - self.x_of(r, lo)) >= 4.0;
                let pos = resp.interact_pointer_pos().unwrap_or(r.center());
                if wide_enough {
                    self.cfg.learned(Hint::SELECT);
                }
                match (wide_enough, self.drag_mode) {
                    (false, _) => self.selection = None,
                    (true, DragMode::Sweep) => {
                        if hi - lo >= 0.2e6 {
                            self.set_range(lo, hi);
                        } else {
                            self.toast("Spans under 200 kHz can't be swept. Ctrl-drag zooms instead.", false);
                        }
                        self.selection = None;
                    }
                    (true, DragMode::Zoom) => {
                        self.view = (lo, hi);
                        self.selection = None;
                    }
                    (true, DragMode::Ask) => self.pending_sel = Some((lo, hi, pos)),
                }
            }
            if resp.double_clicked() {
                self.view = (self.applied.start_hz, self.applied.stop_hz);
                self.selection = None;
                self.pending_sel = None;
            }
        }
        // the frequency axis pans
        // (zoomed all the way out, it moves the swept range instead, retuning on release)
        let ax = ui.interact(axis, ui.id().with("axis"), Sense::drag());
        if ax.drag_started() && self.full_view() {
            self.range_drag = Some(0.0);
        }
        if ax.dragged() {
            let df = -ax.drag_delta().x as f64 / axis.width() as f64 * (self.view.1 - self.view.0);
            if self.range_drag.is_some() {
                self.drag_range(df);
            } else {
                self.cfg.learned(Hint::PAN);
                self.view = (self.view.0 + df, self.view.1 + df);
            }
        }
        if ax.drag_stopped() {
            self.finish_range_drag();
        }
        if ax.hovered() || ax.dragged() {
            ui.ctx().set_cursor_icon(if ax.dragged() { egui::CursorIcon::Grabbing } else { egui::CursorIcon::Grab });
        }
        if self.range_drag.is_none() {
            ax.on_hover_text(if self.full_view() { "Drag to move the swept range (it retunes when you let go)" } else { "Drag to pan" });
        }
        // dB gutter: drag to shift, scroll to stretch
        let g = ui.interact(gutter, ui.id().with("gutter"), Sense::drag());
        if g.dragged() {
            let ddb = g.drag_delta().y / spec.height() * (self.cfg.spec_top - self.cfg.spec_bot);
            self.cfg.spec_top += ddb;
            self.cfg.spec_bot += ddb;
        }
        if g.hovered() {
            let s = ui.input(|i| i.smooth_scroll_delta.y);
            if s != 0.0 {
                let mid = (self.cfg.spec_top + self.cfg.spec_bot) / 2.0;
                let half = ((self.cfg.spec_top - self.cfg.spec_bot) / 2.0 * (-s * 0.003).exp()).clamp(5.0, 90.0);
                self.cfg.spec_top = mid + half;
                self.cfg.spec_bot = mid - half;
            }
        }
        if g.hovered() || g.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
        }
        g.on_hover_text("Drag to move the dB scale, scroll to stretch it");
        if self.pending_sel.is_none() && ui.input(|i| i.key_pressed(Key::Escape)) {
            self.selection = None;
            // cancel a range drag: back to the swept range, nothing retuned
            if self.range_drag.take().is_some() {
                self.view = (self.applied.start_hz, self.applied.stop_hz);
            }
        }
        self.clamp_view();
    }

    /// Reduce the stitched arrays to what is on screen (one lock).
    fn gather(&mut self, cols: usize, ov_cols: usize) {
        let live = self.engine.live.clone();
        let l = live.lock();
        let (a, b) = self.view;
        let det = self.cfg.detector;
        self.shown.live = l.trace(Trace::Live, a, b, cols, det);
        self.shown.avg = self.cfg.show_avg.then(|| l.trace(Trace::Average, a, b, cols, det));
        self.shown.peak = self.cfg.show_peak.then(|| l.trace(Trace::Peak, a, b, cols, Detector::Peak));
        self.shown.overview = self.cfg.overview.then(|| l.trace(Trace::Live, self.applied.start_hz, self.applied.stop_hz, ov_cols, Detector::Peak));
        let px = (b - a) / cols as f64;
        // peak labels, refreshed a few times a second from the full-resolution data
        if self.labels_t.elapsed() > Duration::from_millis(300) {
            self.labels_t = Instant::now();
            let (xs, ys) = if self.cfg.show_avg { self.shown.avg.clone().unwrap_or_default() } else { self.shown.live.clone() };
            self.labels = find_peaks(&xs, &ys, self.cfg.labels, (b - a) / 30.0)
                .into_iter()
                .filter_map(|(f, _)| max_in(&l, Trace::Live, f - px * 2.0, f + px * 2.0))
                .collect();
        }
    }

    fn paint_spectrum(&mut self, p: &Painter, r: Rect) {
        // a flat black well: the chrome never tints the data (rule C10)
        p.rect_filled(r, 0, WELL);
        let p = p.with_clip_rect(r);
        let grid = self.cfg.grid_dim / 100.0;
        let data = self.cfg.data_dim / 100.0;
        let (t, b) = (self.cfg.spec_top, self.cfg.spec_bot);
        let db_step = if t - b > 60.0 { 10.0 } else { 5.0 };
        let mut db = (b / db_step).ceil() * db_step;
        while db <= t {
            let y = self.y_of(r, db);
            let major = (db as i32) % 20 == 0;
            p.line_segment([pos2(r.left(), y), pos2(r.right(), y)], Stroke::new(1.0, GRATICULE.gamma_multiply(grid * if major { 1.0 } else { 0.6 })));
            db += db_step;
        }
        for (f, major) in freq_ticks(self.view.0, self.view.1) {
            let x = self.x_of(r, f);
            p.line_segment([pos2(x, r.top()), pos2(x, r.bottom())], Stroke::new(1.0, GRATICULE.gamma_multiply(grid * if major { 1.0 } else { 0.6 })));
        }
        // sweep beam: steps brighten faintly as the firmware visits them (only when slow enough to follow)
        let now = self.engine.now();
        let plan = &self.applied;
        if self.cfg.beam && self.followable && !self.step_t.is_empty() {
            let lo = plan.f0 as f64 - plan.window_hz / 2.0;
            for (i, &st) in self.step_t.iter().enumerate() {
                let k = (-(now - st) / 0.14).exp();
                if k < 0.03 {
                    continue;
                }
                let x0 = self.x_of(r, lo + i as f64 * plan.step_hz as f64);
                let x1 = self.x_of(r, lo + i as f64 * plan.step_hz as f64 + plan.window_hz);
                if x1 < r.left() || x0 > r.right() {
                    continue;
                }
                let rr = Rect::from_min_max(pos2(x0, r.top()), pos2(x1.max(x0 + 1.0), r.bottom()));
                gradient_rect(&p, rr, INK.gamma_multiply(0.0), INK.gamma_multiply(0.07 * k * data));
            }
        }
        // VCO divider switch points (named on the strip below, not over the data)
        if self.cfg.vco_marks {
            for &f in &self.switches {
                if f < self.view.0 || f > self.view.1 {
                    continue;
                }
                let x = self.x_of(r, f);
                p.extend(Shape::dashed_line(&[pos2(x, r.top()), pos2(x, r.bottom())], Stroke::new(1.0, INK3.gamma_multiply(0.7 * grid.max(0.4))), 4.0, 4.0));
            }
        }
        // traces: identity colours only, no glow
        let map = |xs: &[f64], ys: &[f32]| -> Vec<Option<Pos2>> {
            xs.iter().zip(ys).map(|(&f, &v)| v.is_finite().then(|| pos2(self.x_of(r, f), self.y_of(r, v).clamp(r.top() - 2.0, r.bottom() + 2.0)))).collect()
        };
        let live = map(&self.shown.live.0, &self.shown.live.1);
        fill_under(&p, &live, r.bottom(), TRACE.gamma_multiply(0.16 * data), TRACE.gamma_multiply(0.0));
        if let Some((xs, ys)) = &self.shown.peak {
            polyline(&p, &map(xs, ys), Stroke::new(1.1, PEAK.gamma_multiply(0.85 * data)));
        }
        polyline(&p, &live, Stroke::new(1.25, TRACE.gamma_multiply(data)));
        if let Some((xs, ys)) = &self.shown.avg {
            polyline(&p, &map(xs, ys), Stroke::new(1.5, AVERAGE.gamma_multiply(data)));
        }
        // selection: the one place the accent appears on the plot
        if let Some((a, b)) = self.selection {
            let (x0, x1) = (self.x_of(r, a.min(b)), self.x_of(r, a.max(b)));
            let sr = Rect::from_min_max(pos2(x0, r.top()), pos2(x1, r.bottom()));
            p.rect_filled(sr, 0, ACCENT.gamma_multiply(0.10));
            p.rect_stroke(sr, 0, Stroke::new(1.0, ACCENT.gamma_multiply(0.8)), egui::StrokeKind::Inside);
            let (lo, hi) = (a.min(b), a.max(b));
            let span = format!("{} – {} MHz", fmt_freq(lo, 3), fmt_freq(hi, 3));
            let (head, sub) = match self.drag_mode {
                DragMode::Sweep if hi - lo < 0.2e6 => (span, "Too narrow to sweep (200 kHz minimum)"),
                DragMode::Sweep => (format!("Sweep {span}"), ""),
                DragMode::Zoom => (format!("Zoom to {span}"), ""),
                DragMode::Ask if self.pending_sel.is_some() => (span, ""),
                DragMode::Ask => (span, if self.cfg.hints { "Let go to zoom or sweep · hold Shift to sweep, Ctrl to zoom" } else { "" }),
            };
            let g = p.layout_no_wrap(head, theme::num(12.0), INK);
            let gs = (!sub.is_empty()).then(|| p.layout_no_wrap(sub.to_string(), theme::ui_font(theme::XS), INK2));
            let w = g.size().x.max(gs.as_ref().map_or(0.0, |g| g.size().x)) + 14.0;
            let h = g.size().y + gs.as_ref().map_or(0.0, |g| g.size().y + 2.0) + 8.0;
            let cx = ((x0 + x1) / 2.0).clamp(r.left() + w / 2.0 + 2.0, r.right() - w / 2.0 - 2.0);
            let cr = Rect::from_min_size(pos2(cx - w / 2.0, r.top() + 8.0), vec2(w, h));
            p.rect_filled(cr, 4, WELL.gamma_multiply(0.9));
            let gh = g.size().y;
            p.galley(cr.min + vec2(7.0, 4.0), g, INK);
            if let Some(gs) = gs {
                p.galley(cr.min + vec2(7.0, 6.0 + gh), gs, INK2);
            }
        }
        // hover cursor
        if let Some(x) = self.hover_x
            && x >= r.left()
            && x <= r.right()
        {
            let f = self.f_of(r, x);
            let px = (self.view.1 - self.view.0) / r.width() as f64;
            let live = self.engine.live.clone();
            let found = max_in(&live.lock(), Trace::Live, f - 2.0 * px, f + 2.0 * px);
            self.shown.hover = found;
            p.line_segment([pos2(x, r.top()), pos2(x, r.bottom())], Stroke::new(1.0, TOUCH.gamma_multiply(0.35)));
            if let Some((hf, v)) = found {
                p.circle_stroke(pos2(self.x_of(r, hf), self.y_of(r, v)), 4.0, Stroke::new(1.5, TOUCH));
            }
        } else {
            self.shown.hover = None;
        }
    }

    fn paint_axis(&self, p: &Painter, r: Rect) {
        let p = p.with_clip_rect(r.expand2(vec2(40.0, 0.0)));
        let ticks = freq_ticks(self.view.0, self.view.1);
        let step = ticks.windows(2).map(|w| w[1].0 - w[0].0).fold(f64::INFINITY, f64::min);
        let dec = if step.is_finite() { ((-(step / 1e6).log10()).ceil().max(0.0)) as usize } else { 1 };
        for (f, major) in ticks {
            if !major {
                continue;
            }
            let x = self.x_of(r, f);
            let anchor = if x > r.right() - 18.0 { Align2::RIGHT_TOP } else { Align2::CENTER_TOP };
            p.text(pos2(x.min(r.right()), r.top() + 4.0), anchor, fmt_freq(f, dec), theme::num(theme::XS), SCALE_INK);
        }
        p.text(pos2(r.left() - 8.0, r.top() + 4.0), Align2::RIGHT_TOP, "MHz", theme::ui_font(theme::XS + 1.0), INK2);
    }

    /// The sweep strip: tuner VCO divider bands, step cells, and the sweep head lighting them up.
    fn paint_strip(&self, p: &Painter, r: Rect) {
        let p = p.with_clip_rect(r);
        p.rect_filled(r, 4, PANEL);
        let plan = &self.applied;
        let lo = plan.f0 as f64 - plan.window_hz / 2.0;
        let now = self.engine.now();
        let n = plan.n();
        let mut labels = Vec::new();
        let mut i = 0;
        let mut band = 0;
        while i < n {
            let d = plan.divider(i);
            let mut j = i;
            while j + 1 < n && plan.divider(j + 1) == d {
                j += 1;
            }
            let x0 = self.x_of(r, lo + i as f64 * plan.step_hz as f64).max(r.left());
            let x1 = self.x_of(r, lo + j as f64 * plan.step_hz as f64 + plan.window_hz).min(r.right());
            if x1 > x0 {
                let br = Rect::from_min_max(pos2(x0, r.top()), pos2(x1, r.bottom()));
                p.rect_filled(br, 0, if band % 2 == 0 { CHIP } else { PANEL });
                if band > 0 {
                    p.line_segment([pos2(x0, r.top()), pos2(x0, r.bottom())], Stroke::new(1.0, INK3));
                }
                if x1 - x0 > 64.0 && !plan.rate.complex() {
                    labels.push((x0, format!("VCO ÷{}", 1u32 << (d + 1))));
                }
            }
            band += 1;
            i = j + 1;
        }
        let cell_w = (self.x_of(r, lo + plan.step_hz as f64) - self.x_of(r, lo)).abs();
        let data = self.cfg.data_dim / 100.0;
        // a step counts as fresh while it keeps arriving; VCO order revisits run ends up to 2 passes apart
        let fresh_s = (3.0 * self.pass_s).max(0.3);
        for (i, &st) in self.step_t.iter().enumerate() {
            let k = if self.followable { (-(now - st) / 0.07).exp() } else { 0.0 };
            let x0 = self.x_of(r, lo + i as f64 * plan.step_hz as f64);
            let x1 = self.x_of(r, lo + i as f64 * plan.step_hz as f64 + plan.window_hz);
            if x1 < r.left() || x0 > r.right() {
                continue;
            }
            if st.is_finite() {
                p.rect_filled(Rect::from_min_max(pos2(x0 + 0.5, r.bottom() - 4.0), pos2((x1 - 0.5).max(x0 + 1.0), r.bottom() - 2.0)), 0, INK3.gamma_multiply(0.6));
            }
            if k > 0.02 {
                p.rect_filled(Rect::from_min_max(pos2(x0 + 0.5, r.top() + 2.0), pos2((x1 - 0.5).max(x0 + 1.5), r.bottom() - 2.0)), 1, INK.gamma_multiply(0.75 * k * data));
            } else if !self.followable && st.is_finite() {
                // steady: lit while the step keeps arriving, fading out if it stops
                let f = (2.0 - (now - st) / fresh_s).clamp(0.0, 1.0);
                if f > 0.02 {
                    p.rect_filled(Rect::from_min_max(pos2(x0 + 0.5, r.top() + 2.0), pos2((x1 - 0.5).max(x0 + 1.5), r.bottom() - 2.0)), 1, INK.gamma_multiply(0.2 * f * data));
                }
            }
            if cell_w > 8.0 {
                p.line_segment([pos2(x0, r.top() + 2.0), pos2(x0, r.top() + 5.0)], Stroke::new(1.0, INK3.gamma_multiply(0.5)));
            }
        }
        for (x, text) in labels {
            let g = p.layout_no_wrap(text, theme::num(theme::XS), INK2);
            let br = Rect::from_min_size(pos2(x + 5.0, r.center().y - g.size().y / 2.0 - 1.0), g.size() + vec2(8.0, 2.0));
            p.rect_filled(br, 3, WELL.gamma_multiply(0.85));
            p.galley(br.min + vec2(4.0, 1.0), g, INK2);
        }
        if let Some(h) = self.head.filter(|_| self.followable) {
            let x = self.x_of(r, plan.centre(h));
            p.line_segment([pos2(x, r.top()), pos2(x, r.bottom())], Stroke::new(1.5, TOUCH.gamma_multiply(0.8)));
        }
        p.rect_stroke(r, 4, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
    }

    fn paint_waterfall(&mut self, ctx: &egui::Context, p: &Painter, r: Rect, gutter: Rect) {
        p.rect_filled(r, 0, WELL);
        self.wf.set_levels(self.cfg.palette, self.cfg.wf_lo, self.cfg.wf_hi);
        let (c0, c1) = self.applied.cover();
        let (mut u0, mut u1) = (((self.view.0 - c0) / (c1 - c0)) as f32, ((self.view.1 - c0) / (c1 - c0)) as f32);
        let rows = self.cfg.wf_rows.clamp(8, ROWS);
        // only the part of the view the history covers (a range drag looks past it)
        let x_at = |u: f32| r.left() + (u - u0) / (u1 - u0) * r.width();
        let (xl, xr) = (x_at(0.0).max(r.left()), x_at(1.0).min(r.right()));
        if xr > xl {
            let sub = Rect::from_min_max(pos2(xl, r.top()), pos2(xr, r.bottom()));
            (u0, u1) = (u0.max(0.0), u1.min(1.0));
            self.wf.paint(ctx, &p.with_clip_rect(r), sub, u0, u1, rows);
        }
        p.rect_stroke(r, 0, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
        let now = self.engine.now();
        let row_y = |i: usize| r.top() + (i as f32 + 0.5) / rows as f32 * r.height();
        // retune lines: rows on either side of a receiver change aren't comparable (rule O13)
        let oldest = self.wf.row_time(self.wf.filled.min(rows).saturating_sub(1));
        self.retunes.retain(|&t| !oldest.is_finite() || t >= oldest - 1.0 || now - t < 5.0);
        let mut retune_y = Vec::new();
        for &te in &self.retunes {
            if let Some(i) = (0..self.wf.filled.min(rows)).find(|&i| self.wf.row_time(i) < te) {
                let y = row_y(i).floor() - 0.5;
                p.line_segment([pos2(r.left(), y), pos2(r.right(), y)], Stroke::new(1.0, INK2.gamma_multiply(0.7)));
                p.text(pos2(gutter.right(), y), Align2::RIGHT_CENTER, "retune", theme::ui_font(theme::XS), INK2);
                retune_y.push(y);
            }
        }
        // mode changes: a hard break (a gap across the history) with what changed. Labels of breaks
        // close together sit side by side, newest on the left.
        self.breaks.retain(|(t, _)| !oldest.is_finite() || *t >= oldest - 1.0 || now - t < 5.0);
        let mut last_chip = Rect::NOTHING;
        for (te, what) in self.breaks.iter().rev() {
            let Some(i) = (0..self.wf.filled.min(rows)).find(|&i| self.wf.row_time(i) < *te) else { continue };
            let y = r.top() + i as f32 / rows as f32 * r.height();
            p.rect_filled(Rect::from_min_max(pos2(r.left(), y - 1.5), pos2(r.right(), y + 1.5)), 0, theme::BG);
            p.line_segment([pos2(r.left(), y), pos2(r.right(), y)], Stroke::new(1.0, INK3));
            retune_y.push(y);
            let g = p.layout_no_wrap(what.clone(), theme::num(theme::XS), INK);
            let mut chip = Rect::from_min_size(pos2(r.left() + 6.0, y - g.size().y / 2.0 - 2.0), g.size() + vec2(10.0, 4.0));
            if chip.top() < last_chip.bottom() + 2.0 {
                chip = chip.translate(vec2(last_chip.right() + 6.0 - chip.left(), 0.0));
            }
            if chip.right() < r.right() - 4.0 {
                p.rect_filled(chip, 3, theme::PANEL);
                p.rect_stroke(chip, 3, Stroke::new(1.0, theme::EDGE), egui::StrokeKind::Inside);
                p.galley(chip.min + vec2(5.0, 2.0), g, INK);
                last_chip = chip;
            }
        }
        // time: relative ticks ≥ 48 px apart, the clock time at the top
        let step = 56.0;
        let mut y = r.top() + step;
        while y < r.bottom() - 8.0 {
            let i = ((y - r.top()) / r.height() * rows as f32) as usize;
            let t = self.wf.row_time(i);
            if i < self.wf.filled && t.is_finite() && retune_y.iter().all(|&ry| (ry - y).abs() > 14.0) {
                p.text(pos2(gutter.right(), y), Align2::RIGHT_CENTER, format!("−{:.0} s", now - t), theme::num(theme::XS), SCALE_INK);
                p.line_segment([pos2(r.left(), y), pos2(r.left() + 5.0, y)], Stroke::new(1.0, SCALE_INK));
            }
            y += step;
        }
        p.text(pos2(gutter.right(), r.top() + 2.0), Align2::RIGHT_TOP, local_hms(), theme::num(theme::XS), INK2);
    }

    fn paint_overlays(&self, p: &Painter, spec: Rect, wf: Rect, three: bool) {
        // dB scale in the gutter
        let (t, b) = (self.cfg.spec_top, self.cfg.spec_bot);
        let db_step = if t - b > 60.0 { 10.0 } else { 5.0 };
        let mut db = (b / db_step).ceil() * db_step;
        while db <= t {
            let y = self.y_of(spec, db);
            if y > spec.top() + 24.0 && y < spec.bottom() - 4.0 {
                p.text(pos2(spec.left() - 6.0, y), Align2::RIGHT_CENTER, format!("{db:.0}").replace('-', "−"), theme::num(theme::XS), SCALE_INK);
            }
            db += db_step;
        }
        p.text(pos2(spec.left() - 6.0, spec.top() + 2.0), Align2::RIGHT_TOP, "dBFS", theme::ui_font(theme::XS + 1.0), INK2);
        p.rect_stroke(spec, 0, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
        // spectrum and waterfall share one cursor and one selection (rule L7)
        if three {
            return;
        }
        if let Some(x) = self.hover_x
            && x >= wf.left()
            && x <= wf.right()
        {
            p.line_segment([pos2(x, wf.top()), pos2(x, wf.bottom())], Stroke::new(1.0, TOUCH.gamma_multiply(0.3)));
        }
        if let Some((a, b)) = self.selection {
            let (x0, x1) = (self.x_of(wf, a.min(b)), self.x_of(wf, a.max(b)));
            p.rect_filled(Rect::from_min_max(pos2(x0, wf.top()), pos2(x1, wf.bottom())), 0, ACCENT.gamma_multiply(0.08));
        }
    }

    fn offline_banner(&mut self, ui: &mut Ui, spec: Rect, err: &str) {
        let r = Rect::from_center_size(spec.center(), vec2(spec.width().min(620.0), 150.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(r));
        theme::card(&mut child, |ui| {
            ui.label(RichText::new(format!("The {} isn't open", crate::hydra::board().name())).size(theme::LG).color(INK));
            ui.label(RichText::new("Check that it's plugged in and that no other program is using it, then try again.").size(theme::SM).color(INK));
            if !err.is_empty() {
                ui.label(RichText::new(err).size(theme::XS + 1.0).color(INK2));
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button("Try again").clicked() {
                    self.retry_device();
                }
                if ui.button("Run the simulator").clicked() {
                    self.use_sim();
                }
            });
        });
    }



    fn export_csv(&mut self) {
        let path = captures_dir().join(format!("corvus-sweep-{}.csv", stamp()));
        let out = self.engine.live.lock().to_csv();
        let res = std::fs::create_dir_all(path.parent().unwrap()).and_then(|_| std::fs::write(&path, out));
        match res {
            Ok(()) => self.toast(format!("Saved {}", path.display()), false),
            Err(e) => self.toast(format!("CSV export failed: {e}"), true),
        }
    }

    fn handle_screenshots(&mut self, ui: &mut Ui) {
        if let Some((path, after)) = &self.screenshot
            && self.started.elapsed().as_secs_f32() >= *after
            && self.shot_requested.is_none()
        {
            self.shot_requested = Some((path.clone(), true));
            self.screenshot = None;
            log::info!("requesting a screenshot");
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let img = ui.input(|i| {
            i.raw.events.iter().find_map(|e| if let egui::Event::Screenshot { image, .. } = e { Some(image.clone()) } else { None })
        });
        if let (Some(img), Some((path, exit))) = (img, self.shot_requested.take()) {
            match save_png(&path, &img) {
                Ok(()) => {
                    log::info!("screenshot saved to {}", path.display());
                    self.toast(format!("Saved {}", path.display()), false);
                }
                Err(e) => self.toast(format!("Screenshot failed: {e}"), true),
            }
            if exit {
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    fn snapshot(&mut self, ctx: &egui::Context) {
        let path = captures_dir().join(format!("corvus-sweep-{}.png", stamp()));
        ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        self.shot_requested = Some((path, false));
    }

    /// Zoom the view about its centre (k < 1 zooms in).
    fn zoom_view(&mut self, k: f64) {
        let mid = (self.view.0 + self.view.1) / 2.0;
        let half = (self.view.1 - self.view.0) / 2.0 * k;
        self.view = (mid - half, mid + half);
    }

    fn pan_view(&mut self, frac: f64) {
        let df = (self.view.1 - self.view.0) * frac;
        self.view = (self.view.0 + df, self.view.1 + df);
    }

    /// Move the swept range by its own width, stopping at the band edges.
    fn shift_range(&mut self, dir: f64) {
        if self.cfg.live {
            let c = self.cfg.live_centre_mhz() + dir * self.cfg.live_span_mhz;
            self.cfg.set_live(c);
            return;
        }
        let (a, b) = (self.applied.start_hz, self.applied.stop_hz);
        let span = b - a;
        let (fmin, fmax) = self.cfg.rate.f_range();
        let lo = (a + dir * span).clamp(fmin, fmax - span);
        self.set_range(lo, lo + span);
    }

    fn keys(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        // a text field, the Zoom/Sweep menu or the radio picker has the keys
        if ctx.egui_wants_keyboard_input() || self.pending_sel.is_some() || self.picker.is_some() {
            return;
        }
        let pressed: Vec<(Key, Modifiers)> = ctx.input(|i| {
            i.events
                .iter()
                .filter_map(|e| match e {
                    egui::Event::Key { key, pressed: true, modifiers, .. } => Some((*key, *modifiers)),
                    _ => None,
                })
                .collect()
        });
        for (key, m) in pressed {
            if m.command {
                match key {
                    Key::E => self.export_csv(),
                    Key::S => self.snapshot(&ctx),
                    _ => {}
                }
                continue;
            }
            if m.alt {
                continue;
            }
            match key {
                Key::Space => self.toggle_run(),
                Key::F1 | Key::Questionmark => self.show_keys = !self.show_keys,
                Key::F2 | Key::F3 | Key::F4 | Key::F5 => self.apply_profile([Key::F2, Key::F3, Key::F4, Key::F5].iter().position(|&k| k == key).unwrap()),
                Key::Slash if m.shift => self.show_keys = !self.show_keys,
                Key::Escape => self.show_keys = false,
                // range
                Key::G => self.focus_start = true,
                Key::L if crate::hydra::board().has_live() => {
                    if self.cfg.live {
                        self.cfg.leave_live();
                        self.toast("Sweep", false);
                    } else {
                        // live where the cursor is on the spectrum, else in the middle of the view
                        let c = self.shown.hover.map_or((self.view.0 + self.view.1) / 2.0, |h| h.0) / 1e6;
                        self.cfg.set_live(c);
                        self.toast(format!("Live at {} MHz, {} MHz span", fmt_trim(self.cfg.live_centre_mhz(), 3), fmt_trim(self.cfg.live_span_mhz, 1)), false);
                    }
                }
                Key::Backspace => self.back_range(),
                Key::F => {
                    let (a, b) = self.cfg.rate.f_range();
                    self.set_range(a, b)
                }
                Key::S => {
                    let (a, b) = self.view;
                    if b - a < self.applied.stop_hz - self.applied.start_hz - 1e3 {
                        self.set_range(a, b);
                    }
                }
                Key::ArrowLeft | Key::ArrowRight if m.shift => self.shift_range(if key == Key::ArrowLeft { -1.0 } else { 1.0 }),
                Key::OpenBracket | Key::CloseBracket => {
                    self.cfg.bump_step(if key == Key::OpenBracket { -1 } else { 1 });
                    self.toast(format!("Step {} MHz", fmt_trim(self.cfg.step_mhz, 1)), false);
                }
                Key::M if crate::hydra::board().rates().len() > 1 => {
                    let rates = crate::hydra::board().rates();
                    let i = rates.iter().position(|&r| r == self.cfg.rate).unwrap_or(0);
                    self.cfg.set_rate(rates[(i + 1) % rates.len()]);
                    self.toast(format!("{}, {} MHz steps", self.cfg.rate.label(), fmt_trim(self.cfg.step_mhz, 1)), false);
                }
                Key::O => {
                    self.cfg.order = if self.cfg.order == Order::VcoContinuous { Order::Ascending } else { Order::VcoContinuous };
                    self.toast(if self.cfg.order == Order::VcoContinuous { "VCO-continuous order" } else { "Ascending order" }, false);
                }
                // view
                Key::ArrowLeft => self.pan_view(-0.2),
                Key::ArrowRight => self.pan_view(0.2),
                Key::Plus | Key::Equals => self.zoom_view(1.0 / 1.5),
                Key::Minus => self.zoom_view(1.5),
                Key::Num0 | Key::Home => self.view = (self.applied.start_hz, self.applied.stop_hz),
                Key::ArrowUp | Key::ArrowDown => {
                    let d = if key == Key::ArrowUp { -5.0 } else { 5.0 };
                    self.cfg.spec_top += d;
                    self.cfg.spec_bot += d;
                }
                // display
                Key::A => self.auto_levels(),
                Key::P => self.engine.clear_peak(),
                Key::H => self.cfg.show_peak = !self.cfg.show_peak,
                Key::V => self.cfg.show_avg = !self.cfg.show_avg,
                Key::E => {
                    self.cfg.eq_on = !self.cfg.eq_on;
                    self.toast(if self.cfg.eq_on { "Flatten passband on" } else { "Flatten passband off" }, false);
                }
                Key::B => self.cfg.beam = !self.cfg.beam,
                Key::W if self.view3d_ok => self.cfg.wf3d = !self.cfg.wf3d,
                _ => {
                    let digits = [Key::Num1, Key::Num2, Key::Num3, Key::Num4, Key::Num5, Key::Num6, Key::Num7, Key::Num8, Key::Num9];
                    if let Some(pr) = digits.iter().position(|&d| d == key).and_then(|i| PRESETS.get(i)) {
                        self.set_range(pr.start_mhz * 1e6, pr.stop_mhz * 1e6);
                    }
                }
            }
        }
    }

    fn keys_window(&mut self, ui: &mut Ui) {
        let mut open = self.show_keys;
        egui::Window::new(RichText::new("Keyboard shortcuts").size(theme::MD).color(INK))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ui.ctx(), |ui| {
                ui.horizontal_top(|ui| {
                    for col in KEYS {
                        ui.vertical(|ui| {
                            for (section, rows) in *col {
                                ui.label(theme::label(section, theme::SM));
                                egui::Grid::new(section).num_columns(2).min_col_width(96.0).spacing(vec2(14.0, 4.0)).show(ui, |ui| {
                                    for (k, what) in *rows {
                                        ui.label(RichText::new(*k).font(theme::num_medium(theme::SM)).color(INK));
                                        ui.label(RichText::new(*what).size(theme::SM).color(INK2));
                                        ui.end_row();
                                    }
                                });
                                ui.add_space(10.0);
                            }
                        });
                        ui.add_space(18.0);
                    }
                });
                theme::caption(ui, "Keys are ignored while a text field has the cursor. Every key has a button or control too. ? or Esc closes this.");
            });
        self.show_keys = open && self.show_keys;
    }
}

type KeyRows = &'static [(&'static str, &'static str)];

/// The shortcut sheet, in two columns.
const KEYS: &[&[(&str, KeyRows)]] = &[
    &[
        (
            "Sweep",
            &[
                ("Space", "Run / pause"),
                ("G", "Type a range: Start, Tab, Stop, Enter"),
                ("1 – 9", "Band presets, in list order"),
                ("F", "Full range"),
                ("Backspace", "Back to the previous range"),
                ("Shift ← →", "Move the range by its own width"),
                ("S", "Sweep the visible area"),
                ("[  ]", "Smaller / larger step"),
                ("M", "Sample rate 10 / 12 MSPS"),
                ("O", "VCO-continuous / ascending order"),
            ],
        ),
        ("Profiles", &[("F2", "Fastest"), ("F3", "Quality"), ("F4", "Weak signals"), ("F5", "Bursts")]),
        ("Files", &[("Ctrl E", "Export CSV"), ("Ctrl S", "Snapshot PNG")]),
    ],
    &[
        (
            "View",
            &[
                ("← →", "Pan"),
                ("+  −", "Zoom in / out"),
                ("0  Home", "Reset zoom"),
                ("↑ ↓", "Move the dB scale"),
                ("Drag", "Select a span, then Zoom or Sweep"),
                ("Shift-drag", "Sweep the span at once"),
                ("Ctrl-drag", "Zoom to the span at once"),
                ("Right-drag", "Pan (or drag the axis)"),
                ("Drag the axis, zoomed out", "Move the swept range"),
                ("Scroll", "Zoom at the pointer"),
                ("Swipe sideways", "Pan (touchpad)"),
                ("Double-click", "Show the whole sweep"),
                ("Esc", "Cancel selection"),
                ("Drag the grip", "Resize spectrum / waterfall"),
                ("3D: drag", "Turn (scroll zooms, double-click resets)"),
            ],
        ),
        (
            "Display",
            &[
                ("A", "Auto levels"),
                ("P", "Clear peak hold"),
                ("H", "Peak trace on / off"),
                ("V", "Average on / off"),
                ("E", "Flatten passband on / off"),
                ("B", "Sweep beam on / off"),
                ("W", "Waterfall flat / 3D"),
            ],
        ),
    ],
];

fn freq_field(v: &mut f64, range: std::ops::RangeInclusive<f64>) -> egui::DragValue<'_> {
    egui::DragValue::new(v).speed(0.25).range(range).max_decimals(3).update_while_editing(false)
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let frame_start = Instant::now();
        let before = self.cfg.clone();
        if self.styled != Some((self.cfg.density, self.cfg.text_scale)) {
            theme::apply(ui.ctx(), self.cfg.density);
            ui.ctx().set_zoom_factor(self.cfg.text_scale);
            self.styled = Some((self.cfg.density, self.cfg.text_scale));
        }
        let t = self.started.elapsed().as_secs_f32();
        self.frame_times.push_back(t);
        while self.frame_times.len() > 60 {
            self.frame_times.pop_front();
        }
        self.sync_engine();
        let bad = self.stats.torn + self.stats.gaps + self.stats.misfiled + self.stats.lock_timeouts + self.stats.host_drops;
        if bad > self.bad_last.0 {
            self.bad_last = (bad, self.engine.now());
        } else if bad < self.bad_last.0 {
            self.bad_last.0 = bad;
        }
        // A moving sweep head only reads as motion when a pass takes about a second or more. Faster,
        // it lands somewhere new every frame and strobes, so it gives way to a steady freshness view.
        let steps = self.applied.n() as f32;
        self.pass_s = if self.stats.steps_per_s > 0.0 { steps / self.stats.steps_per_s } else { f32::INFINITY };
        self.followable = self.pass_s > if self.followable { 0.8 } else { 1.2 };
        self.keys(ui);
        // the frame (rules/signature.md §3): top bar, status bar under it, then the instrument
        egui::Panel::top("top").frame(theme::top_bar_frame()).exact_size(46.0).show(ui, |ui| self.top_bar(ui));
        egui::Panel::top("status").frame(theme::status_frame()).show(ui, |ui| self.status_bar(ui));
        egui::Panel::top("hud").frame(egui::Frame::new().fill(theme::BG).inner_margin(egui::Margin { left: 10, right: 10, top: 10, bottom: 0 })).exact_size(84.0).show(ui, |ui| self.hud(ui));
        egui::Panel::bottom("footer").frame(theme::status_frame().inner_margin(egui::Margin::symmetric(14, 2))).exact_size(24.0).show(ui, |ui| self.footer(ui));
        egui::Panel::right("controls").frame(theme::side_frame()).exact_size(392.0).resizable(false).show(ui, |ui| self.sidebar(ui));
        egui::CentralPanel::default().frame(theme::central_frame()).show(ui, |ui| self.central(ui));
        self.keys_window(ui);
        self.device_picker(ui);
        self.handle_screenshots(ui);
        if crate::QUIT.load(std::sync::atomic::Ordering::Relaxed) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }

        if self.auto_pending && self.stats.passes >= 2 {
            self.auto_pending = false;
            self.auto_levels();
        }
        // apply changes: the sweep plan and receiver settings debounced (drag values fire every frame)
        let now = Instant::now();
        let c = &self.cfg;
        if c.plan() != before.plan() || c.plan() != self.applied {
            self.plan_dirty.get_or_insert(now);
        }
        if c.rx() != before.rx() || c.rx() != self.applied_rx {
            self.rx_dirty.get_or_insert(now);
        }
        if c.dsp() != before.dsp() {
            self.engine.set_settings(c.dsp());
            // a new bin width re-lays out the engine, so it shows as a break, not a retune
            if c.eq_on != before.eq_on {
                self.retunes.push(self.engine.now());
            }
        }
        // held while a field has the keyboard, so typing start then stop applies once
        if self.plan_dirty.is_some_and(|t| t.elapsed() > Duration::from_millis(250)) && !ui.ctx().egui_is_using_pointer() && !ui.ctx().egui_wants_keyboard_input() {
            self.plan_dirty = None;
            self.apply_plan();
        }
        if self.rx_dirty.is_some_and(|t| t.elapsed() > Duration::from_millis(150)) {
            self.rx_dirty = None;
            self.apply_rx();
        }
        let ms = frame_start.elapsed().as_secs_f32() * 1e3;
        self.frame_ms = if self.frame_ms == 0.0 { ms } else { self.frame_ms * 0.9 + ms * 0.1 };
        ui.ctx().request_repaint_after(Duration::from_secs_f32(1.0 / self.cfg.fps.max(1) as f32));
    }

    /// Settings survive a crash, not just a clean exit (rule I8).
    fn auto_save_interval(&self) -> Duration {
        Duration::from_secs(5)
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, eframe::APP_KEY, &self.cfg);
    }
}

/// No window: sweep for `secs`, print the rate once a second and the seam ripple at the end, and
/// optionally write the stitched spectrum as CSV.
pub fn headless(opts: Options, secs: f32, csv: Option<PathBuf>) -> Result<(), String> {
    let mut cfg = Config::default();
    for o in &opts.overrides {
        o(&mut cfg);
    }
    cfg.fit_board();
    let plan = cfg.plan();
    let engine = Engine::new(plan.clone(), cfg.dsp(), !opts.sim);
    let asked = crate::hydra::board();
    let mut src = if opts.sim { Src::Sim(Sim::start(engine.sink(), plan.clone(), cfg.rx())) } else { open_device(&opts.lib, opts.serial, &engine, &plan, cfg.rx()) };
    let mut plan = plan;
    // a Pluto reports its tuning range when it opens: re-plan if the range moved the plan
    if crate::hydra::board() == asked && cfg.plan() != plan {
        plan = cfg.plan();
        engine.set_plan(plan.clone());
        if let Src::Device(d) = &mut src {
            d.sweep(&plan)?;
        }
    }
    if crate::hydra::board() != asked {
        // the firmware said which Airspy it is: fit the settings and start the sweep
        engine.set_eq_prefix(crate::hydra::board().cache_prefix());
        cfg.fit_board();
        plan = cfg.plan();
        engine.set_plan(plan.clone());
        if let Src::Device(d) = &mut src {
            d.sweep(&plan)?;
        }
    }
    match &src {
        Src::Offline(e) => return Err(e.clone()),
        Src::Device(d) => println!("{} via {}", d.version(), d.lib_path().display()),
        Src::Sim(_) => println!("simulator"),
    }
    println!(
        "plan: {:.3}–{:.3} MHz, {}, {} steps × {} MHz, {:?}, dwell {}, lock reads {}, bins {:.2} kHz",
        plan.start_hz / 1e6, plan.stop_hz / 1e6, plan.rate.label(), plan.n_steps, plan.step_hz as f64 / 1e6, plan.order, plan.dwell, plan.lock_reads, dsp::bin_hz(cfg.nfft, plan.rate) / 1e3
    );
    let t0 = Instant::now();
    let mut next = 1.0;
    while t0.elapsed().as_secs_f32() < secs && !crate::QUIT.load(std::sync::atomic::Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(50));
        if t0.elapsed().as_secs_f32() >= next {
            next += 1.0;
            let s = engine.live.lock().stats.clone();
            println!(
                "{:4.0} s  {:5.0} steps/s  {:5.2} GHz/s  {:5.2} passes/s  lock {}/{}  torn {} gaps {} misfiled {} timeouts {} drops {}  {:.1} MB/s",
                t0.elapsed().as_secs_f32(), s.steps_per_s, s.steps_per_s as f64 * plan.window_hz / 1e9, s.steps_per_s / plan.n() as f32,
                s.lock_median, s.lock_max, s.torn, s.gaps, s.misfiled, s.lock_timeouts, s.host_drops, s.usb_mb_s
            );
        }
    }
    let l = engine.live.lock();
    println!("passes {}, steps {}, flatness correction {:?}", l.stats.passes, l.stats.steps, l.eq_state);
    if let Some((raw, eq)) = l.ripple() {
        println!("per-step passband ripple: {raw:.2} dB p-p raw, {eq:.2} dB p-p after correction");
    }
    if let Some(path) = csv {
        std::fs::write(&path, l.to_csv()).map_err(|e| format!("{}: {e}", path.display()))?;
        println!("wrote {}", path.display());
    }
    drop(l);
    drop(src);
    Ok(())
}

fn open_device(lib: &std::path::Path, serial: u64, engine: &Engine, plan: &Plan, rx: RxSettings) -> Src {
    let asked = crate::hydra::board();
    match Device::open(lib, serial, engine.sink(), rx, plan.rate) {
        // an Airspy turned out to be the other model: the caller fits the plan and starts the sweep
        Ok(d) if crate::hydra::board() != asked => Src::Device(d),
        Ok(mut d) => match d.sweep(plan) {
            Ok(()) => {
                log::info!("{} via {}", d.version(), d.lib_path().display());
                Src::Device(d)
            }
            Err(e) => Src::Offline(e),
        },
        Err(e) => Src::Offline(e),
    }
}

#[derive(Clone, Copy)]
enum Act {
    Retry,
    Sim,
    Dismiss,
    LockOn,
    Resume,
}

/// A status-bar condition: a sentence, its level, and the actions that fix it.
struct Cond {
    level: Level,
    text: String,
    acts: Vec<(Act, &'static str)>,
}

struct Card {
    label: &'static str,
    value: String,
    unit: &'static str,
    sub: String,
    /// Out of spec: caution ink on the value, and the word in the value says why.
    flag: bool,
    spark: bool,
}

/// A KPI tile: a display well, an uppercase label, a big tabular value with a quieter unit, and a
/// line of context with its time window (rules D1, D7).
fn paint_card(p: &Painter, r: Rect, c: &Card, spark: Option<&std::collections::VecDeque<f32>>) {
    p.rect_filled(r, theme::RADIUS, WELL);
    p.rect_stroke(r, theme::RADIUS, Stroke::new(1.0, EDGE), egui::StrokeKind::Inside);
    if let Some(h) = spark
        && h.len() > 2
    {
        let max = h.iter().copied().fold(1.0f32, f32::max) * 1.15;
        let sr = Rect::from_min_max(pos2(r.left() + r.width() * 0.55, r.top() + 12.0), pos2(r.right() - 10.0, r.top() + r.height() * 0.6));
        let n = h.len().max(60);
        let pts: Vec<Option<Pos2>> = h
            .iter()
            .enumerate()
            .map(|(i, &v)| Some(pos2(sr.right() - (h.len() - 1 - i) as f32 / (n - 1) as f32 * sr.width(), sr.bottom() - v / max * sr.height())))
            .collect();
        polyline(p, &pts, Stroke::new(1.5, TRACE));
    }
    let label = p.layout_no_wrap(c.label.to_uppercase(), theme::label_font(theme::XS), INK2);
    p.galley(r.min + vec2(11.0, 8.0), label, INK2);
    let vr = p.text(r.min + vec2(10.0, 21.0), Align2::LEFT_TOP, &c.value, theme::headline(26.0), if c.flag { CAUTION } else { INK });
    p.text(pos2(vr.right() + 4.0, vr.bottom() - 6.0), Align2::LEFT_BOTTOM, c.unit, theme::ui_font(theme::SM), INK2);
    let sub = p.layout_no_wrap(c.sub.clone(), theme::ui_font(theme::XS + 0.5), INK2);
    p.with_clip_rect(r.shrink(6.0)).galley(pos2(r.left() + 11.0, r.bottom() - 8.0 - sub.size().y), sub, INK2);
}

/// A small identity swatch next to a trace's name.
fn swatch(ui: &mut Ui, c: Color32) {
    let (r, _) = ui.allocate_exact_size(vec2(14.0, 14.0), Sense::hover());
    ui.painter().line_segment([pos2(r.left(), r.center().y), pos2(r.right(), r.center().y)], Stroke::new(2.0, c));
}

/// What a sweep mode change changed, for its waterfall break: the new range, else the profile now in
/// use, else the settings that differ. None if nothing a viewer would care about changed.
fn mode_change((a, na): &(Plan, usize), (b, nb): &(Plan, usize), cfg: &Config) -> Option<String> {
    let mut parts = Vec::new();
    if a.start_hz != b.start_hz || a.stop_hz != b.stop_hz {
        parts.push(format!("{}–{} MHz", fmt_trim(b.start_hz / 1e6, 3), fmt_trim(b.stop_hz / 1e6, 3)));
    }
    let engine = a.rate != b.rate || a.step_hz != b.step_hz || a.order != b.order || a.dwell != b.dwell || a.lock_reads != b.lock_reads || a.skip != b.skip;
    let profile = cfg.profile().filter(|_| engine);
    if let Some(i) = profile {
        parts.push(PROFILES[i].name.to_string());
    } else {
        if a.rate != b.rate {
            parts.push(b.rate.label().to_string());
        }
        if a.step_hz != b.step_hz {
            parts.push(format!("{} MHz steps", fmt_trim(b.step_hz as f64 / 1e6, 2)));
        }
        if a.order != b.order {
            parts.push(if b.order == Order::VcoContinuous { "VCO order" } else { "ascending" }.to_string());
        }
        if a.dwell != b.dwell {
            parts.push(format!("dwell {}", b.dwell));
        }
        if a.lock_reads != b.lock_reads {
            parts.push(if b.lock_reads == 0 { "lock check off".to_string() } else { format!("lock reads {}", b.lock_reads) });
        }
        if a.skip != b.skip {
            parts.push(format!("settle {}", b.skip));
        }
    }
    if na != nb && profile.is_none_or(|i| PROFILES[i].nfft.is_none()) {
        parts.push(format!("{:.1} kHz bins", dsp::bin_hz(*nb, b.rate) / 1e3));
    }
    (!parts.is_empty()).then(|| parts.join(" · "))
}

/// Local wall-clock time, HH:MM:SS, from the C library (localtime_r; _localtime64_s on Windows).
fn local_hms() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    // struct tm starts with sec, min, hour as ints everywhere; glibc adds gmtoff and zone at the end
    #[repr(C)]
    struct Tm {
        f: [i32; 9],
        _glibc: [u64; 2],
    }
    let mut tm = Tm { f: [0; 9], _glibc: [0; 2] };
    #[cfg(not(windows))]
    let ok = {
        unsafe extern "C" {
            fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
        }
        // SAFETY: localtime_r only writes into `tm`, which outlives the call and is big enough.
        !unsafe { localtime_r(&now, &mut tm) }.is_null()
    };
    #[cfg(windows)]
    let ok = {
        unsafe extern "C" {
            fn _localtime64_s(out: *mut Tm, t: *const i64) -> i32;
        }
        // SAFETY: as above; the Windows struct tm is the nine ints alone.
        unsafe { _localtime64_s(&mut tm, &now) == 0 }
    };
    if !ok {
        return String::new();
    }
    format!("{:02}:{:02}:{:02}", tm.f[2], tm.f[1], tm.f[0])
}

fn paint_palette_bar(p: &Painter, r: Rect, pal: Palette) {
    let lut = pal.lut();
    let n = 64;
    for i in 0..n {
        let x0 = r.left() + r.width() * i as f32 / n as f32;
        let x1 = r.left() + r.width() * (i + 1) as f32 / n as f32 + 0.5;
        p.rect_filled(Rect::from_min_max(pos2(x0, r.top()), pos2(x1, r.bottom())), 0, lut[i * 255 / (n - 1)]);
    }
    p.rect_stroke(r, 2, Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
}

fn gradient_rect(p: &Painter, r: Rect, top: Color32, bottom: Color32) {
    let mut m = Mesh::default();
    m.colored_vertex(r.left_top(), top);
    m.colored_vertex(r.right_top(), top);
    m.colored_vertex(r.right_bottom(), bottom);
    m.colored_vertex(r.left_bottom(), bottom);
    m.add_triangle(0, 1, 2);
    m.add_triangle(0, 2, 3);
    p.add(Shape::mesh(m));
}

fn polyline(p: &Painter, pts: &[Option<Pos2>], stroke: Stroke) {
    let mut run: Vec<Pos2> = Vec::new();
    for q in pts {
        match q {
            Some(q) => run.push(*q),
            None => {
                if run.len() > 1 {
                    p.add(Shape::line(std::mem::take(&mut run), stroke));
                }
                run.clear();
            }
        }
    }
    if run.len() > 1 {
        p.add(Shape::line(run, stroke));
    }
}

/// Vertical gradient fill between a broken polyline and `bottom`.
fn fill_under(p: &Painter, pts: &[Option<Pos2>], bottom: f32, top_c: Color32, bot_c: Color32) {
    let mut m = Mesh::default();
    let mut prev: Option<u32> = None;
    for q in pts {
        match q {
            Some(q) => {
                let i = m.vertices.len() as u32;
                m.colored_vertex(*q, top_c);
                m.colored_vertex(pos2(q.x, bottom), bot_c);
                if let Some(j) = prev {
                    m.add_triangle(j, j + 1, i);
                    m.add_triangle(i, j + 1, i + 1);
                }
                prev = Some(i);
            }
            None => prev = None,
        }
    }
    p.add(Shape::mesh(m));
}

/// Nice frequency ticks: (Hz, major).
fn freq_ticks(a: f64, b: f64) -> Vec<(f64, bool)> {
    let span = (b - a).max(1.0);
    let raw = span / 9.0;
    let mag = 10f64.powf(raw.log10().floor());
    let step = [1.0, 2.0, 5.0, 10.0].iter().map(|m| m * mag).find(|s| *s >= raw).unwrap_or(10.0 * mag);
    let minor = step / if (step / mag).round() as i64 == 2 { 4.0 } else { 5.0 };
    let mut out = Vec::new();
    let mut f = (a / minor).ceil() * minor;
    while f <= b {
        let major = ((f / step).round() * step - f).abs() < minor * 0.01;
        out.push((f, major));
        f += minor;
    }
    out
}

fn find_peaks(xs: &[f64], ys: &[f32], n: usize, min_sep_hz: f64) -> Vec<(f64, f32)> {
    if n == 0 || ys.len() < 8 {
        return Vec::new();
    }
    let mut finite: Vec<f32> = ys.iter().copied().filter(|v| v.is_finite()).collect();
    if finite.len() < 8 {
        return Vec::new();
    }
    finite.sort_by(f32::total_cmp);
    let floor = finite[finite.len() / 2];
    let thresh = floor + 12.0;
    let w = 4;
    let mut cand: Vec<(f64, f32)> = (0..ys.len())
        .filter(|&i| {
            let v = ys[i];
            v.is_finite() && v > thresh && (i.saturating_sub(w)..(i + w + 1).min(ys.len())).all(|j| !(ys[j] > v))
        })
        .map(|i| (xs[i], ys[i]))
        .collect();
    cand.sort_by(|a, b| b.1.total_cmp(&a.1));
    let mut out: Vec<(f64, f32)> = Vec::new();
    for c in cand {
        if out.iter().all(|o| (o.0 - c.0).abs() > min_sep_hz) {
            out.push(c);
            if out.len() == n {
                break;
            }
        }
    }
    out
}

/// Strongest full-resolution bin between f_lo and f_hi.
fn max_in(l: &crate::engine::Live, t: Trace, f_lo: f64, f_hi: f64) -> Option<(f64, f32)> {
    let lo = l.index_of(f_lo.max(l.plan.start_hz));
    let hi = l.index_of(f_hi.min(l.plan.stop_hz)).max(lo + 1).min(l.cur.len());
    (lo..hi).filter_map(|i| Some((l.freq(i), l.value(t, i))).filter(|(_, v)| v.is_finite())).max_by(|a, b| a.1.total_cmp(&b.1))
}

fn label_decimals(span_hz: f64) -> usize {
    match span_hz {
        s if s > 300e6 => 1,
        s if s > 30e6 => 2,
        s if s > 3e6 => 3,
        s if s > 0.3e6 => 4,
        _ => 5,
    }
}

fn fmt_freq(hz: f64, dec: usize) -> String {
    format!("{:.*}", dec, hz / 1e6)
}

fn fmt_trim(v: f64, dec: usize) -> String {
    let s = format!("{v:.dec$}");
    if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s }
}

/// A rate and step for a board's rates: the rate if the board has it, else the board's default rate,
/// keeping the step if that rate offers it and otherwise using that rate's default step.
fn fit_rate_step(rate: Rate, step_mhz: f64, rates: &[Rate]) -> (Rate, f64) {
    let fitted = if rates.contains(&rate) { rate } else { rates[0] };
    if fitted == rate || fitted.steps().iter().any(|&(v, _)| (v - step_mhz).abs() < 1e-9) {
        (fitted, step_mhz)
    } else {
        (fitted, fitted.default_step_mhz())
    }
}

/// Live-view spans for the Pluto, MHz.
const LIVE_SPANS: &[(f64, &str)] = &[(50.0, "50"), (25.0, "25"), (12.0, "12"), (6.0, "6"), (3.0, "3"), (1.0, "1 MHz")];

/// The lowest Pluto rate whose usable window covers `span_mhz`: the finest bins for that span.
fn live_rate(span_mhz: f64) -> Rate {
    [Rate::P3, Rate::P7, Rate::P15, Rate::P31, Rate::P61].into_iter().find(|r| r.max_window_hz() >= span_mhz * 1e6 - 1.0).unwrap_or(Rate::P61)
}

/// The board's rate for a requested one: the R2 runs 10 MSPS only.
fn board_rate(rate: Rate) -> Rate {
    let rates = crate::hydra::board().rates();
    if rates.contains(&rate) { rate } else { rates[0] }
}

/// "Connect the RFOne" / "Connect the Airspy R2" (a static string for the condition's button).
fn connect_label() -> &'static str {
    match crate::hydra::board() {
        crate::hydra::Board::RFOne => "Connect the RFOne",
        crate::hydra::Board::Airspy => "Connect the Airspy R2",
        crate::hydra::Board::Mini => "Connect the Airspy Mini",
        crate::hydra::Board::HackRF => "Connect the HackRF",
        crate::hydra::Board::Pluto => "Connect the Pluto",
    }
}

/// Where exports go: the repo's captures/ when it exists (gitignored), else $XDG_DATA_HOME/corvus-sweep/captures
/// (on Windows, Documents\Corvus Sweep).
fn captures_dir() -> PathBuf {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../captures");
    if repo.exists() {
        return repo;
    }
    #[cfg(windows)]
    if let Some(dir) = std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("Documents").join("Corvus Sweep"))
        && std::fs::create_dir_all(&dir).is_ok()
    {
        return dir;
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(std::env::temp_dir);
    let dir = data.join("corvus-sweep").join("captures");
    if std::fs::create_dir_all(&dir).is_ok() { dir } else { std::env::temp_dir() }
}

/// UTC timestamp for file names.
fn stamp() -> String {
    let s = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (s.div_euclid(86400), s.rem_euclid(86400));
    // civil-from-days (Howard Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

fn save_png(path: &std::path::Path, img: &egui::ColorImage) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let f = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(f), img.size[0] as u32, img.size[1] as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut w = enc.write_header().map_err(|e| e.to_string())?;
    let data: Vec<u8> = img.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    w.write_image_data(&data).map_err(|e| e.to_string())
}

#[allow(dead_code)]
fn _assert_send() {
    fn is_send<T: Send>() {}
    is_send::<hydra::Device>();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_ends_that_cross_keep_the_span() {
        let mut c = Config { start_mhz: 87.5, stop_mhz: 108.0, ..Config::default() };
        c.start_mhz = 400.0; // typed a start above the stop
        c.keep_span(87.5, 108.0);
        assert_eq!((c.start_mhz, c.stop_mhz), (400.0, 420.5));
        c.stop_mhz = 100.0; // typed a stop below the start
        c.keep_span(400.0, 420.5);
        assert_eq!((c.start_mhz, c.stop_mhz), (79.5, 100.0));
        c.stop_mhz = 300.0; // an ordinary edit is left alone
        c.keep_span(79.5, 100.0);
        assert_eq!((c.start_mhz, c.stop_mhz), (79.5, 300.0));
        c.start_mhz = 1790.0; // clamped at the top of the band
        c.keep_span(79.5, 300.0);
        assert_eq!(c.stop_mhz, 1800.0);
    }

    #[test]
    fn step_keys_walk_the_offered_steps() {
        let mut c = Config::default();
        assert_eq!(c.step_mhz, 10.0);
        c.bump_step(1);
        assert_eq!(c.step_mhz, 10.0);
        c.bump_step(-1);
        c.bump_step(-1);
        assert_eq!(c.step_mhz, 6.0);
        c.set_rate(Rate::M10);
        c.bump_step(1);
        c.bump_step(1);
        assert_eq!(c.step_mhz, 9.0);
    }

    #[test]
    fn profiles_fit_a_board_without_12msps() {
        // Fastest and Bursts use 10 MHz steps at 12 MSPS: on the R2 they become 8 MHz at 10 MSPS;
        // Quality and Weak keep their 8 MHz steps.
        let fitted: Vec<(Rate, f64)> = PROFILES.iter().map(|p| fit_rate_step(p.rate, p.step_mhz, &[Rate::M10])).collect();
        assert_eq!(fitted, vec![(Rate::M10, 8.0), (Rate::M10, 8.0), (Rate::M10, 8.0), (Rate::M10, 8.0)]);
        let mini: Vec<(Rate, f64)> = PROFILES.iter().map(|p| fit_rate_step(p.rate, p.step_mhz, &[Rate::M6])).collect();
        assert_eq!(mini, vec![(Rate::M6, 4.0); 4]);
        for p in PROFILES {
            assert_eq!(fit_rate_step(p.rate, p.step_mhz, &[Rate::M12, Rate::M10]), (p.rate, p.step_mhz));
        }
    }

    #[test]
    fn each_profile_lights_only_itself() {
        for (i, pr) in PROFILES.iter().enumerate() {
            let mut c = Config::default();
            pr.apply(&mut c);
            assert_eq!(c.profile(), Some(i), "{}", pr.name);
        }
        assert_eq!(Config::default().profile(), Some(0), "the defaults are Fastest");
    }
}
