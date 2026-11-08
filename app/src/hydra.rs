//! The board's library with the Corvus sweep API (libhydrasdr for the RFOne, libairspy for the Airspy R2),
//! loaded at run time, and the board driven in firmware sweep mode. See ../host/README.md. The two
//! libraries share one API shape under different prefixes, and the same tags and sweep parameters.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;

use libloading::Library;

pub use crate::plan::BLOCK_WORDS;
use crate::plan::{Plan, Rate};

pub const TAG_WORDS: usize = 8;
const SAMPLE_RAW: c_int = 5;

/// Which board Corvus Sweep drives. Both run the same Corvus sweep protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Board {
    RFOne,
    /// Airspy R2.
    Airspy,
    /// Airspy Mini: the same firmware image as the R2, which picks the Mini's settings at boot.
    Mini,
}

impl Board {
    pub fn parse(s: &str) -> Option<Board> {
        match s.to_ascii_lowercase().as_str() {
            "rfone" => Some(Board::RFOne),
            "airspy" | "r2" => Some(Board::Airspy),
            "mini" | "airspy-mini" => Some(Board::Mini),
            _ => None,
        }
    }

    /// Name for messages.
    pub fn name(self) -> &'static str {
        match self {
            Board::RFOne => "RFOne",
            Board::Airspy => "Airspy R2",
            Board::Mini => "Airspy Mini",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Board::RFOne => "hydrasdr_",
            Board::Airspy | Board::Mini => "airspy_",
        }
    }

    /// The RFOne has ANT, CABLE1 and CABLE2; the R2 has one antenna input.
    pub fn has_ports(self) -> bool {
        self == Board::RFOne
    }

    /// The sample-rate settings the sweep can use on this board, default first.
    pub fn rates(self) -> &'static [Rate] {
        match self {
            Board::RFOne => &[Rate::M12, Rate::M10],
            Board::Airspy => &[Rate::M10],
            Board::Mini => &[Rate::M6],
        }
    }

    /// The library this repo builds for the board (host/build/build.sh).
    fn repo_lib(self) -> &'static str {
        match self {
            Board::RFOne => "../host/build/out/linux/libhydrasdr.so.1.1.4",
            Board::Airspy | Board::Mini => "../host/build/out/airspy-linux/libairspy.so.1.0.10",
        }
    }

    fn system_lib(self) -> &'static str {
        match self {
            Board::RFOne => "libhydrasdr.so.1",
            Board::Airspy | Board::Mini => "libairspy.so.0",
        }
    }

    /// Prefix for files that hold what was learned from this board (its passband differs).
    pub fn cache_prefix(self) -> &'static str {
        match self {
            Board::RFOne => "",
            Board::Airspy => "airspy-",
            Board::Mini => "airspy-mini-",
        }
    }
}

static BOARD: std::sync::OnceLock<Board> = std::sync::OnceLock::new();

/// The board chosen at startup (`--board`, CORVUS_BOARD); the RFOne unless set.
pub fn board() -> Board {
    *BOARD.get().unwrap_or(&Board::RFOne)
}

pub fn set_board(b: Board) {
    let _ = BOARD.set(b);
}

/// CORVUS_BOARD if it names a board.
pub fn env_board() -> Option<Board> {
    std::env::var("CORVUS_BOARD").ok().and_then(|v| Board::parse(&v))
}

type Dev = c_void;
type RxCb = unsafe extern "C" fn(*mut Transfer) -> c_int;

