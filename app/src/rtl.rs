//! RTL2832U dongles (RTL-SDR Blog V3/V4, NooElec, FlightAware…), through librtlsdr from the corvus-rtl
//! project, which Corvus Sweep downloads the first time one is used (never shipped with it, and pinned by
//! SHA-256 here, so a corvus-rtl release changes nothing until a Corvus Sweep is built for it). On Windows the
//! dongle also needs Windows' WinUSB driver: corvus-rtl's installer gives it that, after a Windows admin prompt.
//!
//! The sweep is the PC's: per step it retunes, empties the dongle's buffer (`rtlsdr_reset_buffer`), drops
//! what comes while the tuner settles, then reads the step's blocks synchronously, so nothing is in flight
//! across a retune; the blocks go to the engine in the Pluto's IQ layout, tagged, back and forth.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libloading::Library;
use sha2::{Digest, Sha256};

use crate::hydra::{Gain, RxSettings, Sink};
use crate::pcsweep::{BLOCK_IQ, Pass, push_block};
use crate::plan::{Plan, Rate};

/// The corvus-rtl release this Corvus Sweep uses, where its files are, and their SHA-256.
const VERSION: &str = "v0.2.0";
const BASE_URL: &str = "https://github.com/auggiesoft/corvus-rtl/releases/download";
#[cfg(windows)]
const LIB: (&str, &str) = ("corvus-rtl-v0.2.0-windows-x64.dll", "7bd84177abd1d998f81fa1d38a5d07d1f2b620374bb8cd36eb38a126ccf97e92");
#[cfg(not(windows))]
const LIB: (&str, &str) = ("libcorvus-rtl-v0.2.0-linux-x86_64.so", "92ed0ee758d9e9f492f038f8eb5db23d093c8621be8ac456ca57cd25f93d9030");
#[cfg(windows)]
const DRIVER: (&str, &str) = ("corvus-rtl-driver-v0.2.0-windows-x64.exe", "308a2e5166d4dbd2b782cad325e35c64ac67f8eecc39014f85d1b4ba5542601c");

/// The open error that means "download corvus-rtl first" / "install the WinUSB driver" (the window offers it).
pub const NEEDS_LIB: &str = "RTL-SDR support isn't downloaded yet";
pub const NEEDS_DRIVER: &str = "the dongle needs Windows' WinUSB driver";

