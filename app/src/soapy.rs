//! Any IQ radio SoapySDR can open, swept from the PC.
//!
//! libSoapySDR is loaded when first needed (never linked), so Corvus Sweep runs without it; with it, the radio
//! picker lists what its modules find (RTL-SDR, LimeSDR, SDRplay, USRP/B210 through SoapyUHD, and the boards
//! Corvus also drives natively). One thread owns the open device: for each step it retunes, reads and drops the
//! samples taken while the tuner settles, then reads the step's blocks, converts them to the Pluto's two-word
//! IQ layout (the radio's full scale reading 0 dBFS) and tags them as a sweep firmware would, visiting the steps
//! back and forth. The engine can't tell it from a Pluto, only that it's slower: each step costs a retune
//! through the radio's driver, and there is no lock check, just the settling time.

use std::ffi::{CStr, CString, c_char, c_int, c_long, c_longlong, c_void};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::Duration;

use libloading::Library;

use crate::hydra::{BLOCK_WORDS, Gain, RxSettings, Sink, TAG_WORDS, Tag, tag_block};
use crate::plan::{Order, Plan, Rate};

const RX: c_int = 1;
const SOAPY_SDR_TIMEOUT: c_int = -1;
const SOAPY_SDR_OVERFLOW: c_int = -4;
/// IQ samples in a block (the Pluto layout: two words each between the tags).
const BLOCK_IQ: usize = (BLOCK_WORDS - 2 * TAG_WORDS) / 2;
/// Full scale in the block's words, as the Pluto's 12-bit samples.
const FULL_SCALE: f32 = 2048.0;
/// Settling after a retune before samples count (ms; `CORVUS_SOAPY_SETTLE_MS` overrides).
const SETTLE_MS: f64 = 3.0;
/// Whole stream transfers (the radio's MTU) dropped after a retune, on top of the settling: what was in
/// flight when it retuned, which otherwise shows as copies of signals one or two steps away. Measured
/// (FM band, against each board's native sweep): SoapyAirspy needs one, SoapyHydraSDR two; two for the rest
/// until measured. SoapyPlutoSDR buffers deeper than six (use the Pluto's own sweep, corvus-pluto).
/// `CORVUS_SOAPY_SKIP_MTU` overrides.
fn skip_mtu(driver: &str) -> f64 {
    match driver {
        "airspy" => 1.0,
        _ => 2.0,
    }
}