#[repr(C)]
struct Transfer {
    device: *mut Dev,
    ctx: *mut c_void,
    samples: *mut c_void,
    sample_count: c_int,
    dropped_samples: u64,
    sample_type: c_int,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct SweepParams {
    pub f_start_hz: u32,
    pub f_step_hz: u32,
    pub n_steps: u16,
    pub skip_blocks: u8,
    pub dwell_blocks: u8,
    pub lock_reads: u8,
    pub flags: u8,
    pub lock_timeout: u16,
}

impl From<&Plan> for SweepParams {
    fn from(p: &Plan) -> Self {
        SweepParams {
            f_start_hz: p.f0,
            f_step_hz: p.step_hz,
            n_steps: p.n_steps,
            skip_blocks: p.skip,
            dwell_blocks: p.dwell,
            lock_reads: p.lock_reads,
            flags: p.flags(),
            lock_timeout: p.lock_timeout,
        }
    }
}

/// Tags of a valid sweep block (same checks as `hydrasdr_sweep_parse_block`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag {
    pub step: u16,
    pub block: u16,
    pub sweep: u32,
    pub freq_hz: u32,
    pub dma_count: u32,
    pub lock_polls: u16,
    pub lock_timed_out: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Block {
    Valid(Tag),
    Untagged,
    Torn,
}

pub fn parse_block(w: &[u16]) -> Block {
    let (h, t) = (&w[..TAG_WORDS], &w[BLOCK_WORDS - TAG_WORDS..BLOCK_WORDS]);
    if h[0] != 0xA55A || h[1] != 0x5357 {
        return Block::Untagged;
    }
    if t[0] != 0x5AA5 || h[2] != t[2] || h[3] != t[3] || h[6] != t[6] || h[7] != t[7] {
        return Block::Torn;
    }
    Block::Valid(Tag {
        step: h[2],
        block: h[3],
        sweep: h[4] as u32 | (h[5] as u32) << 16,
        freq_hz: h[6] as u32 | (h[7] as u32) << 16,
        dma_count: t[4] as u32 | (t[5] as u32) << 16,
        lock_polls: t[1] & 0x7FFF,
        lock_timed_out: t[1] & 0x8000 != 0,
    })
}

/// Write the header/trailer the firmware would (used by the simulator and tests).
pub fn tag_block(w: &mut [u16], t: &Tag) {
    let lock = t.lock_polls & 0x7FFF | if t.lock_timed_out { 0x8000 } else { 0 };
    let h = [0xA55A, 0x5357, t.step, t.block, t.sweep as u16, (t.sweep >> 16) as u16, t.freq_hz as u16, (t.freq_hz >> 16) as u16];
    let r = [0x5AA5, lock, t.step, t.block, t.dma_count as u16, (t.dma_count >> 16) as u16, t.freq_hz as u16, (t.freq_hz >> 16) as u16];
    w[..TAG_WORDS].copy_from_slice(&h);
    w[BLOCK_WORDS - TAG_WORDS..BLOCK_WORDS].copy_from_slice(&r);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Gain {
    Linearity(u8),
    Sensitivity(u8),
    Manual { lna: u8, mixer: u8, vga: u8 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RxSettings {
    pub port: u8,
    pub gain: Gain,
}

/// Serial to open: CORVUS_SERIAL (hex) if set, else 0, which opens the first board found.
pub fn default_serial() -> u64 {
    std::env::var("CORVUS_SERIAL")
        .ok()
        .and_then(|v| u64::from_str_radix(v.trim().trim_start_matches("0x").trim_start_matches("0X"), 16).ok())
        .unwrap_or(0)
}

/// Default library: CORVUS_LIB, else this repo's build for the board (host/build/build.sh), else the loader's.
pub fn default_lib_path(board: Board) -> PathBuf {
    if let Ok(p) = std::env::var("CORVUS_LIB") {
        return PathBuf::from(p);
    }
    let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join(board.repo_lib());
    if repo.exists() {
        return repo.canonicalize().unwrap_or(repo);
    }
    PathBuf::from(board.system_lib())
}

struct Api {
    _lib: Library,
    open_sn: unsafe extern "C" fn(*mut *mut Dev, u64) -> c_int,
    close: unsafe extern "C" fn(*mut Dev) -> c_int,
    version_string_read: unsafe extern "C" fn(*mut Dev, *mut c_char, u8) -> c_int,
    set_samplerate: unsafe extern "C" fn(*mut Dev, u32) -> c_int,
    set_sample_type: unsafe extern "C" fn(*mut Dev, c_int) -> c_int,
    set_packing: unsafe extern "C" fn(*mut Dev, u8) -> c_int,
    /// RFOne only.
    set_rf_port: Option<unsafe extern "C" fn(*mut Dev, c_int) -> c_int>,
    set_linearity_gain: unsafe extern "C" fn(*mut Dev, u8) -> c_int,
    set_sensitivity_gain: unsafe extern "C" fn(*mut Dev, u8) -> c_int,
    set_lna_gain: unsafe extern "C" fn(*mut Dev, u8) -> c_int,
    set_mixer_gain: unsafe extern "C" fn(*mut Dev, u8) -> c_int,
    set_vga_gain: unsafe extern "C" fn(*mut Dev, u8) -> c_int,
    set_lna_agc: unsafe extern "C" fn(*mut Dev, u8) -> c_int,
    set_mixer_agc: unsafe extern "C" fn(*mut Dev, u8) -> c_int,
    start_rx: unsafe extern "C" fn(*mut Dev, RxCb, *mut c_void) -> c_int,
    stop_rx: unsafe extern "C" fn(*mut Dev) -> c_int,
    error_name: unsafe extern "C" fn(c_int) -> *const c_char,
    sweep_supported: unsafe extern "C" fn(*mut Dev) -> c_int,
    sweep_fw_revision: unsafe extern "C" fn(*mut Dev) -> c_int,
    sweep_start: unsafe extern "C" fn(*mut Dev, *const SweepParams) -> c_int,
    sweep_stop: unsafe extern "C" fn(*mut Dev) -> c_int,
}

impl Api {
    fn load(path: &Path, board: Board) -> Result<Api, String> {
        let lib = unsafe { Library::new(path) }.map_err(|e| format!("cannot load {}: {e}", path.display()))?;
        let prefix = board.prefix();
        macro_rules! sym {
            ($name:literal) => {
                *unsafe { lib.get(format!("{prefix}{}\0", $name).as_bytes()) }.map_err(|_| {
                    format!(
                        "{} has no {prefix}{} — it is not the Corvus sweep library for the {} (build it with host/build/build.sh, see host/README.md)",
                        path.display(),
                        $name,
                        board.name()
                    )
                })?
            };
        }
        Ok(Api {
            open_sn: sym!("open_sn"),
            close: sym!("close"),
            version_string_read: sym!("version_string_read"),
            set_samplerate: sym!("set_samplerate"),
            set_sample_type: sym!("set_sample_type"),
            set_packing: sym!("set_packing"),
            set_rf_port: if board.has_ports() { Some(sym!("set_rf_port")) } else { None },
            set_linearity_gain: sym!("set_linearity_gain"),
            set_sensitivity_gain: sym!("set_sensitivity_gain"),
            set_lna_gain: sym!("set_lna_gain"),
            set_mixer_gain: sym!("set_mixer_gain"),
            set_vga_gain: sym!("set_vga_gain"),
            set_lna_agc: sym!("set_lna_agc"),
            set_mixer_agc: sym!("set_mixer_agc"),
            start_rx: sym!("start_rx"),
            stop_rx: sym!("stop_rx"),
            error_name: sym!("error_name"),
            sweep_supported: sym!("sweep_supported"),
            sweep_fw_revision: sym!("sweep_fw_revision"),
            sweep_start: sym!("sweep_start"),
            sweep_stop: sym!("sweep_stop"),
            _lib: lib,
        })
    }

    fn err(&self, what: &str, rc: c_int) -> String {
        let name = unsafe { CStr::from_ptr((self.error_name)(rc)) }.to_string_lossy().into_owned();
        format!("{what}: {name} ({rc})")
    }
}

/// What the RX callback needs: where to send blocks, and a counter for buffers it had to drop.
pub struct Sink {
    pub tx: SyncSender<Vec<u16>>,
    pub host_drops: Arc<AtomicU64>,
}

impl Sink {
    pub fn push(&self, buf: Vec<u16>) {
        if self.tx.try_send(buf).is_err() {
            self.host_drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

unsafe extern "C" fn rx_callback(t: *mut Transfer) -> c_int {
    // SAFETY: libhydrasdr passes a valid transfer whose ctx is the Sink we gave start_rx, which
    // outlives streaming (stop_rx runs before it is freed).
    unsafe {
        let t = &*t;
        let sink = &*(t.ctx as *const Sink);
        let n = t.sample_count.max(0) as usize;
        let samples = std::slice::from_raw_parts(t.samples as *const u16, n);
        sink.push(samples.to_vec());
    }
    0
}

/// What a signal handler needs to stop a sweep when the UI can't shut down cleanly: a host that
/// dies mid-sweep leaves the firmware sweeping, and then hydrasdr_open fails until SWEEP_STOP.
struct Emergency {
    sweep_stop: unsafe extern "C" fn(*mut Dev) -> c_int,
    stop_rx: unsafe extern "C" fn(*mut Dev) -> c_int,
    dev: usize,
}

static EMERGENCY: parking_lot::Mutex<Option<Emergency>> = parking_lot::Mutex::new(None);

/// Stop the sweep and the stream of the open device, if any (for signal handlers).
pub fn emergency_stop() {
    if let Some(e) = EMERGENCY.lock().take() {
        unsafe {
            (e.sweep_stop)(e.dev as *mut Dev);
            (e.stop_rx)(e.dev as *mut Dev);
        }
    }
}

pub struct Device {
    api: Api,
    dev: *mut Dev,
    sink: *mut Sink,
    pub version: String,
    pub fw_revision: i32,
    pub lib_path: PathBuf,
    board: Board,
    plan: Option<Plan>,
    rx: RxSettings,
    rate: Rate,
    streaming: bool,
}

// SAFETY: the handle is only used from the thread that owns the Device (the UI thread).
unsafe impl Send for Device {}

impl Device {
    pub fn open(lib_path: &Path, serial: u64, sink: Sink, rx: RxSettings, rate: Rate) -> Result<Device, String> {
        let board = board();
        let api = Api::load(lib_path, board)?;
        let mut dev = std::ptr::null_mut();
        let rc = unsafe { (api.open_sn)(&mut dev, serial) };
        if rc != 0 {
            let which = if serial == 0 { format!("the {}", board.name()) } else { format!("{} {serial:#018X}", board.name()) };
            return Err(api.err(&format!("open {which} (unplugged, or busy in another program?)"), rc));
        }
        let mut buf = [0 as c_char; 255];
        unsafe { (api.version_string_read)(dev, buf.as_mut_ptr(), 255) };
        let version = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
        let supported = unsafe { (api.sweep_supported)(dev) };
        let fw_revision = unsafe { (api.sweep_fw_revision)(dev) };
        let mut d = Device {
            api,
            dev,
            sink: Box::into_raw(Box::new(sink)),
            version,
            fw_revision,
            lib_path: lib_path.to_path_buf(),
            board,
            plan: None,
            rx,
            rate,
            streaming: false,
        };
        if supported != 1 {
            return Err(format!("firmware \"{}\" has no sweep mode (needs the Corvus firmware, \"...-sweepN\"; see firmware/README.md)", d.version));
        }
        d.configure()?;
        *EMERGENCY.lock() = Some(Emergency { sweep_stop: d.api.sweep_stop, stop_rx: d.api.stop_rx, dev: d.dev as usize });
        Ok(d)
    }

    fn check(&self, what: &str, rc: c_int) -> Result<(), String> {
        if rc == 0 { Ok(()) } else { Err(self.api.err(what, rc)) }
    }

    fn configure(&mut self) -> Result<(), String> {
        // RAW first: it decides how the rate below is read
        unsafe { self.check("set_sample_type RAW", (self.api.set_sample_type)(self.dev, SAMPLE_RAW))? };
        self.apply_rate()?;
        unsafe { self.check("set_packing off", (self.api.set_packing)(self.dev, 0))? };
        self.apply_rx()
    }

    /// Set the sample rate (streaming stopped, sample type RAW). The firmware retunes the tuner IF
    /// and its filter with it.
    ///
    /// Under RAW (and the REAL types) a rate given by value is the real ADC rate: 20 or 24 MSPS.
    /// Asking for "12000000" under RAW selects the firmware's 6 MSPS IQ setting (12 MSPS real); only
    /// 10000000 happens to work either way, because it matches an IQ rate-table index.
    ///
    /// libairspy's table holds its settings by IQ rate (R2 10000000, Mini 6000000), which selects them
    /// under RAW too.
    fn apply_rate(&self) -> Result<(), String> {
        let hz = match self.board {
            Board::RFOne => self.rate.adc_hz() as u32,
            Board::Airspy | Board::Mini => self.rate.msps() * 1_000_000,
        };
        let m = self.rate.msps();
        unsafe { self.check(&format!("set_samplerate {m} MSPS ({hz} real)"), (self.api.set_samplerate)(self.dev, hz)) }
    }

    fn apply_rx(&self) -> Result<(), String> {
        let a = &self.api;
        let d = self.dev;
        unsafe {
            if let Some(set_rf_port) = a.set_rf_port {
                self.check("set_rf_port", set_rf_port(d, self.rx.port as c_int))?;
            }
            match self.rx.gain {
                Gain::Linearity(g) => self.check("set_linearity_gain", (a.set_linearity_gain)(d, g)),
                Gain::Sensitivity(g) => self.check("set_sensitivity_gain", (a.set_sensitivity_gain)(d, g)),
                Gain::Manual { lna, mixer, vga } => {
                    self.check("set_lna_agc off", (a.set_lna_agc)(d, 0))?;
                    self.check("set_mixer_agc off", (a.set_mixer_agc)(d, 0))?;
                    self.check("set_lna_gain", (a.set_lna_gain)(d, lna))?;
                    self.check("set_mixer_gain", (a.set_mixer_gain)(d, mixer))?;
                    self.check("set_vga_gain", (a.set_vga_gain)(d, vga))
                }
            }
        }
    }

    fn start_stream(&mut self) -> Result<(), String> {
        if !self.streaming {
            let rc = unsafe { (self.api.start_rx)(self.dev, rx_callback, self.sink as *mut c_void) };
            self.check("start_rx", rc)?;
            self.streaming = true;
        }
        Ok(())
    }

    /// Start streaming if needed and (re)start the sweep with this plan. A running sweep is replaced
    /// without a stop.
    pub fn sweep(&mut self, plan: &Plan) -> Result<(), String> {
        if plan.rate != self.rate {
            // the rate request is refused mid-sweep: stop everything, change it, start again
            self.pause();
            self.rate = plan.rate;
            self.apply_rate()?;
        }
        self.start_stream()?;
        let p = SweepParams::from(plan);
        let rc = unsafe { (self.api.sweep_start)(self.dev, &p) };
        self.check("sweep_start", rc)?;
        self.plan = Some(plan.clone());
        Ok(())
    }

    /// Tuner, gain and port requests are STALLed while sweeping: stop, change, restart.
    pub fn set_rx(&mut self, rx: RxSettings) -> Result<(), String> {
        self.rx = rx;
        let sweeping = self.streaming && self.plan.is_some();
        if sweeping {
            unsafe { (self.api.sweep_stop)(self.dev) };
        }
        let r = self.apply_rx();
        if sweeping {
            let plan = self.plan.clone().unwrap();
            self.sweep(&plan)?;
        }
        r
    }

    /// Stop the sweep and the stream (otherwise normal mode keeps streaming 40–48 MB/s of raw samples).
    pub fn pause(&mut self) {
        if self.streaming {
            unsafe {
                (self.api.sweep_stop)(self.dev);
                (self.api.stop_rx)(self.dev);
            }
            self.streaming = false;
        }
    }

    pub fn resume(&mut self) -> Result<(), String> {
        let plan = self.plan.clone().ok_or("no plan")?;
        // stop_rx leaves the sample type alone, but be explicit: sweep_start refuses anything but RAW.
        unsafe { self.check("set_sample_type RAW", (self.api.set_sample_type)(self.dev, SAMPLE_RAW))? };
        self.sweep(&plan)
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        EMERGENCY.lock().take();
        self.pause();
        unsafe {
            (self.api.close)(self.dev);
            drop(Box::from_raw(self.sink));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_round_trip_and_torn_blocks_are_caught() {
        let mut w = vec![2048u16; BLOCK_WORDS];
        assert_eq!(parse_block(&w), Block::Untagged);
        let t = Tag { step: 3, block: 0, sweep: 70_001, freq_hz: 500_000_000, dma_count: 1234, lock_polls: 5, lock_timed_out: true };
        tag_block(&mut w, &t);
        assert_eq!(parse_block(&w), Block::Valid(t));
        w[BLOCK_WORDS - 6] ^= 1;
        assert_eq!(parse_block(&w), Block::Torn);
    }

    #[test]
    fn params_are_16_bytes() {
        assert_eq!(std::mem::size_of::<SweepParams>(), 16);
    }
}
