//! Native USB for the R82x boards (HydraSDR RFOne, Airspy R2 and Mini): the vendor requests the app uses
//! from libhydrasdr and libairspy, and the bulk stream, in Rust (nusb). No libusb and no library to ship,
//! so one binary works on Linux and Windows (WinUSB, which both firmwares' Microsoft OS descriptors bind
//! without a driver install).
//!
//! It mirrors `host/airspyone-host/libairspy/src/airspy.c` and `host/hydrasdr-host/libhydrasdr/src/`
//! (`hydrasdr_shared.c`, `hydrasdr_rfone.c`) request for request: open by serial, read the version and the
//! firmware's sample-rate (and on the RFOne bandwidth) tables, packing off, receiver mode, frequency, RF
//! port, gains (the same linearity/sensitivity tables on both), and the Corvus sweep start/stop. The two
//! boards differ in the frequency's width (u32 vs u64), the RFOne's port and bandwidth requests, and the
//! order of the receiver-mode and halt-clearing steps around streaming. Samples are passed through
//! untouched (RAW): 16-bit words, which is all the sweep path needs.
//!
//! The HackRF part (`Hackrf`) mirrors libhackrf (`host/libhackrf/src/hackrf.c`, 2026.01.3) the same way.
//!
//! Ported from, with their notices kept (the release archives carry the full texts):
//! - libairspy: Copyright (c) 2013, Michael Ossmann; 2012, Jared Boone; 2014, Youssef Touil; 2014,
//!   Benjamin Vernoux; 2015, Ian Gilmour. BSD-3-Clause.
//! - libhydrasdr: Copyright (C) 2013-2026, Benjamin Vernoux. MIT.
//! - libhackrf: Copyright (c) 2012-2022 Great Scott Gadgets; 2012, Jared Boone; 2013, Benjamin Vernoux.
//!   BSD-3-Clause.

use std::cell::Cell;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use nusb::MaybeFuture;
use nusb::transfer::{Bulk, ControlIn, ControlOut, ControlType, In, Recipient, TransferError};

use crate::hydra::SweepParams;

pub const AIRSPY_VID: u16 = 0x1d50;
pub const AIRSPY_PID: u16 = 0x60a1;
pub const RFONE_VID: u16 = 0x38af;
pub const RFONE_PID: u16 = 0x0001;

/// Which protocol dialect: the two share nearly every request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    RFOne,
    /// Airspy R2 or Mini (one firmware image, which knows which board it's on).
    Airspy,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::RFOne => "RFOne",
            Kind::Airspy => "Airspy",
        }
    }

    /// Whether a USB device is this kind: the RFOne on its own ID, or on the Airspy ID it had before
    /// v1.0.2 (its serial string then says HYDRASDR).
    fn matches(self, d: &nusb::DeviceInfo) -> bool {
        let hydra_sn = d.serial_number().is_some_and(|s| s.starts_with("HYDRASDR"));
        match self {
            Kind::RFOne => (d.vendor_id(), d.product_id()) == (RFONE_VID, RFONE_PID) || ((d.vendor_id(), d.product_id()) == (AIRSPY_VID, AIRSPY_PID) && hydra_sn),
            Kind::Airspy => (d.vendor_id(), d.product_id()) == (AIRSPY_VID, AIRSPY_PID) && !hydra_sn,
        }
    }
}

const RECEIVER_MODE: u8 = 1;
const VERSION_STRING_READ: u8 = 10;
const SET_SAMPLERATE: u8 = 12;
const SET_FREQ: u8 = 13;
const SET_LNA_GAIN: u8 = 14;
const SET_MIXER_GAIN: u8 = 15;
const SET_VGA_GAIN: u8 = 16;
const SET_LNA_AGC: u8 = 17;
const SET_MIXER_AGC: u8 = 18;
const GET_SAMPLERATES: u8 = 25;
const SET_PACKING: u8 = 26;
/// RFOne only.
const SET_RF_BIAS: u8 = 20;
const SET_RF_PORT: u8 = 28;
const SET_BANDWIDTH: u8 = 30;
const GET_BANDWIDTHS: u8 = 31;
const SWEEP_START: u8 = 0x70;
const SWEEP_STOP: u8 = 0x71;

/// Both libraries' streaming setup: 16 transfers of 256 KB in flight (unpacked).
const TRANSFERS: usize = 16;
const TRANSFER_BYTES: usize = 262_144;
const TIMEOUT: Duration = Duration::from_millis(1000);

