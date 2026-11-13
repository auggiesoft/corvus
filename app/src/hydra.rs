//! The R82x boards (RFOne, Airspy R2/Mini) driven in firmware sweep mode (or PC-driven on stock firmware),
//! through the built-in USB driver (`usbdev`, the default) or the board's C library with the Corvus sweep
//! API (libhydrasdr, libairspy; loaded at run time, see ../host/README.md). The two libraries share one API
//! shape under different prefixes, and the same tags and sweep parameters.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;

use libloading::Library;

pub use crate::plan::BLOCK_WORDS;
use crate::plan::{Plan, Rate};

pub const TAG_WORDS: usize = 8;
const SAMPLE_RAW: c_int = 5;

/// Which board Corvus Sweep drives. Both run the same Corvus sweep protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Board {
    RFOne,
    /// Airspy R2.
    Airspy,
    /// Airspy Mini: the same firmware image as the R2, which picks the Mini's settings at boot.
    Mini,
    /// HackRF One, with its stock firmware sweep (libhackrf): 1 MHz to 6 GHz.
    HackRF,
    /// PlutoSDR family (ADALM-Pluto, Pluto+), over the network to corvus-pluto on the board: ~18 MHz to 6.18 GHz.
    Pluto,
    /// Any IQ radio a SoapySDR module opens (RTL-SDR, LimeSDR, USRP…), swept from the PC (`soapy`).
    Soapy,
    /// An RTL2832U dongle through corvus-rtl's librtlsdr (downloaded on first use), swept from the PC (`rtl`).
    Rtl,
}

impl Board {
    pub fn parse(s: &str) -> Option<Board> {
        match s.to_ascii_lowercase().as_str() {
            "rfone" => Some(Board::RFOne),
            "airspy" | "r2" => Some(Board::Airspy),
            "mini" | "airspy-mini" => Some(Board::Mini),
            "hackrf" => Some(Board::HackRF),
            "pluto" | "plutoplus" | "pluto+" => Some(Board::Pluto),
            "soapy" | "soapysdr" => Some(Board::Soapy),
            "rtl" | "rtlsdr" | "rtl-sdr" => Some(Board::Rtl),
            _ => None,
        }
    }

    /// Name for messages.
    pub fn name(self) -> &'static str {
        match self {
            Board::RFOne => "RFOne",
            Board::Airspy => "Airspy R2",
            Board::Mini => "Airspy Mini",
            Board::HackRF => "HackRF One",
            Board::Pluto => "Pluto",
            Board::Soapy => crate::soapy::name(),
            Board::Rtl => crate::rtl::name(),
        }
    }

    /// Reached over the network rather than USB (no library, no serial).
    pub fn networked(self) -> bool {
        self == Board::Pluto
    }

    fn prefix(self) -> &'static str {
        match self {
            Board::RFOne => "hydrasdr_",
            Board::Airspy | Board::Mini => "airspy_",
            Board::HackRF => "hackrf_",
            Board::Pluto | Board::Soapy | Board::Rtl => "",
        }
    }

    /// The RFOne has ANT, CABLE1 and CABLE2; the R2 has one antenna input.
    /// The live view (one step, the board parked on a frequency): the Pluto so far.
    pub fn has_live(self) -> bool {
        self == Board::Pluto
    }

    pub fn has_ports(self) -> bool {
        !self.ports().is_empty()
    }

    /// The RF inputs to choose from (RxSettings::port): the RFOne's ANT and cable inputs, the Pluto's RX1
    /// and RX2 (on a board in 1R1T with corvus-pluto 0.2.3+). The others have one.
    pub fn ports(self) -> &'static [(u8, &'static str)] {
        match self {
            Board::RFOne => &[(0, "ANT"), (1, "CABLE1"), (2, "CABLE2")],
            Board::Pluto => &[(0, "RX1"), (1, "RX2")],
            _ => &[],
        }
    }

    /// The sample-rate settings the sweep can use on this board, default first.
    pub fn rates(self) -> &'static [Rate] {
        match self {
            Board::RFOne => &[Rate::M12, Rate::M10],
            Board::Airspy => &[Rate::M10],
            Board::Mini => &[Rate::M6],
            Board::HackRF => &[Rate::H20],
            Board::Pluto => &[Rate::P61, Rate::P31, Rate::P15, Rate::P7, Rate::P3],
            Board::Soapy => crate::soapy::rates(),
            Board::Rtl => crate::rtl::RATES,
        }
    }

    /// The library file this repo builds for the board (host/build/build.sh), and where it lands.
    fn lib_file(self) -> &'static str {
        match self {
            Board::RFOne if cfg!(windows) => "libhydrasdr.dll",
            Board::Airspy | Board::Mini if cfg!(windows) => "libairspy.dll",
            Board::HackRF if cfg!(windows) => "hackrf.dll",
            Board::RFOne => "libhydrasdr.so.1.1.4",
            Board::Airspy | Board::Mini => "libairspy.so.1.0.10",
            Board::HackRF => "libhackrf.so.0",
            Board::Pluto | Board::Soapy | Board::Rtl => "",
        }
    }

    fn repo_lib(self) -> String {
        let os = if cfg!(windows) { "windows" } else { "linux" };
        let dir = match self {
            Board::RFOne => os.to_string(),
            Board::Airspy | Board::Mini => format!("airspy-{os}"),
            Board::HackRF => format!("hackrf-{os}"), // not built here: the system libhackrf is used
            Board::Pluto | Board::Soapy | Board::Rtl => "none".into(),
        };
        format!("../host/build/out/{dir}/{}", self.lib_file())
    }

    fn system_lib(self) -> &'static str {
        match self {
            _ if cfg!(windows) => self.lib_file(),
            Board::RFOne => "libhydrasdr.so.1",
            Board::Airspy | Board::Mini => "libairspy.so.0",
            Board::HackRF => "libhackrf.so.0",
            Board::Pluto | Board::Soapy | Board::Rtl => "",
        }
    }

    /// Prefix for files that hold what was learned from this board (its passband differs).
    pub fn cache_prefix(self) -> &'static str {
        match self {
            Board::RFOne => "",
            Board::Airspy => "airspy-",
            Board::Mini => "airspy-mini-",
            Board::HackRF => "hackrf-",
            Board::Pluto => "pluto-",
            Board::Soapy => crate::soapy::prefix(),
            Board::Rtl => "rtl-",
        }
    }
}