#[repr(C)]
struct Kwargs {
    size: usize,
    keys: *mut *mut c_char,
    vals: *mut *mut c_char,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct Range {
    minimum: f64,
    maximum: f64,
    step: f64,
}

type Dev = c_void;
type Stream = c_void;

struct Api {
    _lib: Library,
    path: PathBuf,
    version: String,
    enumerate: unsafe extern "C" fn(*const c_char, *mut usize) -> *mut Kwargs,
    kwargs_list_clear: unsafe extern "C" fn(*mut Kwargs, usize),
    make: unsafe extern "C" fn(*const c_char) -> *mut Dev,
    unmake: unsafe extern "C" fn(*mut Dev) -> c_int,
    last_error: unsafe extern "C" fn() -> *const c_char,
    driver_key: unsafe extern "C" fn(*const Dev) -> *mut c_char,
    hardware_key: unsafe extern "C" fn(*const Dev) -> *mut c_char,
    free: unsafe extern "C" fn(*mut c_void),
    setup_stream: unsafe extern "C" fn(*mut Dev, c_int, *const c_char, *const usize, usize, *const Kwargs) -> *mut Stream,
    close_stream: unsafe extern "C" fn(*mut Dev, *mut Stream) -> c_int,
    activate: unsafe extern "C" fn(*mut Dev, *mut Stream, c_int, c_longlong, usize) -> c_int,
    deactivate: unsafe extern "C" fn(*mut Dev, *mut Stream, c_int, c_longlong) -> c_int,
    read: unsafe extern "C" fn(*mut Dev, *mut Stream, *const *mut c_void, usize, *mut c_int, *mut c_longlong, c_long) -> c_int,
    mtu: unsafe extern "C" fn(*const Dev, *mut Stream) -> usize,
    set_gain_mode: unsafe extern "C" fn(*mut Dev, c_int, usize, bool) -> c_int,
    set_gain: unsafe extern "C" fn(*mut Dev, c_int, usize, f64) -> c_int,
    gain_range: unsafe extern "C" fn(*const Dev, c_int, usize) -> Range,
    set_frequency: unsafe extern "C" fn(*mut Dev, c_int, usize, f64, *const Kwargs) -> c_int,
    frequency_range: unsafe extern "C" fn(*const Dev, c_int, usize, *mut usize) -> *mut Range,
    set_sample_rate: unsafe extern "C" fn(*mut Dev, c_int, usize, f64) -> c_int,
    list_sample_rates: unsafe extern "C" fn(*const Dev, c_int, usize, *mut usize) -> *mut f64,
    set_bandwidth: unsafe extern "C" fn(*mut Dev, c_int, usize, f64) -> c_int,
}

// SAFETY: the function pointers and the library handle may be used from any thread (SoapySDR's C API is).
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// Where libSoapySDR may be: next to this program, the system's, PothosSDR's on Windows.
fn candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(p) = std::env::var("CORVUS_SOAPY_LIB") {
        v.push(PathBuf::from(p));
    }
    let names: &[&str] = if cfg!(windows) {
        &["SoapySDR.dll"]
    } else if cfg!(target_os = "macos") {
        &["libSoapySDR.0.8.dylib", "libSoapySDR.dylib"]
    } else {
        &["libSoapySDR.so.0.8", "libSoapySDR.so"]
    };
    if let Some(dir) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        v.extend(names.iter().map(|n| dir.join(n)));
    }
    if cfg!(windows) {
        v.push(PathBuf::from(r"C:\Program Files\PothosSDR\bin\SoapySDR.dll"));
        // radioconda, installed for the user or for everyone
        if let Some(home) = std::env::var_os("USERPROFILE") {
            v.push(PathBuf::from(home).join(r"radioconda\Library\bin\SoapySDR.dll"));
        }
        v.push(PathBuf::from(r"C:\ProgramData\radioconda\Library\bin\SoapySDR.dll"));
    }
    v.extend(names.iter().map(PathBuf::from));
    v
}

static API: OnceLock<Result<Api, String>> = OnceLock::new();

fn api() -> Result<&'static Api, String> {
    API.get_or_init(load).as_ref().map_err(Clone::clone)
}

/// libSoapySDR is there (it's loaded on the first call).
pub fn available() -> bool {
    api().is_ok()
}

fn load() -> Result<Api, String> {
    let mut why = Vec::new();
    for path in candidates() {
        if path.is_absolute() && !path.exists() {
            continue;
        }
        #[cfg(windows)]
        if path.is_absolute() {
            dll_dir(&path);
        }
        match unsafe { Library::new(&path) } {
            Ok(lib) => return bind(lib, path),
            Err(e) => why.push(format!("{}: {e}", path.display())),
        }
    }
    Err(format!("SoapySDR isn't installed ({})", why.join("; ")))
}

/// Windows: let the DLLs beside SoapySDR.dll be found (its modules load their radios' libraries, rtlsdr.dll,
/// libusb and so on, by name), so it works without its folder on PATH.
#[cfg(windows)]
fn dll_dir(soapy: &Path) {
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "system" {
        fn SetDllDirectoryW(dir: *const u16) -> i32;
    }
    if let Some(dir) = soapy.parent() {
        let w: Vec<u16> = dir.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: a NUL-terminated wide string; the call copies it.
        unsafe { SetDllDirectoryW(w.as_ptr()) };
    }
}