/// RTL2832U dongles by USB ID: librtlsdr's known_devices (corvus-rtl's rtl-sdr/src/librtlsdr.c), as generated from it.
pub const IDS: &[(u16, u16, &str)] = &[
    (0x0bda, 0x2832, "Generic RTL2832U"),
    (0x0bda, 0x2838, "Generic RTL2832U OEM"),
    (0x0413, 0x6680, "DigitalNow Quad DVB-T PCI-E card"),
    (0x0413, 0x6f0f, "Leadtek WinFast DTV Dongle mini D"),
    (0x0458, 0x707f, "Genius TVGo DVB-T03 USB dongle (Ver. B)"),
    (0x0ccd, 0x00a9, "Terratec Cinergy T Stick Black (rev 1)"),
    (0x0ccd, 0x00b3, "Terratec NOXON DAB/DAB+ USB dongle (rev 1)"),
    (0x0ccd, 0x00b4, "Terratec Deutschlandradio DAB Stick"),
    (0x0ccd, 0x00b5, "Terratec NOXON DAB Stick - Radio Energy"),
    (0x0ccd, 0x00b7, "Terratec Media Broadcast DAB Stick"),
    (0x0ccd, 0x00b8, "Terratec BR DAB Stick"),
    (0x0ccd, 0x00b9, "Terratec WDR DAB Stick"),
    (0x0ccd, 0x00c0, "Terratec MuellerVerlag DAB Stick"),
    (0x0ccd, 0x00c6, "Terratec Fraunhofer DAB Stick"),
    (0x0ccd, 0x00d3, "Terratec Cinergy T Stick RC (Rev.3)"),
    (0x0ccd, 0x00d7, "Terratec T Stick PLUS"),
    (0x0ccd, 0x00e0, "Terratec NOXON DAB/DAB+ USB dongle (rev 2)"),
    (0x1554, 0x5020, "PixelView PV-DT235U(RN)"),
    (0x15f4, 0x0131, "Astrometa DVB-T/DVB-T2"),
    (0x15f4, 0x0133, "HanfTek DAB+FM+DVB-T"),
    (0x185b, 0x0620, "Compro Videomate U620F"),
    (0x185b, 0x0650, "Compro Videomate U650F"),
    (0x185b, 0x0680, "Compro Videomate U680F"),
    (0x1b80, 0xd393, "GIGABYTE GT-U7300"),
    (0x1b80, 0xd394, "DIKOM USB-DVBT HD"),
    (0x1b80, 0xd395, "Peak 102569AGPK"),
    (0x1b80, 0xd397, "KWorld KW-UB450-T USB DVB-T Pico TV"),
    (0x1b80, 0xd398, "Zaapa ZT-MINDVBZP"),
    (0x1b80, 0xd39d, "SVEON STV20 DVB-T USB & FM"),
    (0x1b80, 0xd3a4, "Twintech UT-40"),
    (0x1b80, 0xd3a8, "ASUS U3100MINI_PLUS_V2"),
    (0x1b80, 0xd3af, "SVEON STV27 DVB-T USB & FM"),
    (0x1b80, 0xd3b0, "SVEON STV21 DVB-T USB & FM"),
    (0x1d19, 0x1101, "Dexatek DK DVB-T Dongle (Logilink VG0002A)"),
    (0x1d19, 0x1102, "Dexatek DK DVB-T Dongle (MSI DigiVox mini II V3.0)"),
    (0x1d19, 0x1103, "Dexatek Technology Ltd. DK 5217 DVB-T Dongle"),
    (0x1d19, 0x1104, "MSI DigiVox Micro HD"),
    (0x1f4d, 0xa803, "Sweex DVB-T USB"),
    (0x1f4d, 0xb803, "GTek T803"),
    (0x1f4d, 0xc803, "Lifeview LV5TDeluxe"),
    (0x1f4d, 0xd286, "MyGica TD312"),
    (0x1f4d, 0xd803, "PROlectrix DV107669"),
];

/// The dongle's name for a USB ID, if it's one.
pub fn id_name(vid: u16, pid: u16) -> Option<&'static str> {
    IDS.iter().find(|i| i.0 == vid && i.1 == pid).map(|i| i.2)
}

// ---- getting corvus-rtl ----

fn data_dir() -> PathBuf {
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("corvus-sweep").join("rtl").join(VERSION)
}

fn sha256_hex(path: &Path) -> Option<String> {
    let b = std::fs::read(path).ok()?;
    Some(Sha256::digest(&b).iter().map(|x| format!("{x:02x}")).collect())
}

/// A pinned file, downloaded and checked: its path.
fn have(file: (&str, &str)) -> Option<PathBuf> {
    let p = data_dir().join(file.0);
    (sha256_hex(&p).as_deref() == Some(file.1)).then_some(p)
}

