//! The window: header, speed HUD, controls, and the stitched spectrum over its waterfall.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use egui::{Align2, Color32, Key, Mesh, Modifiers, Painter, PointerButton, Pos2, Rect, RichText, Sense, Shape, Stroke, Ui, pos2, vec2};
use serde::{Deserialize, Serialize};

use crate::dsp::{self, NFFTS};
use crate::engine::{Detector, DspSettings, Engine, EqState, Stats, Trace};
use crate::hydra::{self, Device, Gain, RxSettings};
use crate::plan::{self, Order, PRESETS, Plan, Rate};
use crate::sim::Sim;
use crate::theme::{self, ACCENT, AVERAGE, CAUTION, CHIP, Density, EDGE, GRATICULE, HOUSE, INK, INK2, INK3, LINE, Level, PANEL, PEAK, Palette, SCALE_INK, Slide, TOUCH, TRACE, WELL};
use crate::waterfall::{ROWS, Waterfall};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub start_mhz: f64,
    pub stop_mhz: f64,
    pub step_mhz: f64,
    pub rate: Rate,
    pub order: Order,
    pub dwell: u8,
    pub lock_reads: u8,
    pub skip: u8,
    pub port: u8,
    pub gain: Gain,
    pub nfft: usize,
    pub detector: Detector,
    pub show_avg: bool,
    pub avg_alpha: f32,
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
}

impl Default for Config {
    fn default() -> Self {
        Config {
            start_mhz: 24.0,
            stop_mhz: 1800.0,
            step_mhz: Rate::default().default_step_mhz(),
            rate: Rate::default(),
            order: Order::VcoContinuous,
            dwell: 1,
            lock_reads: 2,
            skip: 0,
            port: 0,
            gain: Gain::Linearity(10),
            nfft: 2048,
            detector: Detector::Peak,
            show_avg: false,
            avg_alpha: 0.2,
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
        }
    }
}

impl Config {
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

    /// Fit saved or profile settings to the board: its rates (the Airspys have one each) and ports.
    pub fn fit_board(&mut self) {
        if self.rate != board_rate(self.rate) {
            self.set_rate(board_rate(self.rate));
        }
        if !crate::hydra::board().has_ports() {
            self.port = 0;
        }
    }