fn bind(lib: Library, path: PathBuf) -> Result<Api, String> {
    macro_rules! sym {
        ($name:literal) => {
            // SAFETY: the symbol's type is the C API's (SoapySDR 0.8, Device.h / Types.h / Version.h).
            *unsafe { lib.get(concat!($name, "\0").as_bytes()) }.map_err(|e| format!("{}: no {} ({e}); SoapySDR 0.8 or later is needed", path.display(), $name))?
        };
    }
    let abi: unsafe extern "C" fn() -> *const c_char = sym!("SoapySDR_getABIVersion");
    let lib_ver: unsafe extern "C" fn() -> *const c_char = sym!("SoapySDR_getLibVersion");
    // SAFETY: both return static strings.
    let (abi, lib_ver) = unsafe { (CStr::from_ptr(abi()).to_string_lossy().into_owned(), CStr::from_ptr(lib_ver()).to_string_lossy().into_owned()) };
    if !abi.starts_with("0.8") {
        return Err(format!("{}: SoapySDR ABI {abi}; Corvus Sweep speaks 0.8", path.display()));
    }
    Ok(Api {
        enumerate: sym!("SoapySDRDevice_enumerateStrArgs"),
        kwargs_list_clear: sym!("SoapySDRKwargsList_clear"),
        make: sym!("SoapySDRDevice_makeStrArgs"),
        unmake: sym!("SoapySDRDevice_unmake"),
        last_error: sym!("SoapySDRDevice_lastError"),
        driver_key: sym!("SoapySDRDevice_getDriverKey"),
        hardware_key: sym!("SoapySDRDevice_getHardwareKey"),
        free: sym!("SoapySDR_free"),
        setup_stream: sym!("SoapySDRDevice_setupStream"),
        close_stream: sym!("SoapySDRDevice_closeStream"),
        activate: sym!("SoapySDRDevice_activateStream"),
        deactivate: sym!("SoapySDRDevice_deactivateStream"),
        read: sym!("SoapySDRDevice_readStream"),
        mtu: sym!("SoapySDRDevice_getStreamMTU"),
        set_gain_mode: sym!("SoapySDRDevice_setGainMode"),
        set_gain: sym!("SoapySDRDevice_setGain"),
        gain_range: sym!("SoapySDRDevice_getGainRange"),
        set_frequency: sym!("SoapySDRDevice_setFrequency"),
        frequency_range: sym!("SoapySDRDevice_getFrequencyRange"),
        set_sample_rate: sym!("SoapySDRDevice_setSampleRate"),
        list_sample_rates: sym!("SoapySDRDevice_listSampleRates"),
        set_bandwidth: sym!("SoapySDRDevice_setBandwidth"),
        version: format!("SoapySDR {lib_ver}"),
        path,
        _lib: lib,
    })
}

impl Api {
    fn last_error(&self) -> String {
        // SAFETY: a thread-local static string.
        unsafe { CStr::from_ptr((self.last_error)()).to_string_lossy().into_owned() }
    }

    /// A string the library allocated, copied and freed.
    fn take_str(&self, p: *mut c_char) -> String {
        if p.is_null() {
            return String::new();
        }
        // SAFETY: a NUL-terminated string from the library, freed with its allocator.
        unsafe {
            let s = CStr::from_ptr(p).to_string_lossy().into_owned();
            (self.free)(p as *mut c_void);
            s
        }
    }
}

// ---- finding radios ----

/// A radio a SoapySDR module found.
#[derive(Clone, Debug, PartialEq)]
pub struct Found {
    /// Stable id (a hash of `args`), standing in for a serial.
    pub id: u64,
    pub args: String,
    pub driver: String,
    pub label: String,
}

fn fnv(s: &str) -> u64 {
    s.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3)) | 1
}

