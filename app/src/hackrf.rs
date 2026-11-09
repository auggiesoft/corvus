//! The HackRF One through the system libhackrf, in its stock firmware sweep (`hackrf_init_sweep`, the
//! same sweep `hackrf_sweep` uses): 20 MSPS complex, interleaved tunes 5 and 15 MHz past each 20 MHz base,
//! 7.5 MHz offset. Each 16 KB block (8192 IQ samples, header 0x7f7f + the tune's base frequency) holds the
//! slice 5 MHz below the LO and the slice 5 MHz above it. The RX callback re-tags each block twice, once
//! per slice, as two steps of the plan's 5 MHz grid, in the R82x tag format (the slice goes in the lock
//! field), so the engine handles it like any other board. The PortaPack UI is turned off while open (it
//! redraws the frequency on every retune), the RF amp and the antenna port power stay off.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use libloading::Library;

use crate::hydra::{BLOCK_WORDS, EMERGENCY, Emergency, Gain, RxSettings, Sink, Tag, tag_block};
use crate::plan::Plan;

type Dev = c_void;
const BLOCK_BYTES: usize = 16384;
const TUNE_STEP_HZ: u32 = 20_000_000;
const OFFSET_HZ: u32 = 7_500_000;
const INTERLEAVED: c_int = 1;

#[repr(C)]
struct Transfer {
    device: *mut Dev,
    buffer: *mut u8,
    buffer_length: c_int,
    valid_length: c_int,
    rx_ctx: *mut c_void,
    tx_ctx: *mut c_void,
}

type RxCb = unsafe extern "C" fn(*mut Transfer) -> c_int;

struct Api {
    _lib: Library,
    init: unsafe extern "C" fn() -> c_int,
    exit: unsafe extern "C" fn() -> c_int,
    open_by_serial: unsafe extern "C" fn(*const c_char, *mut *mut Dev) -> c_int,
    close: unsafe extern "C" fn(*mut Dev) -> c_int,
    version_string_read: unsafe extern "C" fn(*mut Dev, *mut c_char, u8) -> c_int,
    set_sample_rate: unsafe extern "C" fn(*mut Dev, f64) -> c_int,
    set_baseband_filter_bandwidth: unsafe extern "C" fn(*mut Dev, u32) -> c_int,
    set_lna_gain: unsafe extern "C" fn(*mut Dev, u32) -> c_int,
    set_vga_gain: unsafe extern "C" fn(*mut Dev, u32) -> c_int,
    set_amp_enable: unsafe extern "C" fn(*mut Dev, u8) -> c_int,
    set_antenna_enable: unsafe extern "C" fn(*mut Dev, u8) -> c_int,
    set_ui_enable: Option<unsafe extern "C" fn(*mut Dev, u8) -> c_int>,
    init_sweep: unsafe extern "C" fn(*mut Dev, *const u16, c_int, u32, u32, u32, c_int) -> c_int,
    start_rx_sweep: unsafe extern "C" fn(*mut Dev, RxCb, *mut c_void) -> c_int,
    stop_rx: unsafe extern "C" fn(*mut Dev) -> c_int,
    error_name: unsafe extern "C" fn(c_int) -> *const c_char,
}

impl Api {
    fn load(path: &Path) -> Result<Api, String> {
        let lib = unsafe { Library::new(path) }.map_err(|e| format!("cannot load {}: {e} (install libhackrf)", path.display()))?;
        macro_rules! sym {
            ($name:literal) => {
                *unsafe { lib.get(concat!("hackrf_", $name, "\0").as_bytes()) }
                    .map_err(|_| format!("{} has no hackrf_{} (libhackrf too old?)", path.display(), $name))?
            };
        }
        Ok(Api {
            init: sym!("init"),
            exit: sym!("exit"),
            open_by_serial: sym!("open_by_serial"),
            close: sym!("close"),
            version_string_read: sym!("version_string_read"),
            set_sample_rate: sym!("set_sample_rate"),
            set_baseband_filter_bandwidth: sym!("set_baseband_filter_bandwidth"),
            set_lna_gain: sym!("set_lna_gain"),
            set_vga_gain: sym!("set_vga_gain"),
            set_amp_enable: sym!("set_amp_enable"),
            set_antenna_enable: sym!("set_antenna_enable"),
            set_ui_enable: unsafe { lib.get(b"hackrf_set_ui_enable\0") }.ok().map(|f| *f),
            init_sweep: sym!("init_sweep"),
            start_rx_sweep: sym!("start_rx_sweep"),
            stop_rx: sym!("stop_rx"),
            error_name: sym!("error_name"),
            _lib: lib,
        })
    }