/// Download a pinned file (curl, which ships with Windows 10 and later and with every desktop Linux), check its
/// hash, and keep it. `CORVUS_RTL_URL` replaces the release URL (a local build: `file:///…/out`).
fn fetch(file: (&str, &str)) -> Result<PathBuf, String> {
    if let Some(p) = have(file) {
        return Ok(p);
    }
    let dir = data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let base = std::env::var("CORVUS_RTL_URL").unwrap_or_else(|_| format!("{BASE_URL}/{VERSION}"));
    let url = format!("{}/{}", base.trim_end_matches('/'), file.0);
    let part = dir.join(format!("{}.part", file.0));
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-fsSL", "--retry", "2", "-o"]).arg(&part).arg(&url);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let out = cmd.output().map_err(|e| format!("couldn't run curl to download {url}: {e}"))?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&part);
        return Err(format!("download of {url} failed: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    let got = sha256_hex(&part).unwrap_or_default();
    if got != file.1 {
        let _ = std::fs::remove_file(&part);
        return Err(format!("{} didn't match its checksum (got {got}); not used", file.0));
    }
    let dst = dir.join(file.0);
    std::fs::rename(&part, &dst).map_err(|e| format!("{}: {e}", dst.display()))?;
    Ok(dst)
}

/// What a background job (download, driver install) is doing, and how the last one ended.
#[derive(Clone, Debug, PartialEq)]
pub enum Job {
    Idle,
    Running(String),
    Done(String),
    Failed(String),
}

static JOB: parking_lot::Mutex<Job> = parking_lot::const_mutex(Job::Idle);

pub fn job() -> Job {
    JOB.lock().clone()
}

/// The finished job's outcome, once (the window then tries the dongle again, or shows why not).
pub fn take_finished() -> Option<Result<String, String>> {
    let mut j = JOB.lock();
    match std::mem::replace(&mut *j, Job::Idle) {
        Job::Done(m) => Some(Ok(m)),
        Job::Failed(e) => Some(Err(e)),
        other => {
            *j = other;
            None
        }
    }
}

fn run_job(what: &str, f: impl FnOnce() -> Result<String, String> + Send + 'static) {
    let mut j = JOB.lock();
    if matches!(*j, Job::Running(_)) {
        return;
    }
    *j = Job::Running(what.to_string());
    drop(j);
    std::thread::spawn(move || {
        let r = f();
        *JOB.lock() = match r {
            Ok(m) => Job::Done(m),
            Err(e) => Job::Failed(e),
        };
    });
}

/// `--get-rtl`: download the library (and on Windows install the driver) now, without a window.
pub fn get_now() -> Result<String, String> {
    let p = fetch(LIB)?;
    #[cfg_attr(not(windows), allow(unused_mut))]
    let mut msg = format!("RTL-SDR support: {}", p.display());
    #[cfg(windows)]
    {
        start_driver_install();
        loop {
            match take_finished() {
                Some(Ok(m)) => {
                    msg = format!("{msg}\n{m}");
                    break;
                }
                Some(Err(e)) => return Err(format!("{msg}\n{e}")),
                None => std::thread::sleep(Duration::from_millis(200)),
            }
        }
    }
    Ok(msg)
}

/// Download corvus-rtl's library in the background.
pub fn start_download() {
    run_job("Downloading RTL-SDR support", || fetch(LIB).map(|p| format!("RTL-SDR support downloaded ({})", p.display())));
}

/// Windows: download corvus-rtl's driver installer and run it (Windows asks for administrator rights).
#[cfg(windows)]
pub fn start_driver_install() {
    run_job("Installing the WinUSB driver (approve the Windows prompt)", || {
        let exe = fetch(DRIVER)?;
        let result = data_dir().join("driver-result.txt");
        let _ = std::fs::remove_file(&result);
        let code = run_elevated(&exe, &format!("--result \"{}\"", result.display()))?;
        let log = std::fs::read_to_string(&result).unwrap_or_default();
        let last = log.lines().last().unwrap_or("").to_string();
        match code {
            0 => Ok("WinUSB driver installed".into()),
            2 => Err("no RTL-SDR dongle was plugged in when the driver installed (it's ready for when one is)".into()),
            _ => Err(format!("the driver install failed: {}", last.strip_prefix("ERROR ").unwrap_or(&last))),
        }
    });
}

#[cfg(not(windows))]
pub fn start_driver_install() {}

/// Run a program with administrator rights (the Windows prompt), wait for it, its exit code.
#[cfg(windows)]
fn run_elevated(exe: &Path, args: &str) -> Result<u32, String> {
    use std::os::windows::ffi::OsStrExt;
    #[repr(C)]
    struct ShellExecuteInfoW {
        cb_size: u32,
        f_mask: u32,
        hwnd: *mut c_void,
        verb: *const u16,
        file: *const u16,
        parameters: *const u16,
        directory: *const u16,
        n_show: i32,
        h_inst_app: *mut c_void,
        id_list: *mut c_void,
        class: *const u16,
        hkey_class: *mut c_void,
        hot_key: u32,
        h_icon_or_monitor: *mut c_void,
        h_process: *mut c_void,
    }
    unsafe extern "system" {
        fn ShellExecuteExW(info: *mut ShellExecuteInfoW) -> i32;
        fn WaitForSingleObject(h: *mut c_void, ms: u32) -> u32;
        fn GetExitCodeProcess(h: *mut c_void, code: *mut u32) -> i32;
        fn CloseHandle(h: *mut c_void) -> i32;
        fn GetLastError() -> u32;
    }
    let wide = |s: &std::ffi::OsStr| s.encode_wide().chain([0]).collect::<Vec<u16>>();
    let (verb, file, params) = (wide("runas".as_ref()), wide(exe.as_os_str()), wide(args.as_ref()));
    let mut info = ShellExecuteInfoW {
        cb_size: std::mem::size_of::<ShellExecuteInfoW>() as u32,
        f_mask: 0x40, // SEE_MASK_NOCLOSEPROCESS
        hwnd: std::ptr::null_mut(),
        verb: verb.as_ptr(),
        file: file.as_ptr(),
        parameters: params.as_ptr(),
        directory: std::ptr::null(),
        n_show: 0, // SW_HIDE
        h_inst_app: std::ptr::null_mut(),
        id_list: std::ptr::null_mut(),
        class: std::ptr::null(),
        hkey_class: std::ptr::null_mut(),
        hot_key: 0,
        h_icon_or_monitor: std::ptr::null_mut(),
        h_process: std::ptr::null_mut(),
    };
    // SAFETY: a filled-in struct whose strings outlive the call; the process handle is closed below.
    unsafe {
        if ShellExecuteExW(&mut info) == 0 {
            let e = GetLastError();
            return Err(if e == 1223 { "the Windows prompt was declined".into() } else { format!("couldn't start the driver installer (error {e})") });
        }
        if info.h_process.is_null() {
            return Err("the driver installer didn't start".into());
        }
        WaitForSingleObject(info.h_process, 0xFFFF_FFFF);
        let mut code = 1u32;
        GetExitCodeProcess(info.h_process, &mut code);
        CloseHandle(info.h_process);
        Ok(code)
    }
}

// ---- librtlsdr ----

type Dev = c_void;

struct Api {
    _lib: Library,
    path: PathBuf,
    count: unsafe extern "C" fn() -> u32,
    usb_strings: unsafe extern "C" fn(u32, *mut c_char, *mut c_char, *mut c_char) -> c_int,
    open: unsafe extern "C" fn(*mut *mut Dev, u32) -> c_int,
    close: unsafe extern "C" fn(*mut Dev) -> c_int,
    set_sample_rate: unsafe extern "C" fn(*mut Dev, u32) -> c_int,
    set_center_freq: unsafe extern "C" fn(*mut Dev, u32) -> c_int,
    tuner_type: unsafe extern "C" fn(*mut Dev) -> c_int,
    tuner_gains: unsafe extern "C" fn(*mut Dev, *mut c_int) -> c_int,
    set_gain_mode: unsafe extern "C" fn(*mut Dev, c_int) -> c_int,
    set_gain: unsafe extern "C" fn(*mut Dev, c_int) -> c_int,
    set_agc_mode: unsafe extern "C" fn(*mut Dev, c_int) -> c_int,
    reset_buffer: unsafe extern "C" fn(*mut Dev) -> c_int,
    read_sync: unsafe extern "C" fn(*mut Dev, *mut c_void, c_int, *mut c_int) -> c_int,
    set_bias_tee: Option<unsafe extern "C" fn(*mut Dev, c_int) -> c_int>,
}

// SAFETY: plain C function pointers; each device handle is used by one thread at a time.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

static API: parking_lot::Mutex<Option<&'static Api>> = parking_lot::const_mutex(None);

/// librtlsdr: `CORVUS_RTL_LIB`, else the downloaded corvus-rtl, else (Linux) the system's.
fn api() -> Result<&'static Api, String> {
    let mut a = API.lock();
    if let Some(a) = *a {
        return Ok(a);
    }
    let mut paths: Vec<PathBuf> = Vec::new();
    if let Ok(p) = std::env::var("CORVUS_RTL_LIB") {
        paths.push(PathBuf::from(p));
    }
    paths.extend(have(LIB));
    if !cfg!(windows) {
        paths.push(PathBuf::from("librtlsdr.so.0"));
    }
    let mut why = Vec::new();
    for p in paths {
        // SAFETY: loading librtlsdr runs no initialisers that matter here.
        match unsafe { Library::new(&p) } {
            Ok(lib) => {
                let api: &'static Api = Box::leak(Box::new(bind(lib, p)?));
                *a = Some(api);
                return Ok(api);
            }
            Err(e) => why.push(format!("{}: {e}", p.display())),
        }
    }
    log::info!("librtlsdr: {}", why.join("; "));
    Err(NEEDS_LIB.into())
}

fn bind(lib: Library, path: PathBuf) -> Result<Api, String> {
    macro_rules! sym {
        ($name:literal) => {
            // SAFETY: librtlsdr's C API (rtl-sdr.h).
            *unsafe { lib.get(concat!($name, "\0").as_bytes()) }.map_err(|e| format!("{}: no {} ({e})", path.display(), $name))?
        };
    }
    Ok(Api {
        count: sym!("rtlsdr_get_device_count"),
        usb_strings: sym!("rtlsdr_get_device_usb_strings"),
        open: sym!("rtlsdr_open"),
        close: sym!("rtlsdr_close"),
        set_sample_rate: sym!("rtlsdr_set_sample_rate"),
        set_center_freq: sym!("rtlsdr_set_center_freq"),
        tuner_type: sym!("rtlsdr_get_tuner_type"),
        tuner_gains: sym!("rtlsdr_get_tuner_gains"),
        set_gain_mode: sym!("rtlsdr_set_tuner_gain_mode"),
        set_gain: sym!("rtlsdr_set_tuner_gain"),
        set_agc_mode: sym!("rtlsdr_set_agc_mode"),
        reset_buffer: sym!("rtlsdr_reset_buffer"),
        read_sync: sym!("rtlsdr_read_sync"),
        // SAFETY: as above; older librtlsdr has no bias tee call
        set_bias_tee: unsafe { lib.get(b"rtlsdr_set_bias_tee\0") }.ok().map(|s| *s),
        path,
        _lib: lib,
    })
}

/// What the open dongle is (its name, tuner, rates and gains decide the plan and the gain readout).
struct Current {
    name: &'static str,
    /// Tuner gains, tenths of a dB, ascending.
    gains: Vec<c_int>,
}

static CURRENT: parking_lot::Mutex<Option<Current>> = parking_lot::const_mutex(None);

/// The rates offered: 2.4 MSPS (the fastest that never drops samples), 2.048, 1.024.
pub const RATES: &[Rate] = &[Rate::Iq(2_400_000), Rate::Iq(2_048_000), Rate::Iq(1_024_000)];

pub fn name() -> &'static str {
    CURRENT.lock().as_ref().map_or("RTL-SDR", |c| c.name)
}