/// Ask every module what it can open (can take seconds: some look on the network).
pub fn enumerate() -> Result<Vec<Found>, String> {
    let api = api()?;
    let mut n = 0usize;
    // SAFETY: an empty args string; the result is freed with SoapySDRKwargsList_clear.
    let list = unsafe { (api.enumerate)(c"".as_ptr(), &mut n) };
    let mut out = Vec::new();
    if list.is_null() {
        return Ok(out);
    }
    for i in 0..n {
        // SAFETY: n Kwargs, each with `size` key and value strings.
        let kw = unsafe { &*list.add(i) };
        let mut pairs = Vec::new();
        for j in 0..kw.size {
            let (k, v) = unsafe { (CStr::from_ptr(*kw.keys.add(j)).to_string_lossy().into_owned(), CStr::from_ptr(*kw.vals.add(j)).to_string_lossy().into_owned()) };
            pairs.push((k, v));
        }
        let get = |key: &str| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
        let driver = get("driver").unwrap_or_default();
        // what identifies it: everything but the free-text label (commas would break the args string)
        let args = pairs.iter().filter(|(k, v)| k != "label" && !v.contains(',')).map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join(",");
        let label = get("label").unwrap_or_else(|| format!("{driver} radio"));
        out.push(Found { id: fnv(&args), args, driver, label });
    }
    // SAFETY: the list from enumerate, cleared once.
    unsafe { (api.kwargs_list_clear)(list, n) };
    Ok(out)
}

/// `--soapy ARGS`: which radio "the first one" means (every key=value given must match).
static FILTER: parking_lot::Mutex<String> = parking_lot::const_mutex(String::new());

pub fn set_filter(args: &str) {
    *FILTER.lock() = args.to_string();
}

fn matches_filter(f: &Found) -> bool {
    let filter = FILTER.lock().clone();
    let have: Vec<&str> = f.args.split(',').collect();
    filter.split(',').map(str::trim).filter(|p| !p.is_empty()).all(|p| have.contains(&p))
}

/// The last list found, and whether a search is running.
static FOUND: parking_lot::Mutex<(Vec<Found>, bool)> = parking_lot::const_mutex((Vec::new(), false));

/// What the last search found; starts another in the background (the picker shows it when it lands).
pub fn cached() -> Vec<Found> {
    let mut f = FOUND.lock();
    if !f.1 && available() {
        f.1 = true;
        let _ = std::thread::Builder::new().name("soapy-find".into()).spawn(|| {
            let list = enumerate().unwrap_or_else(|e| {
                log::warn!("SoapySDR: {e}");
                Vec::new()
            });
            *FOUND.lock() = (list, false);
        });
    }
    f.0.clone()
}

// ---- the open radio, for the rest of the app ----

/// What the open radio is (the board's name, rates and passband file depend on it).
struct Current {
    name: &'static str,
    prefix: &'static str,
    rates: &'static [Rate],
    gain: (f64, f64),
}

static CURRENT: parking_lot::Mutex<Option<Current>> = parking_lot::const_mutex(None);

/// The open radio's name ("AirSpy One", "B210"…), or "SoapySDR" before one is open.
pub fn name() -> &'static str {
    CURRENT.lock().as_ref().map_or("SoapySDR radio", |c| c.name)
}

/// Prefix of its learned-passband files (one per driver: each radio's passband differs).
pub fn prefix() -> &'static str {
    CURRENT.lock().as_ref().map_or("soapy-", |c| c.prefix)
}

/// The rates it offers, fastest first; before one is open, a common one.
pub fn rates() -> &'static [Rate] {
    CURRENT.lock().as_ref().map_or(&[Rate::Iq(10_000_000)], |c| c.rates)
}

/// The gain a setting asks for (dB, in the open radio's range).
pub fn gain_db(g: Gain) -> f64 {
    let (lo, hi) = CURRENT.lock().as_ref().map_or((0.0, 50.0), |c| c.gain);
    let f = match g {
        Gain::Linearity(v) | Gain::Sensitivity(v) => v.min(21) as f64 / 21.0,
        Gain::Manual { lna, mixer, vga } => (lna.min(14) as f64 + mixer.min(15) as f64 + vga.min(15) as f64) / 44.0,
    };
    lo + f * (hi - lo)
}