    fn err(&self, what: &str, rc: c_int) -> String {
        let name = unsafe { CStr::from_ptr((self.error_name)(rc)) }.to_string_lossy().into_owned();
        format!("{what}: {name} ({rc})")
    }
}

/// The plan geometry the callback needs, swapped in on every (re)start.
#[derive(Clone, Copy, Default)]
struct Geometry {
    start_hz: u64,
    f0: u64,
    n_steps: u16,
}

/// The callback's context: the sink, the geometry, and its own counters.
struct Ctx {
    sink: Sink,
    geo: parking_lot::Mutex<Geometry>,
    state: parking_lot::Mutex<CbState>,
}

#[derive(Default)]
struct CbState {
    last_freq: Option<u64>,
    block: u16,
    /// Pass counter: like the Corvus firmware's, bumped on every start and every pass, never reset.
    sweep: u32,
    /// When the stream started: the block clock is real time in 409.6 us blocks (the firmware's blocks carry
    /// no counter), so the HUD's rates are measured, not assumed.
    t0: Option<std::time::Instant>,
}

unsafe extern "C" fn rx_callback(t: *mut Transfer) -> c_int {
    // SAFETY: libhackrf passes a valid transfer whose rx_ctx is the Ctx given to start_rx_sweep, which
    // outlives streaming (stop_rx runs before it is freed).
    unsafe {
        let t = &*t;
        let ctx = &*(t.rx_ctx as *const Ctx);
        let bytes = std::slice::from_raw_parts(t.buffer, t.valid_length.max(0) as usize);
        let geo = *ctx.geo.lock();
        let mut st = ctx.state.lock();
        for blk in bytes.chunks_exact(BLOCK_BYTES) {
            if blk[0] != 0x7F || blk[1] != 0x7F {
                continue;
            }
            let f = u64::from_le_bytes(blk[2..10].try_into().unwrap());
            match st.last_freq {
                Some(prev) if prev == f => st.block += 1,
                prev => {
                    if prev.is_none_or(|p| f < p) {
                        st.sweep += 1;
                    }
                    st.block = 0;
                }
            }
            st.last_freq = Some(f);
            let t0 = *st.t0.get_or_insert_with(std::time::Instant::now);
            let dma = (t0.elapsed().as_secs_f64() / 409.6e-6) as u32;
            if f < geo.start_hz || !(f - geo.start_hz).is_multiple_of(5_000_000) {
                continue;
            }
            let low = ((f - geo.start_hz) / 5_000_000) as usize;
            let words: Vec<u16> = blk.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect();
            debug_assert_eq!(words.len(), BLOCK_WORDS);
            for (side, step) in [(0u16, low), (1u16, low + 2)] {
                if step >= geo.n_steps as usize {
                    continue;
                }
                let mut w = words.clone();
                let centre = geo.f0 + step as u64 * 5_000_000;
                tag_block(&mut w, &Tag { step: step as u16, block: st.block, sweep: st.sweep, freq_hz: centre as u32, dma_count: dma, lock_polls: side, lock_timed_out: false });
                ctx.sink.push(w);
            }
        }
    }
    0
}