/// The tuner gain a setting picks (tenths of a dB), from the open dongle's table.
fn gain_tenths(g: Gain, gains: &[c_int]) -> c_int {
    if gains.is_empty() {
        return 0;
    }
    let f = match g {
        Gain::Linearity(v) | Gain::Sensitivity(v) => v.min(21) as f64 / 21.0,
        Gain::Manual { lna, mixer, vga } => (lna.min(14) as f64 + mixer.min(15) as f64 + vga.min(15) as f64) / 44.0,
    };
    gains[((gains.len() - 1) as f64 * f).round() as usize]
}

/// The gain a setting gives on the open dongle (dB), for the Receiver card.
pub fn gain_db(g: Gain) -> f64 {
    CURRENT.lock().as_ref().map_or(0.0, |c| gain_tenths(g, &c.gains) as f64 / 10.0)
}

/// The tuning range of a tuner type (rtl-sdr.h's enum), Hz.
fn tuner_range(t: c_int) -> (&'static str, f64, f64) {
    match t {
        1 => ("E4000", 52e6, 2200e6),
        2 => ("FC0012", 22e6, 948e6),
        3 => ("FC0013", 22e6, 1100e6),
        4 => ("FC2580", 146e6, 924e6),
        5 => ("R820T", 24e6, 1766e6),
        6 => ("R828D", 24e6, 1766e6),
        _ => ("unknown tuner", 24e6, 1766e6),
    }
}