/// The gain presets, indexed by 21 - gain: identical in libairspy (airspy.c) and libhydrasdr
/// (hydrasdr_rfone.c), both GAIN_COUNT 22.
const LIN_VGA: [u8; 22] = [13, 12, 11, 11, 11, 11, 11, 10, 10, 10, 10, 10, 10, 10, 10, 10, 9, 8, 7, 6, 5, 4];
const LIN_MIXER: [u8; 22] = [12, 12, 11, 9, 8, 7, 6, 6, 5, 0, 0, 1, 0, 0, 2, 2, 1, 1, 1, 1, 0, 0];
const LIN_LNA: [u8; 22] = [14, 14, 14, 13, 12, 10, 9, 9, 8, 9, 8, 6, 5, 3, 1, 0, 0, 0, 0, 0, 0, 0];
const SENS_VGA: [u8; 22] = [13, 12, 11, 10, 9, 8, 7, 6, 5, 5, 5, 5, 5, 4, 4, 4, 4, 4, 4, 4, 4, 4];
const SENS_MIXER: [u8; 22] = [12, 12, 12, 12, 11, 10, 10, 9, 9, 8, 7, 4, 4, 4, 3, 2, 2, 1, 0, 0, 0, 0];
const SENS_LNA: [u8; 22] = [14, 14, 14, 14, 14, 14, 14, 14, 14, 13, 12, 12, 9, 9, 8, 7, 6, 5, 3, 2, 1, 0];

/// The serial in a USB serial string: the last 16 hex digits ("AIRSPY SN:0123456789ABCDEF"; Windows may
/// hand back a mangled instance-ID form, but the digits survive).
pub fn serial_of(s: &str) -> u64 {
    let h: String = s.rsplit(':').next().unwrap_or("").chars().filter(|c| c.is_ascii_hexdigit()).collect();
    u64::from_str_radix(&h[h.len().saturating_sub(16)..], 16).unwrap_or(0)
}

/// The sweep revision in a version string ("...-sweep5" -> 5), 0 for firmware without sweep mode.
pub fn sweep_revision(version: &str) -> i32 {
    let Some(i) = version.find("-sweep") else { return 0 };
    let n: String = version[i + 6..].chars().take_while(|c| c.is_ascii_digit()).collect();
    n.parse::<i32>().ok().filter(|&n| n > 0).unwrap_or(1)
}

fn usb_err(what: &str, e: TransferError) -> String {
    match e {
        TransferError::Stall => format!("{what}: refused by the board (STALL)"),
        TransferError::Disconnected => format!("{what}: the board was unplugged"),
        e => format!("{what}: {e}"),
    }
}

struct Stream {
    stop: Arc<AtomicBool>,
    thread: std::thread::JoinHandle<()>,
}