pub struct HackrfDevice {
    api: Api,
    dev: *mut Dev,
    ctx: *mut Ctx,
    pub version: String,
    pub lib_path: PathBuf,
    plan: Option<Plan>,
    streaming: bool,
}

// SAFETY: the handle is only used from the thread that owns the Device (the UI thread).
unsafe impl Send for HackrfDevice {}

/// LNA (IF) gain 0-40 dB in 8 dB steps and VGA (baseband) 0-62 dB in 2 dB steps, from the app's gain
/// setting: the presets' 0-21 spread over both; manual LNA 0-14 and VGA 0-15 scaled onto them.
fn gains(g: Gain) -> (u32, u32) {
    match g {
        Gain::Linearity(v) | Gain::Sensitivity(v) => {
            let v = v.min(21) as u32;
            ((v * 40 / 21) / 8 * 8, (v * 62 / 21) & !1)
        }
        Gain::Manual { lna, vga, .. } => (((lna.min(14) as u32) * 40 / 14) / 8 * 8, ((vga.min(15) as u32) * 62 / 15) & !1),
    }
}

impl HackrfDevice {
    pub fn open(lib_path: &Path, serial: u64, sink: Sink, rx: RxSettings) -> Result<HackrfDevice, String> {
        let api = Api::load(lib_path)?;
        // hackrf_init is idempotent, and hackrf_exit only tears libusb down once no device is open.
        let rc = unsafe { (api.init)() };
        if rc != 0 {
            return Err(api.err("hackrf_init", rc));
        }
        // libhackrf matches a serial by suffix: the low 64 bits in lower-case hex are enough.
        let sn = (serial != 0).then(|| CString::new(format!("{serial:016x}")).unwrap());
        let mut dev = std::ptr::null_mut();
        let rc = unsafe { (api.open_by_serial)(sn.as_ref().map_or(std::ptr::null(), |s| s.as_ptr()), &mut dev) };
        if rc != 0 {
            unsafe { (api.exit)() };
            let which = if serial == 0 { "the HackRF".to_string() } else { format!("HackRF …{serial:016x}") };
            return Err(api.err(&format!("open {which} (unplugged, or busy in another program?)"), rc));
        }
        let mut buf = [0 as c_char; 255];
        unsafe { (api.version_string_read)(dev, buf.as_mut_ptr(), 255) };
        let version = format!("HackRF {}", unsafe { CStr::from_ptr(buf.as_ptr()) }.to_string_lossy());
        let ctx = Box::into_raw(Box::new(Ctx { sink, geo: parking_lot::Mutex::new(Geometry::default()), state: parking_lot::Mutex::new(CbState::default()) }));
        let d = HackrfDevice { api, dev, ctx, version, lib_path: lib_path.to_path_buf(), plan: None, streaming: false };
        d.configure(rx)?;
        *EMERGENCY.lock() = Some(Emergency { sweep_stop: d.api.stop_rx, stop_rx: d.api.stop_rx, dev: d.dev as usize });
        Ok(d)
    }

    fn check(&self, what: &str, rc: c_int) -> Result<(), String> {
        if rc == 0 { Ok(()) } else { Err(self.api.err(what, rc)) }
    }

    fn configure(&self, rx: RxSettings) -> Result<(), String> {
        unsafe {
            self.check("set_sample_rate 20 MSPS", (self.api.set_sample_rate)(self.dev, 20e6))?;
            self.check("set_baseband_filter_bandwidth 15 MHz", (self.api.set_baseband_filter_bandwidth)(self.dev, 15_000_000))?;
            self.check("RF amp off", (self.api.set_amp_enable)(self.dev, 0))?;
            self.check("antenna port power off", (self.api.set_antenna_enable)(self.dev, 0))?;
            if let Some(f) = self.api.set_ui_enable {
                f(self.dev, 0); // a HackRF without a PortaPack ignores it
            }
        }
        self.apply_rx(rx)
    }