/// Up to five of the radio's rates, fastest first, that make sensible steps (≥ 1 MHz usable, ≤ 61.44 MSPS).
fn pick_rates(listed: &[f64]) -> Vec<Rate> {
    let mut r: Vec<u32> = listed.iter().filter(|&&v| (1.3e6..=61.44e6).contains(&v)).map(|&v| v.round() as u32).collect();
    r.sort_unstable_by(|a, b| b.cmp(a));
    r.dedup();
    // RTL-SDRs list rates above 2.4 MSPS that drop samples: prefer 2.4 as their fastest
    if r.first().is_some_and(|&f| f > 2_400_000 && f <= 3_200_000) && r.contains(&2_400_000) {
        r.retain(|&v| v <= 2_400_000);
    }
    r.truncate(5);
    r.into_iter().map(Rate::Iq).collect()
}

// ---- the device thread ----

enum Cmd {
    Plan(Plan),
    Rx(RxSettings),
    Pause,
    Resume,
}

pub struct SoapyDevice {
    cmd: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
    pub version: String,
    pub lib_path: PathBuf,
    pub label: String,
}

/// A raw pointer that only the device thread uses.
struct Owned(*mut Dev);
// SAFETY: made on the opening thread, then used only by the device thread.
unsafe impl Send for Owned {}

impl SoapyDevice {
    /// Open the radio with this id (0: the first one found) and start its thread; it sweeps once given a plan.
    pub fn open(id: u64, sink: Sink, rx: RxSettings) -> Result<SoapyDevice, String> {
        let api = api()?;
        let mut list = FOUND.lock().0.clone();
        if id == 0 || !list.iter().any(|f| f.id == id) {
            list = enumerate()?;
            FOUND.lock().0 = list.clone();
        }
        let found = if id == 0 { list.iter().find(|f| matches_filter(f)) } else { list.iter().find(|f| f.id == id) }.cloned().ok_or_else(|| match id {
            0 if FILTER.lock().is_empty() => "SoapySDR found no radio".to_string(),
            0 => format!("SoapySDR found no radio matching {}", FILTER.lock()),
            _ => "that SoapySDR radio isn't there any more".to_string(),
        })?;
        let cargs = CString::new(found.args.clone()).map_err(|e| e.to_string())?;
        // SAFETY: a NUL-terminated args string.
        let dev = unsafe { (api.make)(cargs.as_ptr()) };
        if dev.is_null() {
            return Err(format!("SoapySDR couldn't open {}: {}", found.label, api.last_error()));
        }
        let (driver, hw) = (api.take_str(unsafe { (api.driver_key)(dev) }), api.take_str(unsafe { (api.hardware_key)(dev) }));
        let mut n = 0usize;
        // SAFETY: arrays the library allocated, read then freed.
        let rates: Vec<f64> = unsafe {
            let p = (api.list_sample_rates)(dev, RX, 0, &mut n);
            let v = if p.is_null() { Vec::new() } else { std::slice::from_raw_parts(p, n).to_vec() };
            (api.free)(p as *mut c_void);
            v
        };
        let ranges: Vec<Range> = unsafe {
            let p = (api.frequency_range)(dev, RX, 0, &mut n);
            let v = if p.is_null() { Vec::new() } else { std::slice::from_raw_parts(p, n).to_vec() };
            (api.free)(p as *mut c_void);
            v
        };
        let gain = unsafe { (api.gain_range)(dev, RX, 0) };
        let rates = pick_rates(&rates);
        if rates.is_empty() {
            unsafe { (api.unmake)(dev) };
            return Err(format!("{}: no sample rate between 1.3 and 61.44 MSPS", found.label));
        }
        let (flo, fhi) = (ranges.iter().map(|r| r.minimum).fold(f64::INFINITY, f64::min), ranges.iter().map(|r| r.maximum).fold(0.0, f64::max));
        if flo.is_finite() && fhi > flo {
            crate::plan::set_iq_range(flo.max(1e3), fhi);
        }
        let name = if hw.is_empty() { found.label.clone() } else { hw.clone() };
        let leak = |s: String| -> &'static str { Box::leak(s.into_boxed_str()) };
        *CURRENT.lock() = Some(Current {
            name: leak(name.clone()),
            prefix: leak(format!("soapy-{}-", driver.to_ascii_lowercase())),
            rates: Box::leak(rates.into_boxed_slice()),
            gain: (gain.minimum, gain.maximum.max(gain.minimum)),
        });
        let version = format!("{name} via SoapySDR ({driver}), {:.0}–{:.0} MHz, gain {:.0}–{:.0} dB", flo / 1e6, fhi / 1e6, gain.minimum, gain.maximum);
        log::info!("{version}; {}", api.version);
        let (tx, rx_cmd) = channel();
        let dev = Owned(dev);
        let drv = driver.to_ascii_lowercase();
        let thread = std::thread::Builder::new()
            .name("soapy-sweep".into())
            .spawn(move || {
                let dev = dev;
                Sweeper::new(api, dev.0, sink, rx, &drv).run(rx_cmd);
                // SAFETY: the stream is closed; the device is ours to close.
                unsafe { (api.unmake)(dev.0) };
            })
            .map_err(|e| e.to_string())?;
        Ok(SoapyDevice { cmd: tx, thread: Some(thread), version, lib_path: api.path.clone(), label: found.label })
    }