impl Stream {
    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

/// Keep `TRANSFERS` bulk IN transfers in flight on endpoint 0x81 and hand each completed one to `on_bytes`
/// on a thread of its own, until stopped.
fn start_bulk(iface: &nusb::Interface, clear_halt: bool, mut on_bytes: impl FnMut(&[u8]) + Send + 'static) -> Result<Stream, String> {
    let mut ep = iface.endpoint::<Bulk, In>(0x81).map_err(|e| format!("open the data endpoint: {e}"))?;
    if clear_halt {
        let _ = ep.clear_halt().wait();
    }
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    let thread = std::thread::Builder::new()
        .name("corvus-usb-rx".into())
        .spawn(move || {
            while ep.pending() < TRANSFERS {
                let b = ep.allocate(TRANSFER_BYTES);
                ep.submit(b);
            }
            while !stop2.load(Ordering::Relaxed) {
                let Some(c) = ep.wait_next_complete(Duration::from_millis(100)) else { continue };
                match c.status {
                    Ok(()) => on_bytes(&c.buffer),
                    Err(TransferError::Disconnected) => break,
                    Err(_) => {}
                }
                ep.submit(c.buffer);
            }
            ep.cancel_all();
            while ep.pending() > 0 && ep.wait_next_complete(Duration::from_millis(500)).is_some() {}
        })
        .map_err(|e| format!("start the USB thread: {e}"))?;
    Ok(Stream { stop, thread })
}

fn vendor_out(iface: &nusb::Interface, request: u8, value: u16, index: u16, data: &[u8]) -> Result<(), TransferError> {
    iface.control_out(ControlOut { control_type: ControlType::Vendor, recipient: Recipient::Device, request, value, index, data }, TIMEOUT).wait()
}

fn vendor_in(iface: &nusb::Interface, request: u8, value: u16, index: u16, length: u16) -> Result<Vec<u8>, TransferError> {
    iface.control_in(ControlIn { control_type: ControlType::Vendor, recipient: Recipient::Device, request, value, index, length }, TIMEOUT).wait()
}

/// Find and claim a board by vendor/product IDs and serial (0: the first).
fn open_iface(name: &str, serial: u64, matches: impl Fn(&nusb::DeviceInfo) -> bool) -> Result<nusb::Interface, String> {
    let list = nusb::list_devices().wait().map_err(|e| format!("list USB devices: {e}"))?;
    let mut candidates = list.filter(|d| matches(d));
    let info = if serial == 0 { candidates.next() } else { candidates.find(|d| d.serial_number().map(serial_of) == Some(serial)) };
    let Some(info) = info else {
        return Err(if serial == 0 { format!("no {name} found on USB") } else { format!("no {name} {serial:#018X} on USB") });
    };
    let dev = info.open().wait().map_err(|e| format!("open the {name}: {e} (no access? on Linux check the udev rules; on Windows the WinUSB driver)"))?;
    dev.claim_interface(0).wait().map_err(|e| format!("claim the {name}: {e} (busy in another program?)"))
}

/// An open RFOne, Airspy R2 or Airspy Mini.
pub struct R82x {
    kind: Kind,
    iface: nusb::Interface,
    pub version: String,
    rates: Vec<u32>,
    /// RFOne: the firmware's IF filter bandwidths (empty if it has none).
    bandwidths: Vec<u32>,
    /// RFOne: the bandwidth index to set once after the next receiver start (the firmware picks its own
    /// default when RX starts, so libhydrasdr overrides it then, once per rate change).
    pending_bw: Cell<Option<u16>>,
    stream: Option<Stream>,
}

impl R82x {
    /// Open by serial (0: the first one found) and set it up as the board's library does on open.
    pub fn open(kind: Kind, serial: u64) -> Result<R82x, String> {
        let iface = open_iface(kind.name(), serial, |d| kind.matches(d))?;
        let mut a = R82x { kind, iface, version: String::new(), rates: Vec::new(), bandwidths: Vec::new(), pending_bw: Cell::new(None), stream: None };
        a.version = a.read_version()?;
        // a host that died mid-sweep leaves sweep firmware sweeping, which STALLs the tuner setup
        if a.version.contains("-sweep") {
            let _ = a.out(SWEEP_STOP, 0, 0, &[]);
        }
        a.rates = match (a.read_table(GET_SAMPLERATES), kind) {
            (Ok(t), _) if !t.is_empty() => t,
            (_, Kind::Airspy) => vec![10_000_000, 2_500_000], // libairspy's fallback
            _ => Vec::new(),
        };
        if kind == Kind::RFOne {
            a.bandwidths = a.read_table(GET_BANDWIDTHS).unwrap_or_default();
        }
        a.set_packing(false)?;
        Ok(a)
    }

    fn vin(&self, request: u8, value: u16, index: u16, length: u16) -> Result<Vec<u8>, TransferError> {
        vendor_in(&self.iface, request, value, index, length)
    }

    fn out(&self, request: u8, value: u16, index: u16, data: &[u8]) -> Result<(), TransferError> {
        vendor_out(&self.iface, request, value, index, data)
    }

    /// A one-byte vendor IN request, as libairspy's setters use (the byte is a status the lib ignores).
    fn set1(&self, what: &str, request: u8, index: u16) -> Result<(), String> {
        match self.vin(request, 0, index, 1) {
            Ok(v) if !v.is_empty() => Ok(()),
            Ok(_) => Err(format!("{what}: short reply")),
            Err(e) => Err(usb_err(what, e)),
        }
    }

    fn read_version(&self) -> Result<String, String> {
        let v = self.vin(VERSION_STRING_READ, 0, 0, 127).map_err(|e| usb_err("read the version", e))?;
        let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
        Ok(String::from_utf8_lossy(&v[..end]).trim().to_string())
    }