fn serial_of(s: &str) -> u64 {
    crate::usbdev::serial_of(s)
}

/// Dongles often share a serial (many leave the factory as 00000001). The picker keys the k-th of several with
/// the same serial as `serial | k << 56`, and opening one takes the k-th dongle librtlsdr lists with that serial.
const DUP_SHIFT: u32 = 56;

pub fn dup_key(serial: u64, k: usize) -> u64 {
    (serial & ((1 << DUP_SHIFT) - 1)) | (k as u64) << DUP_SHIFT
}

fn split_key(key: u64) -> (u64, usize) {
    (key & ((1 << DUP_SHIFT) - 1), (key >> DUP_SHIFT) as usize)
}

/// Why librtlsdr couldn't open a dongle. libusb's NOT_SUPPORTED (-12) or ACCESS (-3): on Windows, no WinUSB
/// driver yet; on Linux, permissions.
fn open_error(r: c_int) -> String {
    match r {
        -12 | -3 if cfg!(windows) => NEEDS_DRIVER.into(),
        -3 => "no permission to open the dongle: install a udev rule for it (see corvus-rtl's README), then replug".into(),
        -6 => "the dongle is in use by another program".into(),
        _ => format!("couldn't open the dongle (librtlsdr error {r})"),
    }
}

// ---- the device thread ----