    /// After a typed start or stop crosses the other end, keep the old span instead of letting the
    /// plan collapse the range to its 200 kHz minimum.
    fn keep_span(&mut self, start0: f64, stop0: f64) {
        let span = (stop0 - start0).max(0.2);
        if self.start_mhz != start0 && self.start_mhz > self.stop_mhz - 0.2 {
            self.stop_mhz = (self.start_mhz + span).min(plan::F_MAX_HZ / 1e6);
        } else if self.stop_mhz != stop0 && self.stop_mhz < self.start_mhz + 0.2 {
            self.start_mhz = (self.stop_mhz - span).max(plan::F_MIN_HZ / 1e6);
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
        DspSettings { nfft: self.nfft, avg_alpha: if self.show_avg { self.avg_alpha } else { 0.0 }, eq_on: self.eq_on, wf_rate: self.wf_rate }
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
            && (!self.peak_hold || (c.show_peak && c.detector == Detector::Peak))
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
    detector: Detector,
}

impl EngineSnap {
    fn of(c: &Config) -> EngineSnap {
        EngineSnap { rate: c.rate, step_mhz: c.step_mhz, order: c.order, dwell: c.dwell, lock_reads: c.lock_reads, skip: c.skip, nfft: c.nfft, show_peak: c.show_peak, detector: c.detector }
    }

    fn restore(&self, c: &mut Config) {
        (c.rate, c.step_mhz, c.order, c.dwell, c.lock_reads, c.skip, c.nfft, c.show_peak, c.detector) =
            (self.rate, self.step_mhz, self.order, self.dwell, self.lock_reads, self.skip, self.nfft, self.show_peak, self.detector);
    }
}

pub struct Options {
    pub sim: bool,
    pub lib: PathBuf,
    pub serial: u64,
    pub screenshot: Option<(PathBuf, f32)>,
    pub overrides: Vec<Box<dyn Fn(&mut Config)>>,
}

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

#[derive(Default)]
struct Shown {
    live: (Vec<f64>, Vec<f32>),
    avg: Option<(Vec<f64>, Vec<f32>)>,
    peak: Option<(Vec<f64>, Vec<f32>)>,
    hover: Option<(f64, f32)>,
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
        App {
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
            lib: opts.lib,
            serial: opts.serial,
            screenshot: opts.screenshot,
            shot_requested: None,
            frame_times: Default::default(),
            started: Instant::now(),
            focus_start: false,
            show_keys: false,
            cfg,
        }
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
            ui.label(RichText::new(format!("{} firmware sweep, stitched live", crate::hydra::board().name())).size(theme::SM).color(INK2));
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
        if self.applied.lock_reads == 0 {
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
                label: "Full passes",
                value: if passes >= 100.0 { format!("{passes:.0}") } else { format!("{passes:.2}") },
                unit: "/s",
                sub: if passes > 0.0 { format!("{:.0} ms per pass · {} so far", 1e3 / passes, s.passes) } else { format!("{} so far", s.passes) },
                flag: false,
                spark: false,
            },
            Card { label: "Span", value: span_v, unit: span_u, sub: format!("{} steps · {:.1} kHz bins", p.n_steps, dsp::bin_hz(self.cfg.nfft, p.rate) / 1e3), flag: false, spark: false },
            match p.order {
                Order::VcoContinuous => Card { label: "Visit order", value: "VCO".into(), unit: "continuous", sub: format!("{fast} fast joins · {slow} slow per pass"), flag: false, spark: false },
                Order::Ascending => Card { label: "Visit order", value: "Ascending".into(), unit: "", sub: format!("{slow} slow crossings per pass"), flag: false, spark: false },
            },
            if p.lock_reads == 0 {
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
            Card { label: "USB", value: format!("{:.1}", s.usb_mb_s), unit: "MB/s", sub: format!("{} blocks", s.blocks), flag: false, spark: false },
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
                theme::section(ui, "Range");
                // Typed values apply on Enter, Tab or clicking away (Esc cancels), not per keystroke.
                let (s0, e0) = (self.cfg.start_mhz, self.cfg.stop_mhz);
                let focus = std::mem::take(&mut self.focus_start);
                theme::row(ui, "Start", |ui| {
                    if focus {
                        let id = ui.next_auto_id();
                        ui.memory_mut(|m| m.request_focus(id));
                    }
                    ui.add(freq_field(&mut self.cfg.start_mhz, 24.0..=1799.8)).on_hover_text("G to type a range");
                    ui.label(RichText::new("MHz").size(theme::SM).color(INK2));
                });
                theme::row(ui, "Stop", |ui| {
                    ui.add(freq_field(&mut self.cfg.stop_mhz, 24.2..=1800.0));
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
                    if ui.add_enabled(zoomed, egui::Button::new("Sweep visible area")).on_hover_text("Zoom in first (scroll on the plot), or shift-drag on the plot to sweep a selection. Key: S").on_disabled_hover_text("Zoom in first (scroll on the plot), or shift-drag on the plot to sweep a selection").clicked() {
                        let (a, b) = self.view;
                        self.set_range(a, b);
                    }
                    if ui.button("Full range").on_hover_text("F").clicked() {
                        self.set_range(plan::F_MIN_HZ, plan::F_MAX_HZ);
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
                    let rate_hint = "10 MSPS: 20 MSPS real, IF 5 MHz, steps up to 9.3 MHz.\n12 MSPS: 24 MSPS real, tuner filter wide open, steps up to 10 MHz, about a third more GHz/s.";
                    if crate::hydra::board().rates().len() > 1 {
                        if theme::row_hint(ui, "Rate", rate_hint, |ui| theme::segmented(ui, &mut rate, &[(Rate::M10, "10 MSPS"), (Rate::M12, "12 MSPS")])) {
                            self.cfg.set_rate(rate);
                        }
                    } else {
                        let only = format!(
                            "The {}'s fastest setting: {:.0} MSPS real, IF {:.0} MHz, steps up to {} MHz.",
                            crate::hydra::board().name(),
                            self.cfg.rate.adc_hz() / 1e6,
                            self.cfg.rate.if_hz() / 1e6,
                            fmt_trim(self.cfg.rate.max_window_hz() / 1e6, 1)
                        );
                        theme::row_hint(ui, "Rate", &only, |ui| {
                            ui.label(RichText::new(format!("{} MSPS", self.cfg.rate.msps())).size(theme::SM).color(INK));
                            false
                        });
                    }
                    theme::row_hint(
                        ui,
                        "Order",
                        "VCO-continuous groups steps by the tuner's VCO divider, so divider switches cost about 0.5 ms instead of 10 ms.\nAscending goes low to high: each divider switch costs about 10 ms, plus 8 ms to wrap.",
                        |ui| theme::segmented(ui, &mut self.cfg.order, &[(Order::VcoContinuous, "VCO-continuous"), (Order::Ascending, "Ascending")]),
                    );
                    theme::slider_u8(ui, "Dwell", &mut self.cfg.dwell, &Slide::int(1.0, 8.0, 1.0, "blk"));
                    theme::slider_u8(ui, "Lock reads", &mut self.cfg.lock_reads, &Slide::int(0.0, 3.0, 2.0, ""));
                    theme::slider_u8(ui, "Settle", &mut self.cfg.skip, &Slide::int(0.0, 8.0, 0.0, "blk"));
                    theme::caption(ui, &format!("Dwell averages more blocks per step (less noise, slower). Settle discards extra {:.0} µs blocks after lock.", self.cfg.rate.block_s() * 1e6));
                });
            });
            ui.add_space(gap);
            theme::card(ui, |ui| {
                theme::section(ui, "Receiver");
                if crate::hydra::board().has_ports() {
                    theme::row(ui, "Port", |ui| theme::segmented(ui, &mut self.cfg.port, &[(0, "ANT"), (1, "CABLE1"), (2, "CABLE2")]));
                }
                let mut mode = match self.cfg.gain {
                    Gain::Linearity(_) => 0,
                    Gain::Sensitivity(_) => 1,
                    Gain::Manual { .. } => 2,
                };
                let gain_hint = "Linearity 10 keeps FM and TV out of clipping. Gain and port changes stop the sweep for a moment (the firmware refuses tuner commands mid-sweep). The bias tee stays off.";
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
                    ui.checkbox(&mut self.cfg.show_peak, "Peak").on_hover_text("Peak hold (H)");
                    swatch(ui, AVERAGE);
                    ui.checkbox(&mut self.cfg.show_avg, "Average").on_hover_text("V");
                });
                if self.cfg.show_avg {
                    theme::slider_f32(ui, "Response", &mut self.cfg.avg_alpha, &Slide { min: 0.02, max: 0.8, default: 0.2, integer: false, log: true, decimals: 2, unit: "" });
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
                theme::more(ui, "display-more", "Layers, peak hold and passband", |ui| {
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut self.cfg.beam, "Sweep beam")
                            .on_hover_text("Lights the steps as the radio visits them, when a full pass takes about a second or more. Faster sweeps would only flash, so the strip under the axis shows which steps are fresh instead. Key: B");
                        ui.checkbox(&mut self.cfg.vco_marks, "VCO switch marks");
                    });
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
                Src::Device(d) => format!("{} · {} · {} · rev {}", d.version, if self.serial == 0 { format!("first {}", crate::hydra::board().name()) } else { format!("{:016X}", self.serial) }, d.lib_path.display(), d.fw_revision),
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
        let rail_h = if self.cfg.labels > 0 { 26.0 } else { 6.0 };
        let axis_h = 20.0;
        let strip_h = 20.0;
        let spec_h = ((full.height() - rail_h - axis_h - strip_h - 8.0) * 0.48).max(120.0);
        let rail = Rect::from_min_size(pos2(full.left() + gutter, full.top()), vec2(full.width() - gutter, rail_h));
        let spec = Rect::from_min_size(pos2(rail.left(), rail.bottom()), vec2(rail.width(), spec_h));
        let axis = Rect::from_min_size(pos2(spec.left(), spec.bottom()), vec2(spec.width(), axis_h));
        let strip = Rect::from_min_size(pos2(spec.left(), axis.bottom() + 2.0), vec2(spec.width(), strip_h));
        let wf = Rect::from_min_max(pos2(spec.left(), strip.bottom() + 6.0), pos2(spec.right(), full.bottom()));

        self.clamp_view();
        self.interact(ui, spec, wf, Rect::from_min_max(pos2(full.left(), spec.top()), pos2(spec.left() - 4.0, spec.bottom())));
        self.gather(spec.width().max(1.0) as usize);

        let painter = ui.painter().clone();
        self.paint_spectrum(&painter, spec);
        self.paint_axis(&painter, axis);
        self.paint_strip(&painter, strip);
        self.paint_waterfall(ui.ctx(), &painter, wf, Rect::from_min_max(pos2(full.left(), wf.top()), pos2(wf.left() - 4.0, wf.bottom())));
        self.paint_overlays(&painter, spec, wf);
        self.peak_rail(ui, rail, spec);
        self.paint_readout(&painter, spec);
        if let Src::Offline(err) = &self.src {
            let err = err.clone();
            self.offline_banner(ui, spec, &err);
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

    /// The hover readout: always in the same corner of the plot (rule O7).
    fn paint_readout(&self, p: &Painter, r: Rect) {
        let Some((f, db)) = self.shown.hover else { return };
        let dec = label_decimals(self.view.1 - self.view.0) + 1;
        let vf = p.layout_no_wrap(fmt_freq(f, dec), theme::num_medium(15.0), INK);
        let uf = p.layout_no_wrap("MHz".into(), theme::ui_font(12.0), INK2);
        let vd = p.layout_no_wrap(format!("{db:.1}").replace('-', "−"), theme::num_medium(15.0), INK);
        let ud = p.layout_no_wrap("dBFS".into(), theme::ui_font(12.0), INK2);
        let w = vf.size().x + uf.size().x + vd.size().x + ud.size().x + 3.0 * 2.0 + 16.0 + 20.0;
        let br = Rect::from_min_size(r.min + vec2(8.0, 8.0), vec2(w, 28.0));
        p.rect_filled(br, 5, WELL.gamma_multiply(0.9));
        p.rect_stroke(br, 5, egui::Stroke::new(1.0, LINE), egui::StrokeKind::Inside);
        let mut x = br.left() + 10.0;
        let cy = br.center().y;
        for (g, gap) in [(vf, 3.0), (uf, 16.0), (vd, 3.0), (ud, 0.0)] {
            let w = g.size().x;
            p.galley(pos2(x, cy - g.size().y / 2.0), g, INK);
            x += w + gap;
        }
    }

    fn clamp_view(&mut self) {
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

    fn interact(&mut self, ui: &mut Ui, spec: Rect, wf: Rect, gutter: Rect) {
        let shift = ui.input(|i| i.modifiers.shift);
        self.hover_x = None;
        for (i, r) in [spec, wf].into_iter().enumerate() {
            let resp = ui.interact(r, ui.id().with(("plot", i)), Sense::click_and_drag());
            if let Some(p) = resp.hover_pos() {
                self.hover_x = Some(p.x);
                let (scroll, zoom) = ui.input(|i| (i.smooth_scroll_delta, i.zoom_delta()));
                let k = (-scroll.y as f64 * 0.0025).exp() / zoom as f64;
                if k != 1.0 {
                    let f = self.f_of(r, p.x);
                    let span = (self.view.1 - self.view.0) * k;
                    let a = f - (f - self.view.0) * k;
                    self.view = (a, a + span);
                }
                if scroll.x != 0.0 {
                    let df = -scroll.x as f64 / r.width() as f64 * (self.view.1 - self.view.0);
                    self.view = (self.view.0 + df, self.view.1 + df);
                }
            }
            if resp.drag_started_by(PointerButton::Primary) && shift
                && let Some(p) = resp.interact_pointer_pos()
            {
                let f = self.f_of(r, p.x);
                self.selection = Some((f, f));
            }
            if resp.dragged_by(PointerButton::Primary) {
                if let (Some(sel), Some(p)) = (&mut self.selection, resp.interact_pointer_pos()) {
                    sel.1 = self.view.0 + ((p.x - r.left()) / r.width()) as f64 * (self.view.1 - self.view.0);
                } else {
                    let df = -resp.drag_delta().x as f64 / r.width() as f64 * (self.view.1 - self.view.0);
                    self.view = (self.view.0 + df, self.view.1 + df);
                }
            }
            if resp.drag_stopped()
                && let Some((a, b)) = self.selection.take()
            {
                let (lo, hi) = (a.min(b), a.max(b));
                if hi - lo >= 0.2e6 {
                    self.set_range(lo, hi);
                }
            }
            if resp.double_clicked() {
                self.view = (self.applied.start_hz, self.applied.stop_hz);
            }
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
        if ui.input(|i| i.key_pressed(Key::Escape)) {
            self.selection = None;
        }
        self.clamp_view();
    }

    /// Reduce the stitched arrays to what is on screen (one lock).
    fn gather(&mut self, cols: usize) {
        let live = self.engine.live.clone();
        let l = live.lock();
        let (a, b) = self.view;
        let det = self.cfg.detector;
        self.shown.live = l.trace(Trace::Live, a, b, cols, det);
        self.shown.avg = self.cfg.show_avg.then(|| l.trace(Trace::Average, a, b, cols, det));
        self.shown.peak = self.cfg.show_peak.then(|| l.trace(Trace::Peak, a, b, cols, Detector::Peak));
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
            let g = p.layout_no_wrap(format!("Sweep {} – {} MHz", fmt_freq(a.min(b), 3), fmt_freq(a.max(b), 3)), theme::num(12.0), INK);
            let cr = Rect::from_center_size(pos2((x0 + x1) / 2.0, r.top() + 18.0), g.size() + vec2(14.0, 8.0));
            p.rect_filled(cr, 4, WELL.gamma_multiply(0.9));
            p.galley(cr.min + vec2(7.0, 4.0), g, INK);
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
                if x1 - x0 > 64.0 {
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
        let u0 = ((self.view.0 - c0) / (c1 - c0)) as f32;
        let u1 = ((self.view.1 - c0) / (c1 - c0)) as f32;
        let rows = self.cfg.wf_rows.clamp(8, ROWS);
        self.wf.paint(ctx, &p.with_clip_rect(r), r, u0, u1, rows);
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

    fn paint_overlays(&self, p: &Painter, spec: Rect, wf: Rect) {
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
        let (a, b) = (self.applied.start_hz, self.applied.stop_hz);
        let span = b - a;
        let lo = (a + dir * span).clamp(plan::F_MIN_HZ, plan::F_MAX_HZ - span);
        self.set_range(lo, lo + span);
    }

    fn keys(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if ctx.egui_wants_keyboard_input() {
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
                Key::Backspace => self.back_range(),
                Key::F => self.set_range(plan::F_MIN_HZ, plan::F_MAX_HZ),
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
                    self.cfg.set_rate(if self.cfg.rate == Rate::M10 { Rate::M12 } else { Rate::M10 });
                    self.toast(format!("{} MSPS, {} MHz steps", self.cfg.rate.msps(), fmt_trim(self.cfg.step_mhz, 1)), false);
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
                ("Shift-drag", "Select a range to sweep"),
                ("Esc", "Cancel selection"),
            ],
        ),
        (
            "Display",
            &[
                ("A", "Auto levels"),
                ("P", "Clear peak hold"),
                ("H", "Peak hold on / off"),
                ("V", "Average on / off"),
                ("E", "Flatten passband on / off"),
                ("B", "Sweep beam on / off"),
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
    let src = if opts.sim { Src::Sim(Sim::start(engine.sink(), plan.clone(), cfg.rx())) } else { open_device(&opts.lib, opts.serial, &engine, &plan, cfg.rx()) };
    match &src {
        Src::Offline(e) => return Err(e.clone()),
        Src::Device(d) => println!("{} via {}", d.version, d.lib_path.display()),
        Src::Sim(_) => println!("simulator"),
    }
    println!(
        "plan: {:.3}–{:.3} MHz, {} MSPS, {} steps × {} MHz, {:?}, dwell {}, lock reads {}, bins {:.2} kHz",
        plan.start_hz / 1e6, plan.stop_hz / 1e6, plan.rate.msps(), plan.n_steps, plan.step_hz as f64 / 1e6, plan.order, plan.dwell, plan.lock_reads, dsp::bin_hz(cfg.nfft, plan.rate) / 1e3
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
    match Device::open(lib, serial, engine.sink(), rx, plan.rate) {
        Ok(mut d) => match d.sweep(plan) {
            Ok(()) => {
                log::info!("{} via {}", d.version, d.lib_path.display());
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

/// Local wall-clock time, HH:MM:SS (glibc localtime_r; Linux only, like the rest of the app).
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
            parts.push(format!("{} MSPS", b.rate.msps()));
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

fn local_hms() -> String {
    #[repr(C)]
    struct Tm {
        sec: i32,
        min: i32,
        hour: i32,
        mday: i32,
        mon: i32,
        year: i32,
        wday: i32,
        yday: i32,
        isdst: i32,
        gmtoff: i64,
        zone: *const std::ffi::c_char,
    }
    unsafe extern "C" {
        fn localtime_r(t: *const i64, out: *mut Tm) -> *mut Tm;
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let mut tm = Tm { sec: 0, min: 0, hour: 0, mday: 0, mon: 0, year: 0, wday: 0, yday: 0, isdst: 0, gmtoff: 0, zone: std::ptr::null() };
    // SAFETY: localtime_r only writes into `tm`, which outlives the call.
    if unsafe { localtime_r(&now, &mut tm) }.is_null() {
        return String::new();
    }
    format!("{:02}:{:02}:{:02}", tm.hour, tm.min, tm.sec)
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
    }
}

/// Where exports go: the repo's captures/ when it exists (gitignored), else $XDG_DATA_HOME/corvus-sweep/captures.
fn captures_dir() -> PathBuf {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../captures");
    if repo.exists() {
        return repo;
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