    /// A u32 table (sample rates, bandwidths): its length with index 0, then the entries.
    fn read_table(&self, request: u8) -> Result<Vec<u32>, String> {
        let n = self.vin(request, 0, 0, 4).map_err(|e| usb_err("read a table", e))?;
        let count = u32::from_le_bytes(n.get(..4).ok_or("short table count")?.try_into().unwrap());
        if count == 0 || count >= 64 {
            return Ok(Vec::new());
        }
        let count = count as u16;
        let t = self.vin(request, 0, count, count * 4).map_err(|e| usb_err("read a table", e))?;
        Ok(t.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect())
    }

    pub fn sweep_revision(&self) -> i32 {
        sweep_revision(&self.version)
    }

    /// The firmware's sample-rate table holds IQ rates (R2: 10000000, Mini: 6000000, RFOne: 10000000 among
    /// others); a rate in it is selected by index, which works under RAW too. Anything else goes by value
    /// in kHz, which under RAW is the real ADC rate (RFOne 20000/24000). Both libraries do exactly this.
    pub fn set_samplerate(&self, hz: u32) -> Result<(), String> {
        let arg = match self.rates.iter().position(|&r| r == hz) {
            Some(i) => i as u32,
            None if hz >= 1_000_000 => hz / 1000,
            None => hz,
        };
        if self.kind == Kind::Airspy {
            self.clear_halt();
        }
        self.set1(&format!("set the sample rate ({hz})"), SET_SAMPLERATE, arg as u16)?;
        // libhydrasdr: the smallest IF bandwidth that covers the rate, set after the next RX start
        let bw = self.bandwidths.iter().enumerate().filter(|(_, b)| **b >= hz).min_by_key(|(_, b)| **b).map(|(i, _)| i as u16);
        self.pending_bw.set(bw);
        Ok(())
    }

    /// RFOne: ANT 0, CABLE1 1, CABLE2 2.
    pub fn set_rf_port(&self, port: u8) -> Result<(), String> {
        match self.vin(SET_RF_PORT, 0, port as u16, 1) {
            Ok(v) if v.first() == Some(&1) => Ok(()),
            Ok(_) => Err(format!("set RF port {port}: refused")),
            Err(e) => Err(usb_err("set RF port", e)),
        }
    }

    pub fn set_packing(&self, on: bool) -> Result<(), String> {
        self.set1("set packing", SET_PACKING, on as u16)
    }

    fn receiver_mode(&self, on: bool) -> Result<(), String> {
        self.out(RECEIVER_MODE, on as u16, 0, &[]).map_err(|e| usb_err("set the receiver mode", e))
    }

    fn clear_halt(&self) {
        if self.stream.is_none()
            && let Ok(mut ep) = self.iface.endpoint::<Bulk, In>(0x81)
        {
            let _ = ep.clear_halt().wait();
        }
    }

    pub fn setter(&self) -> Freq {
        Freq(self.iface.clone(), self.kind)
    }

    pub fn set_lna_agc(&self, on: bool) -> Result<(), String> {
        self.set1("set LNA AGC", SET_LNA_AGC, on as u16)
    }

    pub fn set_mixer_agc(&self, on: bool) -> Result<(), String> {
        self.set1("set mixer AGC", SET_MIXER_AGC, on as u16)
    }

    pub fn set_lna_gain(&self, v: u8) -> Result<(), String> {
        self.set1("set LNA gain", SET_LNA_GAIN, v.min(14) as u16)
    }

    pub fn set_mixer_gain(&self, v: u8) -> Result<(), String> {
        self.set1("set mixer gain", SET_MIXER_GAIN, v.min(15) as u16)
    }

    pub fn set_vga_gain(&self, v: u8) -> Result<(), String> {
        self.set1("set VGA gain", SET_VGA_GAIN, v.min(15) as u16)
    }

    fn preset(&self, g: u8, vga: &[u8; 22], mixer: &[u8; 22], lna: &[u8; 22]) -> Result<(), String> {
        let i = 21 - g.min(21) as usize;
        self.set_mixer_agc(false)?;
        self.set_lna_agc(false)?;
        self.set_vga_gain(vga[i])?;
        self.set_mixer_gain(mixer[i])?;
        self.set_lna_gain(lna[i])
    }

    pub fn set_linearity_gain(&self, g: u8) -> Result<(), String> {
        self.preset(g, &LIN_VGA, &LIN_MIXER, &LIN_LNA)
    }

    pub fn set_sensitivity_gain(&self, g: u8) -> Result<(), String> {
        self.preset(g, &SENS_VGA, &SENS_MIXER, &SENS_LNA)
    }