enum Cmd {
    Plan(Plan),
    Rx(RxSettings),
    Pause,
    Resume,
}

pub struct RtlDevice {
    cmd: Sender<Cmd>,
    thread: Option<JoinHandle<()>>,
    pub version: String,
    pub lib_path: PathBuf,
    pub label: String,
    /// Its serial string.
    pub sn: String,
    /// The picker's key for the dongle opened (when asked for "the first one", its own).
    pub key: u64,
}

struct Owned(*mut Dev);
// SAFETY: opened here, then used only by the device thread.
unsafe impl Send for Owned {}

impl RtlDevice {
    /// Open the dongle with this key (0: the first one; see `dup_key`) and start its thread; it sweeps once
    /// given a plan.
    pub fn open(serial: u64, sink: Sink, rx: RxSettings) -> Result<RtlDevice, String> {
        let api = api()?;
        let (want, nth) = split_key(serial);
        // SAFETY: plain calls into librtlsdr; buffers of 256 bytes as rtl-sdr.h asks.
        let n = unsafe { (api.count)() };
        let mut pick = None;
        let mut seen = 0;
        // a dongle librtlsdr can't open can't give its serial either: no driver (Windows) or no permission
        let mut unreadable = None;
        for i in 0..n {
            let (mut m, mut p, mut s) = ([0 as c_char; 256], [0 as c_char; 256], [0 as c_char; 256]);
            let r = unsafe { (api.usb_strings)(i, m.as_mut_ptr(), p.as_mut_ptr(), s.as_mut_ptr()) };
            if r < 0 {
                unreadable.get_or_insert(r);
                continue;
            }
            let sn = unsafe { CStr::from_ptr(s.as_ptr()) }.to_string_lossy().into_owned();
            let label = unsafe { format!("{} {}", CStr::from_ptr(m.as_ptr()).to_string_lossy(), CStr::from_ptr(p.as_ptr()).to_string_lossy()) };
            if serial == 0 || serial_of(&sn) == want {
                if seen == nth {
                    let key = if serial == 0 { serial_of(&sn) } else { serial };
                    pick = Some((i, sn, label.trim().to_string(), key));
                    break;
                }
                seen += 1;
            }
        }
        let Some((index, sn, label, key)) = pick else {
            return Err(match unreadable {
                Some(r) => open_error(r),
                None if n == 0 => "no RTL-SDR dongle found".into(),
                None => "that RTL-SDR dongle isn't plugged in".into(),
            });
        };
        let mut dev: *mut Dev = std::ptr::null_mut();
        // SAFETY: as above.
        let r = unsafe { (api.open)(&mut dev, index) };
        if r < 0 || dev.is_null() {
            return Err(open_error(r));
        }
        let (tuner, lo, hi) = tuner_range(unsafe { (api.tuner_type)(dev) });
        let ng = unsafe { (api.tuner_gains)(dev, std::ptr::null_mut()) }.max(0) as usize;
        let mut gains = vec![0 as c_int; ng];
        if ng > 0 {
            unsafe { (api.tuner_gains)(dev, gains.as_mut_ptr()) };
        }
        if let Some(bias) = api.set_bias_tee {
            unsafe { bias(dev, 0) };
        }
        // Somewhere it tunes before anything else: setting the sample rate retunes to the current frequency,
        // 0 after opening, which this librtlsdr takes as a request for direct sampling (below 24 MHz).
        unsafe { (api.set_center_freq)(dev, 100e6f64.clamp(lo, hi) as u32) };
        crate::plan::set_iq_range(lo, hi);
        let name = if label.is_empty() { format!("RTL2832U ({tuner})") } else { format!("{label} ({tuner})") };
        let version = format!("{name}, serial {sn}, {:.0}–{:.0} MHz, {} gains to {:.1} dB, corvus-rtl {VERSION}", lo / 1e6, hi / 1e6, gains.len(), gains.last().copied().unwrap_or(0) as f64 / 10.0);
        *CURRENT.lock() = Some(Current { name: Box::leak(name.clone().into_boxed_str()), gains: gains.clone() });
        log::info!("{version} via {}", api.path.display());
        let (tx, rx_cmd) = channel();
        let dev = Owned(dev);
        let thread = std::thread::Builder::new()
            .name("rtl-sweep".into())
            .spawn(move || {
                let dev = dev;
                Sweeper { api, dev: dev.0, sink, rx, gains, range: (lo, hi), rate: None, plan: None, paused: false, pass: Pass::default(), skipped: 0, tuned: None, samples: 0, read_end: None, fifo: Vec::new(), chunk: Vec::new(), iq: vec![0.0; 2 * BLOCK_IQ] }.run(rx_cmd);
                // SAFETY: our handle, closed once.
                unsafe { (api.close)(dev.0) };
            })
            .map_err(|e| e.to_string())?;
        Ok(RtlDevice { cmd: tx, thread: Some(thread), version, lib_path: api.path.clone(), label: name, sn, key })
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
        self.cmd.send(c).map_err(|_| "the RTL-SDR dongle stopped (see the log)".to_string())
    }
}