static BOARD: parking_lot::RwLock<Board> = parking_lot::const_rwlock(Board::RFOne);

/// The board chosen at startup (`--board`, CORVUS_BOARD); the RFOne unless set.
pub fn board() -> Board {
    *BOARD.read()
}

/// Switch boards (at startup, or from the device picker with no device open).
pub fn set_board(b: Board) {
    *BOARD.write() = b;
}

/// A board found on USB (nothing is opened). Airspy R2 and Mini share an ID, so they show
/// as Airspy until one is opened and its firmware says which.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    pub board: Board,
    pub serial: u64,
    /// "Airspy (R2 or Mini)" until known.
    pub label: String,
}

/// Boards on USB, by vendor/product ID and serial string (any platform).
pub fn list_devices() -> Vec<Found> {
    let mut found = crate::usbdev::list();
    // a Pluto given by address (--pluto, CORVUS_PLUTO) may not be on this PC's USB at all
    if crate::pluto::addr_given() && !found.iter().any(|f| f.board == Board::Pluto) {
        found.push(Found { board: Board::Pluto, serial: 0, label: "Pluto".into() });
    }
    found
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
            f_start_hz: p.f0 as u32,
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

/// Default library: CORVUS_LIB, else one next to the executable (release archives), else this repo's build
/// for the board (host/build/build.sh), else the loader's.
pub fn default_lib_path(board: Board) -> PathBuf {
    if let Ok(p) = std::env::var("CORVUS_LIB") {
        return PathBuf::from(p);
    }
    if let Some(beside) = std::env::current_exe().ok().and_then(|e| e.parent().map(|d| d.join(board.lib_file())))
        && beside.exists()
    {
        return beside;
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
    /// The Corvus sweep API: absent from a stock library (then the sweep is PC-driven).
    sweep_supported: Option<unsafe extern "C" fn(*mut Dev) -> c_int>,
    sweep_fw_revision: Option<unsafe extern "C" fn(*mut Dev) -> c_int>,
    sweep_start: Option<unsafe extern "C" fn(*mut Dev, *const SweepParams) -> c_int>,
    sweep_stop: Option<unsafe extern "C" fn(*mut Dev) -> c_int>,
    set_freq: Option<SetFreq>,
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
        macro_rules! opt {
            ($name:literal) => {
                unsafe { lib.get(format!("{prefix}{}\0", $name).as_bytes()) }.ok().map(|f| *f)
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
            sweep_supported: opt!("sweep_supported"),
            sweep_fw_revision: opt!("sweep_fw_revision"),
            sweep_start: opt!("sweep_start"),
            sweep_stop: opt!("sweep_stop"),
            set_freq: match board {
                Board::RFOne => opt!("set_freq").map(SetFreq::U64),
                _ => opt!("set_freq").map(SetFreq::U32),
            },
            _lib: lib,
        })
    }

    fn err(&self, what: &str, rc: c_int) -> String {
        let name = unsafe { CStr::from_ptr((self.error_name)(rc)) }.to_string_lossy().into_owned();
        format!("{what}: {name} ({rc})")
    }
}

/// What the RX callback needs: where to send blocks, a counter for buffers it had to drop, and (for a
/// PC-driven sweep) the sweeper it tags blocks for.
pub struct Sink {
    pub tx: SyncSender<Vec<u16>>,
    pub host_drops: Arc<AtomicU64>,
    pub host: Option<Arc<HostShared>>,
}

impl Sink {
    pub fn push(&self, buf: Vec<u16>) {
        if self.tx.try_send(buf).is_err() {
            self.host_drops.fetch_add(1, Ordering::Relaxed);
        }
    }
}

unsafe extern "C" fn rx_callback(t: *mut Transfer) -> c_int {
    // SAFETY: the library passes a valid transfer whose ctx is the Sink we gave start_rx, which
    // outlives streaming (stop_rx runs before it is freed).
    unsafe {
        let t = &*t;
        let sink = &*(t.ctx as *const Sink);
        let n = t.sample_count.max(0) as usize;
        let samples = std::slice::from_raw_parts(t.samples as *const u16, n);
        match &sink.host {
            Some(h) => h.on_transfer(samples, sink),
            None => sink.push(samples.to_vec()),
        }
    }
    0
}

/// A PC-driven sweep for boards whose firmware has no sweep mode (stock RFOne and Airspy firmware), or
/// when forced (`--host-sweep`). The sweeper thread retunes with set_freq and then arms the RX callback,
/// which tags the first whole blocks of the second USB transfer that starts afterwards (the one in flight
/// may hold samples from before the retune, the next one samples buffered in the board before it) as the
/// firmware would and passes them on; everything else is dropped. Same tags and visiting order as the
/// firmware, so the engine can't tell the difference, only that it's slower (a step costs ~2-3 transfers
/// of 131072 samples, 6.5 ms each at 20 MSPS real).
pub struct HostShared {
    state: parking_lot::Mutex<HostState>,
    cv: parking_lot::Condvar,
    transfers: AtomicU64,
}

#[derive(Default)]
struct HostState {
    armed: bool,
    accept_from: u64,
    step: u16,
    freq_hz: u32,
    sweep: u32,
    block: u16,
    left: u16,
}

/// Whole transfers skipped after a retune before blocks are taken (see `HostShared`).
const HOST_SETTLE_TRANSFERS: u64 = 2;

impl HostShared {
    fn new() -> HostShared {
        HostShared { state: parking_lot::Mutex::new(HostState::default()), cv: parking_lot::Condvar::new(), transfers: AtomicU64::new(0) }
    }

    fn on_transfer(&self, samples: &[u16], sink: &Sink) {
        let idx = self.transfers.fetch_add(1, Ordering::AcqRel);
        let mut st = self.state.lock();
        if !st.armed || idx < st.accept_from {
            return;
        }
        let per = (samples.len() / BLOCK_WORDS) as u64;
        for (i, chunk) in samples.chunks_exact(BLOCK_WORDS).enumerate() {
            if st.left == 0 {
                break;
            }
            let mut w = chunk.to_vec();
            let dma = (idx * per + i as u64) as u32;
            tag_block(&mut w, &Tag { step: st.step, block: st.block, sweep: st.sweep, freq_hz: st.freq_hz, dma_count: dma, lock_polls: 0, lock_timed_out: false });
            sink.push(w);
            st.block += 1;
            st.left -= 1;
        }
        if st.left == 0 {
            st.armed = false;
            self.cv.notify_all();
        }
    }
}

#[derive(Clone, Copy)]
enum SetFreq {
    U64(unsafe extern "C" fn(*mut Dev, u64) -> c_int),
    U32(unsafe extern "C" fn(*mut Dev, u32) -> c_int),
}

struct HostSweeper {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

/// Whether a PC-driven sweep is forced even on sweep firmware (`--host-sweep`, for testing).
static FORCE_HOST: AtomicBool = AtomicBool::new(false);

pub fn set_force_host_sweep(on: bool) {
    FORCE_HOST.store(on, Ordering::Relaxed);
}

/// What a signal handler runs to stop the open board's sweep and stream when the UI can't shut down
/// cleanly: a host that dies mid-sweep leaves the firmware sweeping, and the next open fails until
/// SWEEP_STOP.
pub(crate) type Emergency = Box<dyn Fn() + Send>;

pub(crate) static EMERGENCY: parking_lot::Mutex<Option<Emergency>> = parking_lot::Mutex::new(None);

/// Stop the sweep and the stream of the open device, if any (for signal handlers).
pub fn emergency_stop() {
    if let Some(f) = EMERGENCY.lock().take() {
        f();
    }
}

/// How a board is reached: its C library (libhydrasdr, libairspy, libhackrf), or the built-in USB driver
/// (`usbdev`), which needs no library.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsbMode {
    /// The built-in driver (the default).
    Native,
    /// The board's C library (`--usb lib`).
    Lib,
}

static USB_MODE: parking_lot::Mutex<UsbMode> = parking_lot::const_mutex(UsbMode::Native);

pub fn set_usb_mode(m: UsbMode) {
    *USB_MODE.lock() = m;
}

/// What `Device::lib_path` shows when no library is involved.
pub const NATIVE_LIB: &str = "built-in USB driver";

/// Whether to use the built-in driver for this board (every board has one).
pub(crate) fn native_for(_board: Board, _lib_path: &Path) -> bool {
    *USB_MODE.lock() == UsbMode::Native
}

/// The open board: an R82x board (RFOne or Airspy; firmware or PC-driven sweep), a HackRF or a Pluto.
pub enum Device {
    R82x(R82xDevice),
    HackRF(crate::hackrf::HackrfDevice),
    Pluto(crate::pluto::PlutoDevice),
    Soapy(crate::soapy::SoapyDevice),
    Rtl(crate::rtl::RtlDevice),
}

impl Device {
    pub fn open(lib_path: &Path, serial: u64, sink: Sink, rx: RxSettings, rate: Rate) -> Result<Device, String> {
        match board() {
            Board::HackRF => crate::hackrf::HackrfDevice::open(lib_path, serial, sink, rx).map(Device::HackRF),
            Board::Pluto => crate::pluto::PlutoDevice::open(sink, rx).map(Device::Pluto),
            Board::Soapy => crate::soapy::SoapyDevice::open(serial, sink, rx).map(Device::Soapy),
            Board::Rtl => crate::rtl::RtlDevice::open(serial, sink, rx).map(Device::Rtl),
            _ => R82xDevice::open(lib_path, serial, sink, rx, rate).map(Device::R82x),
        }
    }

    pub fn version(&self) -> &str {
        match self {
            Device::R82x(d) => &d.version,
            Device::HackRF(d) => &d.version,
            Device::Pluto(d) => &d.version,
            Device::Soapy(d) => &d.version,
            Device::Rtl(d) => &d.version,
        }
    }

    pub fn fw_revision(&self) -> i32 {
        match self {
            Device::R82x(d) => d.fw_revision,
            Device::HackRF(_) | Device::Pluto(_) | Device::Soapy(_) | Device::Rtl(_) => 0,
        }
    }

    /// The key of the radio actually opened, where it can differ from the one asked for (an RTL-SDR dongle
    /// opened as "the first one", or one of several sharing a serial).
    pub fn opened_serial(&self) -> Option<u64> {
        match self {
            Device::Rtl(d) => Some(d.key),
            _ => None,
        }
    }

    pub fn lib_path(&self) -> &Path {
        match self {
            Device::R82x(d) => &d.lib_path,
            Device::HackRF(d) => &d.lib_path,
            Device::Pluto(d) => &d.lib_path,
            Device::Soapy(d) => &d.lib_path,
            Device::Rtl(d) => &d.lib_path,
        }
    }

    /// The PC retunes each step (stock firmware, or forced): much slower than a firmware sweep.
    pub fn host_driven(&self) -> bool {
        matches!(self, Device::R82x(d) if d.set_freq.is_some() && d.host_mode)
    }

    pub fn sweep(&mut self, plan: &Plan) -> Result<(), String> {
        match self {
            Device::R82x(d) => d.sweep(plan),
            Device::HackRF(d) => d.sweep(plan),
            Device::Pluto(d) => d.sweep(plan),
            Device::Soapy(d) => d.sweep(plan),
            Device::Rtl(d) => d.sweep(plan),
        }
    }

    pub fn set_rx(&mut self, rx: RxSettings) -> Result<(), String> {
        match self {
            Device::R82x(d) => d.set_rx(rx),
            Device::HackRF(d) => d.set_rx(rx),
            Device::Pluto(d) => d.set_rx(rx),
            Device::Soapy(d) => d.set_rx(rx),
            Device::Rtl(d) => d.set_rx(rx),
        }
    }

    pub fn pause(&mut self) {
        match self {
            Device::R82x(d) => d.pause(),
            Device::HackRF(d) => d.pause(),
            Device::Pluto(d) => d.pause(),
            Device::Soapy(d) => d.pause(),
            Device::Rtl(d) => d.pause(),
        }
    }

    pub fn resume(&mut self) -> Result<(), String> {
        match self {
            Device::R82x(d) => d.resume(),
            Device::HackRF(d) => d.resume(),
            Device::Pluto(d) => d.resume(),
            Device::Soapy(d) => d.resume(),
            Device::Rtl(d) => d.resume(),
        }
    }
}

/// Retune from the PC-driven sweeper thread.
type FreqFn = Arc<dyn Fn(u64) + Send + Sync>;

/// How the R82x board is driven: through its library, or the built-in USB driver.
enum Io {
    Lib { api: Api, dev: *mut Dev, sink: *mut Sink },
    Native { usb: crate::usbdev::R82x, sink: Arc<Sink> },
}

fn rc_check(api: &Api, what: &str, rc: c_int) -> Result<(), String> {
    if rc == 0 { Ok(()) } else { Err(api.err(what, rc)) }
}

pub struct R82xDevice {
    io: Io,
    pub version: String,
    pub fw_revision: i32,
    pub lib_path: PathBuf,
    board: Board,
    plan: Option<Plan>,
    rx: RxSettings,
    rate: Rate,
    streaming: bool,
    /// PC-driven sweep (no sweep firmware, or forced).
    host_mode: bool,
    set_freq: Option<FreqFn>,
    host: Option<Arc<HostShared>>,
    sweeper: Option<HostSweeper>,
    /// Pass counter of the PC-driven sweep: like the firmware's, it never resets.
    host_sweep_no: Arc<AtomicU64>,
}

// SAFETY: a library handle is only used from the thread that owns the Device (the UI thread), and by the
// PC-driven sweeper thread for set_freq, which the device stops before anything else touches the stream.
unsafe impl Send for R82xDevice {}

/// The board as first opened, before the sink is attached.
enum Opened {
    Lib(Api, *mut Dev),
    Native(crate::usbdev::R82x),
}

impl R82xDevice {
    pub fn open(lib_path: &Path, serial: u64, mut sink: Sink, rx: RxSettings, rate: Rate) -> Result<R82xDevice, String> {
        let board = board();
        let opened = if native_for(board, lib_path) {
            let kind = if board == Board::RFOne { crate::usbdev::Kind::RFOne } else { crate::usbdev::Kind::Airspy };
            Opened::Native(crate::usbdev::R82x::open(kind, serial)?)
        } else {
            let api = Api::load(lib_path, board)?;
            let mut dev = std::ptr::null_mut();
            let rc = unsafe { (api.open_sn)(&mut dev, serial) };
            if rc != 0 {
                let which = if serial == 0 { format!("the {}", board.name()) } else { format!("{} {serial:#018X}", board.name()) };
                return Err(api.err(&format!("open {which} (unplugged, or busy in another program?)"), rc));
            }
            Opened::Lib(api, dev)
        };
        let (version, fw_revision, set_freq, shown_path) = match &opened {
            Opened::Native(usb) => {
                let f = usb.setter();
                let set: FreqFn = Arc::new(move |hz| {
                    let _ = f.set(hz);
                });
                (usb.version.clone(), usb.sweep_revision(), Some(set), PathBuf::from(NATIVE_LIB))
            }
            Opened::Lib(api, dev) => {
                let mut buf = [0 as c_char; 255];
                unsafe { (api.version_string_read)(*dev, buf.as_mut_ptr(), 255) };
                let version = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy().into_owned();
                let supported = api.sweep_supported.map(|f| unsafe { f(*dev) }).unwrap_or(0);
                let rev = if supported == 1 { api.sweep_fw_revision.map(|f| unsafe { f(*dev) }).unwrap_or(1).max(1) } else { 0 };
                let set: Option<FreqFn> = api.set_freq.map(|sf| {
                    let d = *dev as usize;
                    let f: FreqFn = Arc::new(move |hz| unsafe {
                        match sf {
                            SetFreq::U64(f) => f(d as *mut Dev, hz),
                            SetFreq::U32(f) => f(d as *mut Dev, hz as u32),
                        };
                    });
                    f
                });
                (version, rev, set, lib_path.to_path_buf())
            }
        };
        // The R2 and the Mini share a USB ID and one firmware image; the version string says which.
        let (board, rate) = match board {
            Board::Airspy if version.contains("MINI") => (Board::Mini, Rate::M6),
            Board::Mini if version.contains("NOS") => (Board::Airspy, Rate::M10),
            b => (b, rate),
        };
        set_board(board);
        let host_mode = fw_revision == 0 || FORCE_HOST.load(Ordering::Relaxed);
        let host = if host_mode { Some(Arc::new(HostShared::new())) } else { None };
        sink.host = host.clone();
        let io = match opened {
            Opened::Lib(api, dev) => Io::Lib { api, dev, sink: Box::into_raw(Box::new(sink)) },
            Opened::Native(usb) => Io::Native { usb, sink: Arc::new(sink) },
        };
        let mut d = R82xDevice {
            io,
            version,
            fw_revision,
            lib_path: shown_path,
            board,
            plan: None,
            rx,
            rate,
            streaming: false,
            host_mode,
            set_freq,
            host,
            sweeper: None,
            host_sweep_no: Arc::new(AtomicU64::new(0)),
        };
        if host_mode && d.set_freq.is_none() {
            return Err(format!("firmware \"{}\" has no sweep mode and the library has no set_freq", d.version));
        }
        d.configure()?;
        let stop: Emergency = match &d.io {
            Io::Lib { api, dev, .. } => {
                let (a, b, dev) = (api.sweep_stop.unwrap_or(api.stop_rx), api.stop_rx, *dev as usize);
                Box::new(move || unsafe {
                    a(dev as *mut Dev);
                    b(dev as *mut Dev);
                })
            }
            Io::Native { usb, .. } => Box::new(usb.stopper()),
        };
        *EMERGENCY.lock() = Some(stop);
        Ok(d)
    }

    fn configure(&mut self) -> Result<(), String> {
        // RAW first: it decides how the rate below is read (the built-in driver is always RAW)
        if let Io::Lib { api, dev, .. } = &self.io {
            rc_check(api, "set_sample_type RAW", unsafe { (api.set_sample_type)(*dev, SAMPLE_RAW) })?;
        }
        self.apply_rate()?;
        match &self.io {
            Io::Lib { api, dev, .. } => rc_check(api, "set_packing off", unsafe { (api.set_packing)(*dev, 0) })?,
            Io::Native { usb, .. } => usb.set_packing(false)?,
        }
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
            _ => self.rate.msps() * 1_000_000,
        };
        let m = self.rate.msps();
        match &self.io {
            Io::Lib { api, dev, .. } => rc_check(api, &format!("set_samplerate {m} MSPS ({hz} real)"), unsafe { (api.set_samplerate)(*dev, hz) }),
            Io::Native { usb, .. } => usb.set_samplerate(hz),
        }
    }

    fn apply_rx(&self) -> Result<(), String> {
        match &self.io {
            Io::Lib { api: a, dev, .. } => {
                let d = *dev;
                unsafe {
                    if let Some(set_rf_port) = a.set_rf_port {
                        rc_check(a, "set_rf_port", set_rf_port(d, self.rx.port as c_int))?;
                    }
                    match self.rx.gain {
                        Gain::Linearity(g) => rc_check(a, "set_linearity_gain", (a.set_linearity_gain)(d, g)),
                        Gain::Sensitivity(g) => rc_check(a, "set_sensitivity_gain", (a.set_sensitivity_gain)(d, g)),
                        Gain::Manual { lna, mixer, vga } => {
                            rc_check(a, "set_lna_agc off", (a.set_lna_agc)(d, 0))?;
                            rc_check(a, "set_mixer_agc off", (a.set_mixer_agc)(d, 0))?;
                            rc_check(a, "set_lna_gain", (a.set_lna_gain)(d, lna))?;
                            rc_check(a, "set_mixer_gain", (a.set_mixer_gain)(d, mixer))?;
                            rc_check(a, "set_vga_gain", (a.set_vga_gain)(d, vga))
                        }
                    }
                }
            }
            Io::Native { usb, .. } => {
                if self.board.has_ports() {
                    usb.set_rf_port(self.rx.port)?;
                }
                match self.rx.gain {
                Gain::Linearity(g) => usb.set_linearity_gain(g),
                Gain::Sensitivity(g) => usb.set_sensitivity_gain(g),
                Gain::Manual { lna, mixer, vga } => {
                    usb.set_lna_agc(false)?;
                    usb.set_mixer_agc(false)?;
                    usb.set_lna_gain(lna)?;
                    usb.set_mixer_gain(mixer)?;
                    usb.set_vga_gain(vga)
                }
                }
            }
        }
    }

    fn start_stream(&mut self) -> Result<(), String> {
        if !self.streaming {
            match &mut self.io {
                Io::Lib { api, dev, sink } => rc_check(api, "start_rx", unsafe { (api.start_rx)(*dev, rx_callback, *sink as *mut c_void) })?,
                Io::Native { usb, sink } => {
                    let sink = sink.clone();
                    usb.start_rx(move |words| match &sink.host {
                        Some(h) => h.on_transfer(&words, &sink),
                        None => sink.push(words),
                    })?
                }
            }
            self.streaming = true;
        }
        Ok(())
    }

    fn sweep_stop(&self) {
        match &self.io {
            Io::Lib { api, dev, .. } => {
                if let Some(f) = api.sweep_stop {
                    unsafe { f(*dev) };
                }
            }
            Io::Native { usb, .. } => {
                let _ = usb.sweep_stop();
            }
        }
    }

    fn stop_sweeper(&mut self) {
        if let Some(s) = self.sweeper.take() {
            s.stop.store(true, Ordering::Relaxed);
            if let Some(h) = &self.host {
                let mut st = h.state.lock();
                st.armed = false;
                h.cv.notify_all();
            }
            let _ = s.thread.join();
        }
    }

    fn start_sweeper(&mut self, plan: &Plan) {
        let (Some(set_freq), Some(host)) = (self.set_freq.clone(), self.host.clone()) else { return };
        let stop = Arc::new(AtomicBool::new(false));
        let (plan, stop2, sweep_no) = (plan.clone(), stop.clone(), self.host_sweep_no.clone());
        let thread = std::thread::Builder::new()
            .name("corvus-host-sweep".into())
            .spawn(move || host_sweep(set_freq, plan, host, stop2, sweep_no))
            .expect("spawn host sweeper");
        self.sweeper = Some(HostSweeper { stop, thread });
    }

    /// Start streaming if needed and (re)start the sweep with this plan. A running firmware sweep is
    /// replaced without a stop.
    pub fn sweep(&mut self, plan: &Plan) -> Result<(), String> {
        self.stop_sweeper();
        if plan.rate != self.rate {
            // the rate request is refused mid-sweep: stop everything, change it, start again
            self.pause();
            self.rate = plan.rate;
            self.apply_rate()?;
        }
        self.start_stream()?;
        if self.host_mode {
            self.start_sweeper(plan);
        } else {
            let p = SweepParams::from(plan);
            match &self.io {
                Io::Lib { api, dev, .. } => {
                    let f = api.sweep_start.ok_or("the library has no sweep_start")?;
                    rc_check(api, "sweep_start", unsafe { f(*dev, &p) })?;
                }
                Io::Native { usb, .. } => usb.sweep_start(&p)?,
            }
        }
        self.plan = Some(plan.clone());
        Ok(())
    }

    /// Tuner, gain and port requests are STALLed while the firmware sweeps: stop, change, restart. In a
    /// PC-driven sweep the sweeper is stopped instead, so its set_freq calls never interleave.
    pub fn set_rx(&mut self, rx: RxSettings) -> Result<(), String> {
        self.rx = rx;
        let sweeping = self.streaming && self.plan.is_some();
        if sweeping {
            if self.host_mode {
                self.stop_sweeper();
            } else {
                self.sweep_stop();
            }
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
        self.stop_sweeper();
        if self.streaming {
            if !self.host_mode {
                self.sweep_stop();
            }
            match &mut self.io {
                Io::Lib { api, dev, .. } => unsafe {
                    (api.stop_rx)(*dev);
                },
                Io::Native { usb, .. } => {
                    let _ = usb.stop_rx();
                }
            }
            self.streaming = false;
        }
    }

    pub fn resume(&mut self) -> Result<(), String> {
        let plan = self.plan.clone().ok_or("no plan")?;
        // stop_rx leaves the sample type alone, but be explicit: sweep_start refuses anything but RAW.
        if let Io::Lib { api, dev, .. } = &self.io {
            rc_check(api, "set_sample_type RAW", unsafe { (api.set_sample_type)(*dev, SAMPLE_RAW) })?;
        }
        self.sweep(&plan)
    }
}

/// The PC-driven sweep loop: the firmware's visiting order (VCO-continuous passes alternate direction),
/// the pass counter bumped on every start and every pass.
fn host_sweep(set_freq: FreqFn, plan: Plan, host: Arc<HostShared>, stop: Arc<AtomicBool>, sweep_no: Arc<AtomicU64>) {
    let seq = plan.sequence();
    let vco = plan.order == crate::plan::Order::VcoContinuous;
    let mut forward = true;
    let mut sweep = sweep_no.fetch_add(1, Ordering::Relaxed) as u32 + 1;
    while !stop.load(Ordering::Relaxed) {
        let pass: Vec<u16> = if forward || !vco { seq.clone() } else { seq.iter().rev().copied().collect() };
        for &step in &pass {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let centre = plan.centre(step as usize) as u64;
            set_freq(centre);
            {
                let mut st = host.state.lock();
                *st = HostState {
                    armed: true,
                    accept_from: host.transfers.load(Ordering::Acquire) + HOST_SETTLE_TRANSFERS,
                    step,
                    freq_hz: centre as u32,
                    sweep,
                    block: 0,
                    left: plan.dwell as u16,
                };
                while st.armed && !stop.load(Ordering::Relaxed) {
                    host.cv.wait_for(&mut st, std::time::Duration::from_millis(50));
                }
            }
        }
        if vco {
            forward = !forward;
        }
        sweep = sweep_no.fetch_add(1, Ordering::Relaxed) as u32 + 1;
    }
}

impl Drop for R82xDevice {
    fn drop(&mut self) {
        EMERGENCY.lock().take();
        self.pause();
        if let Io::Lib { api, dev, sink } = &self.io {
            unsafe {
                (api.close)(*dev);
                drop(Box::from_raw(*sink));
            }
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

    /// On real boards, through the built-in driver: sweep, change gain mid-sweep, pause and resume, change
    /// the plan (and the RFOne's rate), close and reopen, checking that tagged blocks keep coming.
    /// `CORVUS_TEST_SERIALS=board:hex,...` (e.g. rfone:<serial>,airspy:<serial>,hackrf:<serial>; a Pluto
    /// takes its address instead: pluto:<host>);
    /// run with `cargo test --release -- --ignored real_boards --nocapture`.
    #[test]
    #[ignore]
    fn real_boards_survive_the_ui_paths() {
        use crate::plan::Order;
        let list = std::env::var("CORVUS_TEST_SERIALS").expect("set CORVUS_TEST_SERIALS");
        for item in list.split(',') {
            let (b, sn) = item.split_once(':').expect("board:serial");
            let board = Board::parse(b).expect("board");
            let serial = if board.networked() {
                crate::pluto::set_addr(sn);
                0
            } else {
                u64::from_str_radix(sn.trim_start_matches("0x"), 16).expect("hex serial")
            };
            let rates = board.rates();
            let plan = |r: Rate, lo: f64, hi: f64| Plan::new(lo, hi, if r == Rate::H20 { 5.0 } else { r.max_window_hz() / 1e6 - 1.0 }, 1, 0, 2, Order::VcoContinuous, r);
            let blocks = |rx: &std::sync::mpsc::Receiver<Vec<u16>>, ms: u64| {
                std::thread::sleep(std::time::Duration::from_millis(ms));
                let mut n = 0;
                while let Ok(buf) = rx.try_recv() {
                    n += buf.chunks_exact(BLOCK_WORDS).filter(|b| matches!(parse_block(b), Block::Valid(_))).count();
                }
                n
            };
            for round in 0..2 {
                set_board(board);
                let (tx, rx) = std::sync::mpsc::sync_channel(4096);
                let sink = Sink { tx, host_drops: Arc::new(AtomicU64::new(0)), host: None };
                let rx_set = RxSettings { port: 0, gain: Gain::Linearity(10) };
                let mut d = Device::open(Path::new(NATIVE_LIB), serial, sink, rx_set, rates[0]).unwrap_or_else(|e| panic!("{b}: {e}"));
                let r = rates[0];
                d.sweep(&plan(r, 88.0, 470.0)).unwrap();
                let a = blocks(&rx, 700);
                d.set_rx(RxSettings { port: 0, gain: Gain::Sensitivity(14) }).unwrap();
                let g = blocks(&rx, 700);
                d.pause();
                let _ = blocks(&rx, 200);
                let paused = blocks(&rx, 300);
                d.resume().unwrap();
                let res = blocks(&rx, 700);
                let r2 = *rates.last().unwrap();
                d.sweep(&plan(r2, 400.0, 1000.0)).unwrap();
                let p2 = blocks(&rx, 900);
                eprintln!("{b} round {round} ({}): blocks {a} sweeping, {g} after a gain change, {paused} paused, {res} resumed, {p2} after a new plan at {}", d.version(), r2.label());
                assert!(a > 50 && g > 50 && res > 50 && p2 > 50, "{b}: blocks stopped coming");
                assert!(paused < 20, "{b}: still streaming while paused ({paused})");
                drop(d);
            }
        }
    }

    #[test]
    fn params_are_16_bytes() {
        assert_eq!(std::mem::size_of::<SweepParams>(), 16);
    }
}