    /// Start the sweep (streaming must be on: the firmware acks but ignores a start while it's off).
    pub fn sweep_start(&self, p: &SweepParams) -> Result<(), String> {
        let rev = self.sweep_revision();
        if rev == 0 {
            return Err("this firmware has no sweep mode".into());
        }
        if p.flags & 1 != 0 && rev < 4 {
            return Err(format!("VCO-continuous order needs sweep firmware 4 or later (this is {rev})"));
        }
        if self.stream.is_none() {
            return Err("sweep start: streaming is off".into());
        }
        self.out(SWEEP_START, 0, 0, &params_bytes(p)).map_err(|e| usb_err("start the sweep", e))
    }

    pub fn sweep_stop(&self) -> Result<(), String> {
        self.out(SWEEP_STOP, 0, 0, &[]).map_err(|e| usb_err("stop the sweep", e))
    }

    /// Receiver on and the bulk stream running: each transfer goes to `on_data` as 16-bit words.
    pub fn start_rx(&mut self, mut on_data: impl FnMut(Vec<u16>) + Send + 'static) -> Result<(), String> {
        if self.stream.is_some() {
            return Ok(());
        }
        // libairspy clears the halt before RX on; libhydrasdr turns RX on, sets the bandwidth it picked
        // for the rate, then clears the halt
        self.receiver_mode(false)?;
        if self.kind == Kind::Airspy {
            self.clear_halt();
        }
        self.receiver_mode(true)?;
        if let Some(bw) = self.pending_bw.take() {
            let _ = self.vin(SET_BANDWIDTH, 0, bw, 1);
        }
        self.stream = Some(start_bulk(&self.iface, self.kind == Kind::RFOne, move |bytes| {
            on_data(bytes.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect())
        })?);
        Ok(())
    }

    /// libhydrasdr turns the receiver off first (that halts the bulk IN), libairspy stops the transfers
    /// first.
    pub fn stop_rx(&mut self) -> Result<(), String> {
        let Some(s) = self.stream.take() else { return self.receiver_mode(false) };
        s.stop.store(true, Ordering::Relaxed);
        let r = if self.kind == Kind::RFOne { self.receiver_mode(false) } else { Ok(()) };
        s.stop();
        if self.kind == Kind::Airspy { self.receiver_mode(false) } else { r }
    }

    /// What a signal handler needs to stop the board without the rest of the device.
    pub fn stopper(&self) -> impl Fn() + Send + 'static {
        let iface = self.iface.clone();
        move || {
            for (req, value) in [(SWEEP_STOP, 0), (RECEIVER_MODE, 0)] {
                let _ = vendor_out(&iface, req, value, 0, &[]);
            }
        }
    }
}

impl Drop for R82x {
    fn drop(&mut self) {
        let _ = self.stop_rx();
        // libhydrasdr's close also turns the bias tee off, for safety
        if self.kind == Kind::RFOne {
            let _ = self.out(SET_RF_BIAS, 0, 0, &[]);
        }
    }
}

/// Retunes the board from another thread (the PC-driven sweeper) while the stream runs.
#[derive(Clone)]
pub struct Freq(nusb::Interface, Kind);

impl Freq {
    /// The RFOne takes a u64, the Airspy a u32 (little-endian).
    pub fn set(&self, hz: u64) -> Result<(), String> {
        let (b64, b32) = (hz.to_le_bytes(), (hz as u32).to_le_bytes());
        let data: &[u8] = if self.1 == Kind::RFOne { &b64 } else { &b32 };
        vendor_out(&self.0, SET_FREQ, 0, 0, data).map_err(|e| usb_err("set the frequency", e))
    }
}

/// HackRF One (and its relatives on libhackrf's ID list), stock firmware: the requests Corvus Sweep uses
/// from libhackrf (host/libhackrf/src/hackrf.c, 2026.01.3), request for request.
pub struct Hackrf {
    iface: nusb::Interface,
    pub version: String,
    stream: Option<Stream>,
}