    pub fn sweep(&mut self, plan: &Plan) -> Result<(), String> {
        self.send(Cmd::Plan(plan.clone()))
    }

    pub fn set_rx(&mut self, rx: RxSettings) -> Result<(), String> {
        self.send(Cmd::Rx(rx))
    }

    pub fn pause(&mut self) {
        let _ = self.send(Cmd::Pause);
    }

    pub fn resume(&mut self) -> Result<(), String> {
        self.send(Cmd::Resume)
    }

    fn send(&self, c: Cmd) -> Result<(), String> {
        self.cmd.send(c).map_err(|_| "the SoapySDR radio stopped (see the log)".to_string())
    }
}

impl Drop for SoapyDevice {
    fn drop(&mut self) {
        // the thread ends when its command channel closes
        let (tx, _) = channel();
        drop(std::mem::replace(&mut self.cmd, tx));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

struct Sweeper {
    api: &'static Api,
    dev: *mut Dev,
    sink: Sink,
    rx: RxSettings,
    stream: *mut Stream,
    rate: Option<u32>,
    plan: Option<Plan>,
    paused: bool,
    /// Where in the pass: its index, and whether it runs backwards (every other pass, for back-and-forth order).
    pos: usize,
    back: bool,
    sweep: u32,
    tuned: Option<u64>,
    /// Samples read since opening (the block counter the tags carry).
    samples: u64,
    buf: Vec<f32>,
    settle_s: f64,
    skip_mtu: f64,
    /// The stream's transfer size (samples).
    mtu: usize,
}

impl Sweeper {
    fn new(api: &'static Api, dev: *mut Dev, sink: Sink, rx: RxSettings, driver: &str) -> Sweeper {
        let settle_ms = std::env::var("CORVUS_SOAPY_SETTLE_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(SETTLE_MS);
        Sweeper {
            api,
            dev,
            sink,
            rx,
            stream: std::ptr::null_mut(),
            rate: None,
            plan: None,
            paused: false,
            pos: 0,
            back: false,
            sweep: 0,
            tuned: None,
            samples: 0,
            buf: vec![0.0; 2 * BLOCK_IQ],
            settle_s: settle_ms / 1e3,
            skip_mtu: std::env::var("CORVUS_SOAPY_SKIP_MTU").ok().and_then(|v| v.parse().ok()).unwrap_or(skip_mtu(driver)),
            mtu: 0,
        }
    }

    fn run(&mut self, cmds: Receiver<Cmd>) {
        self.apply_gain();
        loop {
            let active = self.plan.is_some() && !self.paused;
            let c = if active {
                match cmds.try_recv() {
                    Ok(c) => Some(c),
                    Err(std::sync::mpsc::TryRecvError::Empty) => None,
                    Err(_) => break,
                }
            } else {
                match cmds.recv() {
                    Ok(c) => Some(c),
                    Err(_) => break,
                }
            };
            match c {
                Some(Cmd::Plan(p)) => {
                    if let Err(e) = self.ensure_rate(p.rate) {
                        log::warn!("SoapySDR: {e}");
                    }
                    self.plan = Some(p);
                    self.restart();
                }
                Some(Cmd::Rx(r)) => {
                    self.rx = r;
                    self.apply_gain();
                }
                Some(Cmd::Pause) => {
                    self.paused = true;
                    self.stop_stream();
                }
                Some(Cmd::Resume) => {
                    self.paused = false;
                    if let Some(r) = self.plan.as_ref().map(|p| p.rate) {
                        let _ = self.ensure_rate(r);
                    }
                    self.restart();
                }
                None => {
                    if let Err(e) = self.step() {
                        log::warn!("SoapySDR: {e}");
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
        self.stop_stream();
    }

    /// A new pass from the first step (what the engine expects after a change).
    fn restart(&mut self) {
        self.pos = 0;
        self.back = false;
        self.sweep = self.sweep.wrapping_add(1);
        self.tuned = None;
    }

    fn apply_gain(&mut self) {
        let db = gain_db(self.rx.gain);
        // SAFETY: the device is open.
        unsafe {
            (self.api.set_gain_mode)(self.dev, RX, 0, false);
            if (self.api.set_gain)(self.dev, RX, 0, db) != 0 {
                log::warn!("SoapySDR: gain {db:.1} dB: {}", self.api.last_error());
            }
        }
        self.tuned = None;
    }

    fn stop_stream(&mut self) {
        if !self.stream.is_null() {
            // SAFETY: our stream on our device.
            unsafe {
                (self.api.deactivate)(self.dev, self.stream, 0, 0);
                (self.api.close_stream)(self.dev, self.stream);
            }
            self.stream = std::ptr::null_mut();
        }
    }

    /// The radio at this rate, streaming.
    fn ensure_rate(&mut self, rate: Rate) -> Result<(), String> {
        let Rate::Iq(sr) = rate else { return Err(format!("{} isn't a SoapySDR rate", rate.label())) };
        if self.rate == Some(sr) && !self.stream.is_null() {
            return Ok(());
        }
        self.stop_stream();
        // SAFETY: the device is open; the stream is set up on channel 0 and closed in stop_stream.
        unsafe {
            if (self.api.set_sample_rate)(self.dev, RX, 0, sr as f64) != 0 {
                return Err(format!("sample rate {sr}: {}", self.api.last_error()));
            }
            // not every radio has a settable filter; it's fine if this fails
            (self.api.set_bandwidth)(self.dev, RX, 0, sr as f64);
            let ch = [0usize];
            let s = (self.api.setup_stream)(self.dev, RX, c"CF32".as_ptr(), ch.as_ptr(), 1, std::ptr::null());
            if s.is_null() {
                return Err(format!("stream: {}", self.api.last_error()));
            }
            if (self.api.activate)(self.dev, s, 0, 0, 0) != 0 {
                (self.api.close_stream)(self.dev, s);
                return Err(format!("start streaming: {}", self.api.last_error()));
            }
            self.stream = s;
            self.mtu = (self.api.mtu)(self.dev, s);
        }
        log::info!("SoapySDR stream at {sr} S/s, MTU {} samples ({:.1} ms)", self.mtu, self.mtu as f64 / sr as f64 * 1e3);
        self.rate = Some(sr);
        self.tuned = None;
        Ok(())
    }

    /// Read exactly `n` samples into the buffer's start. Err on a dropped buffer or a dead stream.
    fn read(&mut self, n: usize) -> Result<(), String> {
        let mut got = 0;
        let mut timeouts = 0;
        while got < n {
            let p = self.buf[2 * got..].as_mut_ptr() as *mut c_void;
            let (mut flags, mut t) = (0, 0);
            // SAFETY: room for n - got complex floats at p.
            let r = unsafe { (self.api.read)(self.dev, self.stream, &p, n - got, &mut flags, &mut t, 200_000) };
            match r {
                r if r > 0 => {
                    got += r as usize;
                    self.samples += r as u64;
                }
                SOAPY_SDR_OVERFLOW => return Err("overflow".into()),
                SOAPY_SDR_TIMEOUT if timeouts < 10 => timeouts += 1,
                0 => {}
                r => return Err(format!("read: error {r}")),
            }
        }
        Ok(())
    }

    /// Visit the next step: retune, settle, read its blocks.
    fn step(&mut self) -> Result<(), String> {
        let Some(plan) = self.plan.clone() else { return Ok(()) };
        if self.stream.is_null() {
            self.ensure_rate(plan.rate)?;
        }
        let seq = plan.sequence();
        let n = seq.len();
        let k = seq[if self.back { n - 1 - self.pos } else { self.pos }];
        let f = plan.centre(k as usize).round() as u64;
        let sr = self.rate.unwrap_or(1) as f64;
        if self.tuned != Some(f) {
            // SAFETY: the device is open.
            if unsafe { (self.api.set_frequency)(self.dev, RX, 0, f as f64, std::ptr::null()) } != 0 {
                return Err(format!("tune {:.3} MHz: {}", f as f64 / 1e6, self.api.last_error()));
            }
            self.tuned = Some(f);
            // what was in flight and the settling, dropped (in pieces of a block at most)
            let mut left = (sr * self.settle_s) as usize + (self.skip_mtu * self.mtu as f64) as usize + plan.skip as usize * BLOCK_IQ;
            while left > 0 {
                let m = left.min(BLOCK_IQ);
                let _ = self.read(m);
                left -= m;
            }
        }
        let mut b = 0u16;
        let mut tries = 0;
        while b < plan.dwell as u16 {
            if let Err(e) = self.read(BLOCK_IQ) {
                // a dropped buffer: that block is lost; read it again
                tries += 1;
                if tries > 4 {
                    return Err(e);
                }
                continue;
            }
            let mut w = vec![0u16; BLOCK_WORDS];
            for (o, v) in w[TAG_WORDS..BLOCK_WORDS - TAG_WORDS].iter_mut().zip(&self.buf) {
                *o = (v * FULL_SCALE).round().clamp(-32768.0, 32767.0) as i16 as u16;
            }
            let dma = (self.samples / BLOCK_IQ as u64) as u32;
            tag_block(&mut w, &Tag { step: k, block: b, sweep: self.sweep, freq_hz: f as u32, dma_count: dma, lock_polls: 0, lock_timed_out: false });
            self.sink.push(w);
            b += 1;
        }
        // the next step; at the end of a pass, the next one (back and forth: it starts where this one ended)
        self.pos += 1;
        if self.pos >= n {
            self.pos = 0;
            self.sweep = self.sweep.wrapping_add(1);
            if plan.order == Order::VcoContinuous && n > 1 {
                self.back = !self.back;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates_offered() {
        let r = pick_rates(&[2.5e6, 10e6]);
        assert_eq!(r, vec![Rate::Iq(10_000_000), Rate::Iq(2_500_000)]);
        // an RTL-SDR's list: 2.4 MSPS is its fastest dependable one
        let r = pick_rates(&[0.25e6, 1.024e6, 1.536e6, 1.792e6, 1.92e6, 2.048e6, 2.16e6, 2.56e6, 2.88e6, 3.2e6, 2.4e6]);
        assert_eq!(r[0], Rate::Iq(2_400_000));
        assert!(r.len() <= 5 && r.iter().all(|&x| matches!(x, Rate::Iq(v) if v <= 2_400_000)));
    }

    #[test]
    fn iq_rate_steps() {
        let r = Rate::Iq(10_000_000);
        assert_eq!(r.label(), "10 MSPS");
        assert!((r.max_window_hz() - 7.5e6).abs() < 1.0);
        assert_eq!(r.steps().last().unwrap().0, 7.5);
        assert_eq!(r.default_step_mhz(), 7.5);
        assert!(r.iq16() && r.complex() && !r.pluto());
    }
}