impl Drop for RtlDevice {
    fn drop(&mut self) {
        let (tx, _) = channel();
        drop(std::mem::replace(&mut self.cmd, tx));
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Settling after a retune (after the buffer is emptied): R820T PLLs lock in about a millisecond.
const SETTLE_S: f64 = 2e-3;
/// librtlsdr reads in multiples of 512 bytes (256 samples). Each read asks for what the step still needs, so a
/// step costs its settling and its blocks and little more: a fixed 16 KiB read wasted up to 8192 samples a step.
const READ_UNIT: usize = 512;

struct Sweeper {
    api: &'static Api,
    dev: *mut Dev,
    sink: Sink,
    rx: RxSettings,
    gains: Vec<c_int>,
    /// What the tuner reaches (Hz). A plan made before the dongle said so can hold steps outside it: they're
    /// skipped (librtlsdr would switch to direct sampling below 24 MHz, and the PLL can't lock past the top).
    range: (f64, f64),
    rate: Option<u32>,
    plan: Option<Plan>,
    paused: bool,
    pass: Pass,
    /// Steps skipped in a row (all of them: nothing to sweep until the next plan).
    skipped: usize,
    tuned: Option<u32>,
    /// The block counter's clock: samples read, plus the samples the dongle made while it was retuned
    /// (thrown away with its buffer), so it keeps real time. A retune costs ~11 ms in corvus-rtl's librtlsdr
    /// (~50 in stock librtlsdr), longer than the step's reads: counting only what was read ran several times
    /// fast, and so did the sweep rate shown and the averaging and smoothing times.
    samples: u64,
    /// When the last read ended (the retune's lost time runs from here).
    read_end: Option<Instant>,
    /// Bytes read but not yet used (u8 I/Q pairs).
    fifo: Vec<u8>,
    chunk: Vec<u8>,
    iq: Vec<f32>,
}

impl Sweeper {
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
                    self.plan = Some(p);
                    self.restart();
                }
                Some(Cmd::Rx(r)) => {
                    self.rx = r;
                    self.apply_gain();
                }
                Some(Cmd::Pause) => self.paused = true,
                Some(Cmd::Resume) => {
                    self.paused = false;
                    self.restart();
                }
                None => {
                    if let Err(e) = self.step() {
                        log::warn!("RTL-SDR: {e}");
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
            }
        }
    }

    fn restart(&mut self) {
        self.pass.restart();
        self.tuned = None;
    }

    fn apply_gain(&mut self) {
        let g = gain_tenths(self.rx.gain, &self.gains);
        // SAFETY: the device is open.
        unsafe {
            (self.api.set_agc_mode)(self.dev, 0);
            (self.api.set_gain_mode)(self.dev, 1);
            (self.api.set_gain)(self.dev, g);
        }
        self.tuned = None;
    }