const HACKRF_VID: u16 = 0x1d50;
/// HackRF One, Jawbreaker, rad1o (libhackrf's list).
const HACKRF_PIDS: [u16; 3] = [0x6089, 0x604b, 0xcc15];
const HRF_TRANSCEIVER_MODE: u8 = 1;
const HRF_SAMPLE_RATE_SET: u8 = 6;
const HRF_BASEBAND_FILTER_SET: u8 = 7;
const HRF_VERSION_STRING_READ: u8 = 15;
const HRF_AMP_ENABLE: u8 = 17;
const HRF_SET_LNA_GAIN: u8 = 19;
const HRF_SET_VGA_GAIN: u8 = 20;
const HRF_ANTENNA_ENABLE: u8 = 23;
const HRF_INIT_SWEEP: u8 = 26;
const HRF_UI_ENABLE: u8 = 37;
const HRF_MODE_OFF: u16 = 0;
const HRF_MODE_RX_SWEEP: u16 = 5;
/// The MAX2837 baseband filter's bandwidths (libhackrf's max2837_ft).
const MAX2837_FT: [u32; 16] = [1_750_000, 2_500_000, 3_500_000, 5_000_000, 5_500_000, 6_000_000, 7_000_000, 8_000_000, 9_000_000, 10_000_000, 12_000_000, 14_000_000, 15_000_000, 20_000_000, 24_000_000, 28_000_000];

/// hackrf_compute_baseband_filter_bw: the largest filter at or under the request (the smallest if none).
pub fn baseband_filter_bw(hz: u32) -> u32 {
    MAX2837_FT.iter().copied().rfind(|&b| b <= hz).unwrap_or(MAX2837_FT[0])
}

/// hackrf_set_sample_rate's fraction: the smallest multiplier (1..31) that makes `freq * n` (nearly) whole,
/// found from the bits of the double exactly as libhackrf does. Returns (freq_hz, divider).
pub fn sample_rate_fraction(freq: f64) -> (u32, u32) {
    const MAX_N: u32 = 32;
    let freq_frac = 1.0 + freq - (freq as i64) as f64;
    let e = ((freq.to_bits() >> 52) as i64) - 1023;
    let mut m: u64 = (1u64 << 52) - 1;
    let v = freq_frac.to_bits() & m;
    m &= !((1u64 << (e + 4)) - 1);
    let mut a: u64 = 0;
    let mut i = 1;
    while i < MAX_N {
        a = a.wrapping_add(v);
        if a & m == 0 || !a & m == 0 {
            break;
        }
        i += 1;
    }
    if i == MAX_N {
        i = 1;
    }
    ((freq * i as f64 + 0.5) as u32, i)
}

impl Hackrf {
    pub fn open(serial: u64) -> Result<Hackrf, String> {
        let iface = open_iface("HackRF", serial, |d| d.vendor_id() == HACKRF_VID && HACKRF_PIDS.contains(&d.product_id()))?;
        let mut h = Hackrf { iface, version: String::new(), stream: None };
        let v = vendor_in(&h.iface, HRF_VERSION_STRING_READ, 0, 0, 255).map_err(|e| usb_err("read the version", e))?;
        let end = v.iter().position(|&b| b == 0).unwrap_or(v.len());
        h.version = String::from_utf8_lossy(&v[..end]).trim().to_string();
        Ok(h)
    }

    fn out(&self, what: &str, request: u8, value: u16, index: u16, data: &[u8]) -> Result<(), String> {
        vendor_out(&self.iface, request, value, index, data).map_err(|e| usb_err(what, e))
    }

    /// hackrf_set_sample_rate: the rate as a fraction, then the baseband filter for 3/4 of it.
    pub fn set_sample_rate(&self, freq: f64) -> Result<(), String> {
        let (hz, div) = sample_rate_fraction(freq);
        let mut data = [0u8; 8];
        data[..4].copy_from_slice(&hz.to_le_bytes());
        data[4..].copy_from_slice(&div.to_le_bytes());
        self.out("set the sample rate", HRF_SAMPLE_RATE_SET, 0, 0, &data)?;
        self.set_baseband_filter_bandwidth(baseband_filter_bw((0.75 * hz as f64 / div as f64) as u32))
    }

    pub fn set_baseband_filter_bandwidth(&self, hz: u32) -> Result<(), String> {
        self.out("set the baseband filter", HRF_BASEBAND_FILTER_SET, (hz & 0xffff) as u16, (hz >> 16) as u16, &[])
    }

    fn gain(&self, what: &str, request: u8, v: u32) -> Result<(), String> {
        match vendor_in(&self.iface, request, 0, v as u16, 1) {
            Ok(r) if r.first().is_some_and(|&b| b != 0) => Ok(()),
            Ok(_) => Err(format!("{what} {v}: refused")),
            Err(e) => Err(usb_err(what, e)),
        }
    }