    fn apply_rx(&self, rx: RxSettings) -> Result<(), String> {
        let (lna, vga) = gains(rx.gain);
        unsafe {
            self.check("set_lna_gain", (self.api.set_lna_gain)(self.dev, lna))?;
            self.check("set_vga_gain", (self.api.set_vga_gain)(self.dev, vga))
        }
    }

    fn stop_stream(&mut self) {
        if self.streaming {
            unsafe { (self.api.stop_rx)(self.dev) };
            self.streaming = false;
        }
    }

    /// (Re)start the firmware sweep with this plan: stop, program, start (the sweep can't be replaced
    /// while it runs).
    pub fn sweep(&mut self, plan: &Plan) -> Result<(), String> {
        self.stop_stream();
        let lo = (plan.start_hz / 1e6).round() as u16;
        let hi = (plan.stop_hz / 1e6).round() as u16;
        let freqs = [lo, hi];
        {
            // SAFETY: the stream is stopped, so the callback isn't running.
            let ctx = unsafe { &*self.ctx };
            *ctx.geo.lock() = Geometry { start_hz: lo as u64 * 1_000_000, f0: plan.f0, n_steps: plan.n_steps };
            let mut st = ctx.state.lock();
            st.last_freq = None;
            st.block = 0;
        }
        let num_bytes = plan.dwell.max(1) as u32 * BLOCK_BYTES as u32;
        unsafe {
            self.check(
                "init_sweep",
                (self.api.init_sweep)(self.dev, freqs.as_ptr(), 1, num_bytes, TUNE_STEP_HZ, OFFSET_HZ, INTERLEAVED),
            )?;
            self.check("start_rx_sweep", (self.api.start_rx_sweep)(self.dev, rx_callback, self.ctx as *mut c_void))?;
        }
        self.streaming = true;
        self.plan = Some(plan.clone());
        Ok(())
    }

    /// Gains can change while the firmware sweeps.
    pub fn set_rx(&mut self, rx: RxSettings) -> Result<(), String> {
        self.apply_rx(rx)
    }

    pub fn pause(&mut self) {
        self.stop_stream();
    }

    pub fn resume(&mut self) -> Result<(), String> {
        let plan = self.plan.clone().ok_or("no plan")?;
        self.sweep(&plan)
    }
}

impl Drop for HackrfDevice {
    fn drop(&mut self) {
        EMERGENCY.lock().take();
        self.stop_stream();
        unsafe {
            if let Some(f) = self.api.set_ui_enable {
                f(self.dev, 1);
            }
            (self.api.close)(self.dev);
            (self.api.exit)();
            drop(Box::from_raw(self.ctx));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{Order, Rate};

    #[test]
    fn hackrf_grid_matches_the_firmware_sweep() {
        let p = Plan::new(1.0, 6000.0, 5.0, 1, 0, 2, Order::VcoContinuous, Rate::H20);
        assert_eq!((p.start_hz, p.f0, p.step_hz, p.order), (1e6, 3_500_000, 5_000_000, Order::Interleaved));
        // bases 1, 21, ... while base + 25 < 6000: 300 of them, 4 slices each
        assert_eq!(p.n_steps, 1200);
        assert_eq!(&p.sequence()[..8], &[0, 2, 1, 3, 4, 6, 5, 7]);
        // a 25 MHz span is still one base (the firmware wraps after base + 5 + 20 >= stop)
        assert_eq!(Plan::new(100.0, 125.0, 5.0, 1, 0, 2, Order::Ascending, Rate::H20).n_steps, 4);
        assert_eq!(Plan::new(100.0, 126.0, 5.0, 1, 0, 2, Order::Ascending, Rate::H20).n_steps, 8);
    }

    #[test]
    fn gains_stay_in_range() {
        for v in 0..=21 {
            let (l, g) = gains(Gain::Linearity(v));
            assert!(l <= 40 && l % 8 == 0 && g <= 62 && g % 2 == 0);
        }
        assert_eq!(gains(Gain::Manual { lna: 14, mixer: 0, vga: 15 }), (40, 62));
    }
}