    /// Read until `n` samples are buffered.
    fn fill(&mut self, n: usize) -> Result<(), String> {
        while self.fifo.len() < 2 * n {
            let want = (2 * n - self.fifo.len()).div_ceil(READ_UNIT) * READ_UNIT;
            self.chunk.resize(want, 0);
            let mut got: c_int = 0;
            // SAFETY: room for `want` bytes.
            let r = unsafe { (self.api.read_sync)(self.dev, self.chunk.as_mut_ptr() as *mut c_void, want as c_int, &mut got) };
            if r < 0 {
                return Err(format!("read failed (librtlsdr error {r}); the dongle may have been unplugged"));
            }
            let got = (got.max(0) as usize).min(want);
            self.fifo.extend_from_slice(&self.chunk[..got]);
            self.samples += got as u64 / 2;
            self.read_end = Some(Instant::now());
        }
        Ok(())
    }

    fn step(&mut self) -> Result<(), String> {
        let Some(plan) = self.plan.clone() else { return Ok(()) };
        let Rate::Iq(sr) = plan.rate else { return Err(format!("{} isn't an RTL-SDR rate", plan.rate.label())) };
        if self.rate != Some(sr) {
            // SAFETY: the device is open.
            if unsafe { (self.api.set_sample_rate)(self.dev, sr) } != 0 {
                return Err(format!("sample rate {sr} refused"));
            }
            self.rate = Some(sr);
            self.tuned = None;
        }
        let seq = plan.sequence();
        let n = seq.len();
        let k = self.pass.step(&seq);
        let fc = plan.centre(k as usize);
        if fc < self.range.0 || fc > self.range.1 {
            self.pass.advance(&plan, n);
            self.skipped += 1;
            if self.skipped >= n {
                self.skipped = 0;
                std::thread::sleep(Duration::from_millis(20));
            }
            return Ok(());
        }
        self.skipped = 0;
        let f = fc.round() as u32;
        if self.tuned != Some(f) {
            // SAFETY: the device is open.
            if unsafe { (self.api.set_center_freq)(self.dev, f) } != 0 {
                return Err(format!("tune {:.3} MHz refused", f as f64 / 1e6));
            }
            self.tuned = Some(f);
            // what the dongle buffered before the retune, then what comes while the tuner settles
            unsafe { (self.api.reset_buffer)(self.dev) };
            if let Some(t) = self.read_end {
                self.samples += (t.elapsed().as_secs_f64() * sr as f64) as u64;
            }
            self.fifo.clear();
            let settle = ((sr as f64 * SETTLE_S) as usize).max(4096) + plan.skip as usize * BLOCK_IQ;
            self.fill(settle)?;
            self.fifo.drain(..2 * settle);
        }
        for b in 0..plan.dwell as u16 {
            self.fill(BLOCK_IQ)?;
            // 8-bit offset binary around 127.4; scaled so both rails (0, 255) reach full scale, where the
            // health check counts clipping
            for (o, &x) in self.iq.iter_mut().zip(&self.fifo[..2 * BLOCK_IQ]) {
                *o = (x as f32 - 127.4) / 127.4;
            }
            self.fifo.drain(..2 * BLOCK_IQ);
            push_block(&self.sink, &self.iq, k, b, self.pass.sweep, f as u64, (self.samples / BLOCK_IQ as u64) as u32);
        }
        self.pass.advance(&plan, n);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gains_spread_over_the_table() {
        let g = [0, 9, 14, 27, 37, 77, 87, 125, 144, 157, 166, 197, 207, 229, 254, 280, 297, 328, 338, 364, 372, 386, 402, 421, 434, 439, 445, 480, 496];
        assert_eq!(gain_tenths(Gain::Linearity(0), &g), 0);
        assert_eq!(gain_tenths(Gain::Linearity(21), &g), 496);
        assert_eq!(gain_tenths(Gain::Linearity(10), &g), g[13]);
    }

    #[test]
    fn duplicate_serials_get_distinct_keys() {
        assert_eq!(split_key(dup_key(1, 0)), (1, 0));
        assert_eq!(split_key(dup_key(1, 1)), (1, 1));
        assert_ne!(dup_key(1, 0), dup_key(1, 1));
    }

    #[test]
    fn known_ids() {
        assert_eq!(IDS.len(), 42);
        assert_eq!(id_name(0x0bda, 0x2838), Some("Generic RTL2832U OEM"));
        assert_eq!(id_name(0x1d50, 0x60a1), None);
    }
}