    /// IF gain 0-40 dB in 8 dB steps.
    pub fn set_lna_gain(&self, v: u32) -> Result<(), String> {
        if v > 40 {
            return Err(format!("LNA gain {v} > 40"));
        }
        self.gain("set LNA gain", HRF_SET_LNA_GAIN, v & !7)
    }

    /// Baseband gain 0-62 dB in 2 dB steps.
    pub fn set_vga_gain(&self, v: u32) -> Result<(), String> {
        if v > 62 {
            return Err(format!("VGA gain {v} > 62"));
        }
        self.gain("set VGA gain", HRF_SET_VGA_GAIN, v & !1)
    }

    pub fn set_amp_enable(&self, on: bool) -> Result<(), String> {
        self.out("set the RF amp", HRF_AMP_ENABLE, on as u16, 0, &[])
    }

    /// Antenna port power (the bias tee).
    pub fn set_antenna_enable(&self, on: bool) -> Result<(), String> {
        self.out("set antenna port power", HRF_ANTENNA_ENABLE, on as u16, 0, &[])
    }

    /// The PortaPack UI (a HackRF without one ignores it).
    pub fn set_ui_enable(&self, on: bool) {
        let _ = vendor_out(&self.iface, HRF_UI_ENABLE, on as u16, 0, &[]);
    }

    /// hackrf_init_sweep with one range [lo_mhz, hi_mhz].
    pub fn init_sweep(&self, lo_mhz: u16, hi_mhz: u16, num_bytes: u32, step_hz: u32, offset_hz: u32, interleaved: bool) -> Result<(), String> {
        let mut data = [0u8; 13];
        data[..4].copy_from_slice(&step_hz.to_le_bytes());
        data[4..8].copy_from_slice(&offset_hz.to_le_bytes());
        data[8] = interleaved as u8;
        data[9..11].copy_from_slice(&lo_mhz.to_le_bytes());
        data[11..13].copy_from_slice(&hi_mhz.to_le_bytes());
        self.out("set up the sweep", HRF_INIT_SWEEP, (num_bytes & 0xffff) as u16, (num_bytes >> 16) as u16, &data)
    }

    /// hackrf_start_rx_sweep: sweep mode on, then the bulk stream (raw bytes to `on_bytes`).
    pub fn start_rx_sweep(&mut self, on_bytes: impl FnMut(&[u8]) + Send + 'static) -> Result<(), String> {
        if self.stream.is_some() {
            return Ok(());
        }
        self.out("start the sweep", HRF_TRANSCEIVER_MODE, HRF_MODE_RX_SWEEP, 0, &[])?;
        self.stream = Some(start_bulk(&self.iface, false, on_bytes)?);
        Ok(())
    }

    /// hackrf_stop_rx: the transfers first, then the transceiver off.
    pub fn stop_rx(&mut self) -> Result<(), String> {
        if let Some(s) = self.stream.take() {
            s.stop();
        }
        self.out("stop", HRF_TRANSCEIVER_MODE, HRF_MODE_OFF, 0, &[])
    }

    pub fn stopper(&self) -> impl Fn() + Send + 'static {
        let iface = self.iface.clone();
        move || {
            let _ = vendor_out(&iface, HRF_TRANSCEIVER_MODE, HRF_MODE_OFF, 0, &[]);
        }
    }
}

impl Drop for Hackrf {
    fn drop(&mut self) {
        let _ = self.stop_rx();
        self.set_ui_enable(true);
    }
}

/// The 16-byte sweep_params_t, little-endian, as airspy_sweep_start packs it.
pub fn params_bytes(p: &SweepParams) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[0..4].copy_from_slice(&p.f_start_hz.to_le_bytes());
    b[4..8].copy_from_slice(&p.f_step_hz.to_le_bytes());
    b[8..10].copy_from_slice(&p.n_steps.to_le_bytes());
    b[10] = p.skip_blocks;
    b[11] = p.dwell_blocks;
    b[12] = p.lock_reads;
    b[13] = p.flags;
    b[14..16].copy_from_slice(&p.lock_timeout.to_le_bytes());
    b
}

/// ADALM-Pluto and its clones in their normal (not DFU) mode.
const PLUTO_VID: u16 = 0x0456;
const PLUTO_PID: u16 = 0xB673;

/// Boards on USB (all platforms): RFOne, Airspy R2/Mini, HackRF, by vendor/product ID and serial string.
pub fn list() -> Vec<crate::hydra::Found> {
    use crate::hydra::{Board, Found};
    let Ok(devs) = nusb::list_devices().wait() else { return Vec::new() };
    let mut out: Vec<Found> = devs
        .filter_map(|d| {
            let sn = d.serial_number().unwrap_or("").to_string();
            let serial = serial_of(&sn);
            match (d.vendor_id(), d.product_id()) {
                (RFONE_VID, RFONE_PID) => Some(Found { board: Board::RFOne, serial, label: "HydraSDR RFOne".into() }),
                (AIRSPY_VID, AIRSPY_PID) if sn.starts_with("HYDRASDR") => Some(Found { board: Board::RFOne, serial, label: "HydraSDR RFOne (old firmware ID)".into() }),
                (AIRSPY_VID, AIRSPY_PID) => Some(Found { board: Board::Airspy, serial, label: "Airspy (R2 or Mini)".into() }),
                (HACKRF_VID, pid) if HACKRF_PIDS.contains(&pid) => Some(Found { board: Board::HackRF, serial, label: "HackRF One".into() }),
                // a Pluto on USB (its network link); Corvus reaches it over the network, not this device
                (PLUTO_VID, PLUTO_PID) => Some(Found { board: Board::Pluto, serial, label: "Pluto".into() }),
                // an RTL2832U dongle: listed even before its library is downloaded (opening it offers that)
                (vid, pid) => crate::rtl::id_name(vid, pid).map(|n| Found { board: Board::Rtl, serial, label: format!("RTL-SDR ({n})") }),
            }
        })
        .collect();
    out.sort_by_key(|f| (f.board as u8, f.serial));
    // RTL-SDR dongles sharing a serial: number them, each with its own key (rtl::dup_key)
    let mut i = 0;
    while i < out.len() {
        let same = out[i..].iter().take_while(|f| f.board == Board::Rtl && f.board == out[i].board && f.serial == out[i].serial).count();
        if same > 1 {
            for (k, f) in out[i..i + same].iter_mut().enumerate() {
                f.serial = crate::rtl::dup_key(f.serial, k);
                f.label = format!("{} #{}", f.label, k + 1);
            }
        }
        i += same.max(1);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serials_and_revisions_parse() {
        assert_eq!(serial_of("AIRSPY SN:0123456789ABCDEF"), 0x0123456789ABCDEF);
        assert_eq!(serial_of("HYDRASDR SN:FEDCBA9876543210"), 0xFEDCBA9876543210);
        assert_eq!(serial_of("0000000000000000a1b2c3d4e5f60718"), 0xa1b2c3d4e5f60718);
        assert_eq!(sweep_revision("AirSpy NOS v1.0.0-rc10-6-8-gcf1a374-sweep5 2026-11-08"), 5);
        assert_eq!(sweep_revision("AirSpy MINI v1.0.0-rc10-sweep"), 1);
        assert_eq!(sweep_revision("AirSpy NOS v1.0.0-rc10-0-g946184a 2016-09-19"), 0);
    }

    #[test]
    fn hackrf_rate_and_filter_match_libhackrf() {
        assert_eq!(sample_rate_fraction(20e6), (20_000_000, 1));
        assert_eq!(sample_rate_fraction(10e6), (10_000_000, 1));
        // 8.333... MSPS needs a multiplier of 3
        assert_eq!(sample_rate_fraction(25e6 / 3.0), (25_000_000, 3));
        assert_eq!(baseband_filter_bw(15_000_000), 15_000_000);
        assert_eq!(baseband_filter_bw(16_000_000), 15_000_000);
        assert_eq!(baseband_filter_bw(1_000_000), 1_750_000);
        assert_eq!(baseband_filter_bw(30_000_000), 28_000_000);
    }

    #[test]
    fn params_pack_like_libairspy() {
        let p = SweepParams { f_start_hz: 0x0102_0304, f_step_hz: 10_000_000, n_steps: 178, skip_blocks: 1, dwell_blocks: 4, lock_reads: 2, flags: 1, lock_timeout: 200 };
        let b = params_bytes(&p);
        assert_eq!(&b[0..4], &[4, 3, 2, 1]);
        assert_eq!(u32::from_le_bytes(b[4..8].try_into().unwrap()), 10_000_000);
        assert_eq!(&b[8..16], &[178, 0, 1, 4, 2, 1, 200, 0]);
    }
}
