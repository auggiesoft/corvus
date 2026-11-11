//! Fast mode: the sweep drives the AD936x itself instead of retuning through the driver.
//!
//! - **Retune:** each step's RX synthesizer settings are calibrated once through the driver (a normal retune,
//!   then the driver's fastlock store/save), then the sweep switches between them with the chip's fastlock
//!   profiles: one register write, settled in about a microsecond. The chip holds 8 profiles; the one 7 steps
//!   ahead is written into the slot just left while the current step's block fills.
//! - **Registers:** written over the Zynq's SPI0 controller directly (/dev/mem), ~2 us per transaction
//!   instead of ~45 us through the kernel. Only while nothing else uses the bus: the controller must be idle
//!   the way the kernel leaves it, the bus quiet, and its interrupt count unchanged after every pass.
//! - **Gain tables:** fastlock doesn't reload the RX gain table the driver keeps per LO band (to 1.3 GHz,
//!   1.3-4 GHz, 4-6 GHz), so the sweep writes it where a pass crosses a band edge (~0.5 ms), with the index
//!   that gives the same gain in dB.
//! - **Capture:** the IIO mmap block interface, so a block starts filling only when it's queued, after the
//!   switch: no stale blocks to drop.
//!
//! Handing the radio back (STOP, CONFIG, a new plan, the end of a session): the controller's settings
//! restored, then two driver retunes in different bands, so the driver leaves fastlock mode and reloads the
//! gain table it believes is loaded.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::gaintable::{ABS_GAIN_DB, BANDS, SIZE, TABLE};

/// SPI0, the AD936x's bus on a Pluto (checked against the IIO device's sysfs path).
const SPI0_BASE: libc::off_t = 0xE000_6000;
const SPI0_NAME: &str = "e0006000.spi";
const CR: usize = 0x00;
const ISR: usize = 0x04;
const IMR: usize = 0x10;
const ER: usize = 0x14;
const TXD: usize = 0x1C;
const RXD: usize = 0x20;
const ISR_RX_NEMPTY: u32 = 1 << 4;
/// SCLK = SPI reference / 8 (~20 MHz; the kernel uses /32). Reads verified correct down to /4.
const SPI_DIV: u32 = 2;

const REG_GAIN_TABLE_WRITE_DATA3: u16 = 0x133;
const REG_GAIN_TABLE_READ_DATA1: u16 = 0x134;
const REG_GAIN_TABLE_CONFIG: u16 = 0x137;
const GT_START_CLOCK: u8 = 1 << 1;
const GT_WRITE: u8 = 1 << 2;
const GT_RX1_RX2: u8 = 3 << 3;
const REG_RX1_MANUAL_GAIN: u16 = 0x109;
const REG_RX_VCO_LOCK: u16 = 0x247;
const VCO_LOCK: u8 = 1 << 1;
const REG_RX_FAST_LOCK_SETUP: u16 = 0x25A;
const REG_RX_FAST_LOCK_PROGRAM_DATA: u16 = 0x25D;
const REG_RX_FAST_LOCK_PROGRAM_CTRL: u16 = 0x25F;
const FL_PROGRAM_WRITE_CLOCK: u8 = 0x03;

/// Samples dropped at the start of each block (the pipeline around the switch; 4 us at 61.44 MSPS).
pub const DISCARD: usize = 256;
/// Capture blocks allocated: the most one step can queue at once.
pub const NBLOCKS: usize = 16;
/// Profiles older than this are calibrated again (the VCO drifts with temperature).
const PROFILE_MAX_AGE: Duration = Duration::from_secs(600);

/// The LO band (gain table) of a frequency, as the driver picks it: start < f <= end.
pub fn band(f: u64) -> usize {
    BANDS.iter().position(|&(s, e)| s < f && f <= e).unwrap_or(BANDS.len() - 1)
}

/// The table index nearest `db` in a band (the driver's find_table_index).
pub fn gain_index(band: usize, db: i32) -> u8 {
    let t = &ABS_GAIN_DB[band];
    for i in 0..SIZE {
        if t[i] as i32 >= db {
            let p = i.saturating_sub(1);
            return if (t[p] as i32 - db).abs() < (t[i] as i32 - db).abs() { p as u8 } else { i as u8 };
        }
    }
    (SIZE - 1) as u8
}

// ---------------------------------------------------------------------------------------------------------
// SPI0

pub struct Spi {
    base: *mut u32,
    cr_kernel: u32,
    cr: u32,
}

// The mapping is only touched from the sweep thread.
unsafe impl Send for Spi {}

impl Spi {
    fn open() -> Result<Spi, String> {
        unsafe {
            let fd = libc::open(c"/dev/mem".as_ptr(), libc::O_RDWR | libc::O_SYNC);
            if fd < 0 {
                return Err(format!("/dev/mem: {}", std::io::Error::last_os_error()));
            }
            let p = libc::mmap(std::ptr::null_mut(), 4096, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, SPI0_BASE);
            libc::close(fd);
            if p == libc::MAP_FAILED {
                return Err(format!("mmap SPI0: {}", std::io::Error::last_os_error()));
            }
            let base = p as *mut u32;
            let cr = std::ptr::read_volatile(base.add(CR / 4));
            Ok(Spi { base, cr_kernel: cr, cr: (cr & !(7 << 3)) | (SPI_DIV << 3) })
        }
    }

    fn rd(&self, off: usize) -> u32 {
        unsafe { std::ptr::read_volatile(self.base.add(off / 4)) }
    }

    fn wr(&self, off: usize, v: u32) {
        unsafe { std::ptr::write_volatile(self.base.add(off / 4), v) }
    }

    /// Idle the way the kernel's driver leaves it: disabled, interrupts masked.
    fn idle(&self) -> bool {
        self.rd(ER) == 0 && self.rd(IMR) == 0
    }

    /// One transaction on CS0: `tx` out, as many bytes back into `rx`.
    fn xfer(&self, tx: &[u8], rx: &mut [u8]) -> Result<(), String> {
        if !self.idle() {
            return Err("the SPI controller is in use by someone else".into());
        }
        while self.rd(ISR) & ISR_RX_NEMPTY != 0 {
            self.rd(RXD);
        }
        self.wr(CR, (self.cr & !(0xF << 10)) | (0xE << 10));
        self.wr(ER, 1);
        for &b in tx {
            self.wr(TXD, b as u32);
        }
        let mut res = Ok(());
        'bytes: for r in rx.iter_mut() {
            let mut n = 0u32;
            while self.rd(ISR) & ISR_RX_NEMPTY == 0 {
                n += 1;
                if n > 200_000 {
                    res = Err("SPI timeout".to_string());
                    break 'bytes;
                }
            }
            *r = self.rd(RXD) as u8;
        }
        self.wr(ER, 0);
        self.wr(CR, self.cr_kernel);
        res
    }

    /// AD936x instruction word: bit 15 write, bits 14:12 byte count - 1, bits 9:0 the (first) address;
    /// multi-byte transfers count the address down.
    fn read_reg(&self, reg: u16) -> Result<u8, String> {
        let mut rx = [0u8; 3];
        self.xfer(&[(reg >> 8) as u8 & 0x3, reg as u8, 0], &mut rx)?;
        Ok(rx[2])
    }

    fn writem(&self, reg: u16, vals: &[u8]) -> Result<(), String> {
        let cmd: u16 = 0x8000 | (((vals.len() as u16 - 1) & 7) << 12) | (reg & 0x3FF);
        let mut tx = [0u8; 10];
        tx[0] = (cmd >> 8) as u8;
        tx[1] = cmd as u8;
        tx[2..2 + vals.len()].copy_from_slice(vals);
        let mut rx = [0u8; 10];
        self.xfer(&tx[..2 + vals.len()], &mut rx[..2 + vals.len()])
    }

    fn write_reg(&self, reg: u16, val: u8) -> Result<(), String> {
        self.writem(reg, &[val])
    }

    /// The driver's fastlock_load, for the listed words of RX profile `slot` only: the first word's data and
    /// address, then per further word one transfer that commits the previous word and stages the next.
    fn fastlock_load(&self, slot: u8, v: &[u8; 16], words: &[u8]) -> Result<(), String> {
        let Some((&first, rest)) = words.split_first() else { return Ok(()) };
        let addr = |w: u8| ((slot & 7) << 4) | (w & 0xF);
        self.writem(REG_RX_FAST_LOCK_PROGRAM_DATA, &[v[first as usize], addr(first)])?;
        for &w in rest {
            // 0x25F (write the staged word), 0x25E, 0x25D (data), 0x25C (address)
            self.writem(REG_RX_FAST_LOCK_PROGRAM_CTRL, &[FL_PROGRAM_WRITE_CLOCK, 0, v[w as usize], addr(w)])?;
        }
        self.write_reg(REG_RX_FAST_LOCK_PROGRAM_CTRL, FL_PROGRAM_WRITE_CLOCK)?;
        self.write_reg(REG_RX_FAST_LOCK_PROGRAM_CTRL, 0)
    }

    /// The driver's load_gt for RX1+RX2, then the manual gain index for the new table.
    fn load_gain_table(&self, band: usize, index: u8) -> Result<(), String> {
        self.write_reg(REG_GAIN_TABLE_CONFIG, GT_START_CLOCK | GT_RX1_RX2)?;
        for (i, w) in TABLE[band].iter().enumerate() {
            // 0x133 (word 3), 0x132, 0x131, 0x130 (index)
            self.writem(REG_GAIN_TABLE_WRITE_DATA3, &[w[2], w[1], w[0], i as u8])?;
            self.write_reg(REG_GAIN_TABLE_CONFIG, GT_START_CLOCK | GT_WRITE | GT_RX1_RX2)?;
            self.write_reg(REG_GAIN_TABLE_READ_DATA1, 0)?; // dummy write: the delay the driver leaves
        }
        self.write_reg(REG_GAIN_TABLE_CONFIG, GT_START_CLOCK | GT_RX1_RX2)?;
        self.write_reg(REG_GAIN_TABLE_READ_DATA1, 0)?;
        self.write_reg(REG_GAIN_TABLE_READ_DATA1, 0)?;
        self.write_reg(REG_GAIN_TABLE_CONFIG, 0)?;
        self.set_gain_index(index)
    }

    fn set_gain_index(&self, index: u8) -> Result<(), String> {
        let v = self.read_reg(REG_RX1_MANUAL_GAIN)?;
        self.write_reg(REG_RX1_MANUAL_GAIN, (v & 0x80) | (index & 0x7F))
    }

    fn locked(&self) -> Result<bool, String> {
        Ok(self.read_reg(REG_RX_VCO_LOCK)? & VCO_LOCK != 0)
    }
}

impl Drop for Spi {
    fn drop(&mut self) {
        self.wr(ER, 0);
        self.wr(CR, self.cr_kernel);
        unsafe { libc::munmap(self.base as *mut libc::c_void, 4096) };
    }
}

/// SPI0's interrupt count: the kernel's transfers raise interrupts, ours don't (they're masked).
fn spi_irqs() -> u64 {
    fs::read_to_string("/proc/interrupts")
        .unwrap_or_default()
        .lines()
        .find(|l| l.contains(SPI0_NAME))
        .and_then(|l| l.split_whitespace().nth(1).and_then(|v| v.parse().ok()))
        .unwrap_or(0)
}

/// Move the capture DMA interrupts to CPU1, where the sweep loop waits for them: on CPU0 they queue behind
/// the network interrupt and the TCP sending (~20% more steps/s on a Pluto+ over Ethernet). Returns what to
/// restore.
fn dma_irqs_to_cpu1() -> Vec<(String, String)> {
    if crate::cpus() < 2 {
        return Vec::new();
    }
    let mut moved = Vec::new();
    for l in fs::read_to_string("/proc/interrupts").unwrap_or_default().lines() {
        if !l.trim_end().ends_with(".dma") {
            continue;
        }
        let irq = l.split(':').next().unwrap_or("").trim().to_string();
        let path = format!("/proc/irq/{irq}/smp_affinity_list");
        if let Ok(old) = fs::read_to_string(&path)
            && fs::write(&path, "1").is_ok()
        {
            moved.push((irq, old.trim().to_string()));
        }
    }
    moved
}

fn spi0_power() -> PathBuf {
    PathBuf::from(format!("/sys/devices/soc0/amba/{SPI0_NAME}/power/control"))
}

// ---------------------------------------------------------------------------------------------------------
// Capture blocks (IIO mmap interface: ADI kernels' IIO_BLOCK_*_IOCTL)

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct BlockAllocReq {
    typ: u32,
    size: u32,
    count: u32,
    id: u32,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct BlockDesc {
    id: u32,
    size: u32,
    bytes_used: u32,
    typ: u32,
    flags: u32,
    offset: u32,
    timestamp: u64,
}

const fn iowr(nr: u32, size: usize) -> libc::Ioctl {
    ((3u32 << 30) | ((size as u32) << 16) | ((b'i' as u32) << 8) | nr) as libc::Ioctl
}
const BLOCK_ALLOC: libc::Ioctl = iowr(0xa0, std::mem::size_of::<BlockAllocReq>());
const BLOCK_QUERY: libc::Ioctl = iowr(0xa2, std::mem::size_of::<BlockDesc>());
const BLOCK_ENQUEUE: libc::Ioctl = iowr(0xa3, std::mem::size_of::<BlockDesc>());
const BLOCK_DEQUEUE: libc::Ioctl = iowr(0xa4, std::mem::size_of::<BlockDesc>());

struct Blocks {
    fd: i32,
    bufs: Vec<(*const u8, BlockDesc)>,
    rx: PathBuf,
}

unsafe impl Send for Blocks {}

impl Blocks {
    fn open(rx: &Path, samples: usize) -> Result<Blocks, String> {
        let bytes = (samples * 4) as u32;
        let w = |p: PathBuf, v: &str| fs::write(&p, v).map_err(|e| format!("{} = {v}: {e}", p.display()));
        let se = rx.join("scan_elements");
        w(rx.join("buffer/enable"), "0")?;
        w(se.join("in_voltage0_en"), "1")?;
        w(se.join("in_voltage1_en"), "1")?;
        w(rx.join("buffer/length"), &samples.to_string())?;
        let devname = rx.file_name().unwrap().to_string_lossy().to_string();
        let path = std::ffi::CString::new(format!("/dev/{devname}")).unwrap();
        let mut b = Blocks { fd: -1, bufs: Vec::new(), rx: rx.to_path_buf() };
        unsafe {
            b.fd = libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NONBLOCK);
            if b.fd < 0 {
                return Err(format!("/dev/{devname}: {}", std::io::Error::last_os_error()));
            }
            let mut req = BlockAllocReq { typ: 0, size: bytes, count: NBLOCKS as u32, id: 0 };
            if libc::ioctl(b.fd, BLOCK_ALLOC, &mut req) != 0 || req.count != NBLOCKS as u32 {
                return Err(format!("no mmap capture blocks ({})", std::io::Error::last_os_error()));
            }
            for id in 0..NBLOCKS as u32 {
                let mut d = BlockDesc { id, ..Default::default() };
                if libc::ioctl(b.fd, BLOCK_QUERY, &mut d) != 0 {
                    return Err(format!("block query: {}", std::io::Error::last_os_error()));
                }
                let p = libc::mmap(std::ptr::null_mut(), d.size as usize, libc::PROT_READ, libc::MAP_SHARED, b.fd, d.offset as libc::off_t);
                if p == libc::MAP_FAILED {
                    return Err(format!("block mmap: {}", std::io::Error::last_os_error()));
                }
                b.bufs.push((p as *const u8, d));
            }
        }
        w(rx.join("buffer/enable"), "1")?;
        Ok(b)
    }

    fn enqueue(&self, id: usize) -> Result<(), String> {
        let mut d = self.bufs[id].1;
        d.bytes_used = d.size;
        if unsafe { libc::ioctl(self.fd, BLOCK_ENQUEUE, &mut d) } != 0 {
            return Err(format!("block enqueue: {}", std::io::Error::last_os_error()));
        }
        Ok(())
    }

    /// Spin until the oldest queued block is full (a block takes ~70 us; sleeping would cost more).
    fn dequeue(&self) -> Result<usize, String> {
        let mut d = BlockDesc::default();
        let t = Instant::now();
        loop {
            if unsafe { libc::ioctl(self.fd, BLOCK_DEQUEUE, &mut d) } == 0 {
                return Ok(d.id as usize);
            }
            let e = std::io::Error::last_os_error();
            if e.raw_os_error() != Some(libc::EAGAIN) {
                return Err(format!("block dequeue: {e}"));
            }
            if t.elapsed() > Duration::from_secs(1) {
                return Err("capture stalled".into());
            }
        }
    }

    fn data(&self, id: usize) -> &[u8] {
        let (p, d) = self.bufs[id];
        unsafe { std::slice::from_raw_parts(p, d.size as usize) }
    }
}

impl Drop for Blocks {
    fn drop(&mut self) {
        let _ = fs::write(self.rx.join("buffer/enable"), "0");
        for &(p, d) in &self.bufs {
            unsafe { libc::munmap(p as *mut libc::c_void, d.size as usize) };
        }
        if self.fd >= 0 {
            unsafe { libc::close(self.fd) };
        }
    }
}

// ---------------------------------------------------------------------------------------------------------
// Fast mode

/// Why fast mode isn't available on this board, if it isn't.
pub fn unavailable(phy: &Path) -> Option<String> {
    let real = fs::canonicalize(phy).ok()?;
    if !real.to_string_lossy().contains(&format!("{SPI0_NAME}/")) {
        return Some(format!("the AD936x isn't on {SPI0_NAME}"));
    }
    if !phy.join("out_altvoltage0_RX_LO_fastlock_save").exists() {
        return Some("the driver has no fastlock attributes".into());
    }
    if fs::metadata("/dev/mem").is_err() {
        return Some("no /dev/mem".into());
    }
    None
}

/// The driver side: what fast mode needs from the normal IIO path.
pub trait Driver {
    fn phy(&self) -> &Path;
    fn rx(&self) -> &Path;
    /// A normal driver retune.
    fn tune(&self, hz: u64) -> Result<(), String>;
    fn stop_capture(&mut self);
}

/// Calibrated fastlock profiles by LO frequency.
#[derive(Default)]
pub struct Profiles {
    map: HashMap<u64, ([u8; 16], Instant)>,
}

impl Profiles {
    /// Calibrate every frequency that has no fresh profile (a driver retune, then the driver's fastlock
    /// store and save into profile 0).
    pub fn calibrate(&mut self, d: &impl Driver, freqs: &[u64]) -> Result<usize, String> {
        let phy = d.phy();
        let store = phy.join("out_altvoltage0_RX_LO_fastlock_store");
        let save = phy.join("out_altvoltage0_RX_LO_fastlock_save");
        let mut n = 0;
        for &f in freqs {
            if self.map.get(&f).is_some_and(|(_, t)| t.elapsed() < PROFILE_MAX_AGE) {
                continue;
            }
            d.tune(f)?;
            fs::write(&store, "0").map_err(|e| format!("fastlock store: {e}"))?;
            fs::write(&save, "0").map_err(|e| format!("fastlock save: {e}"))?;
            let s = fs::read_to_string(&save).map_err(|e| format!("fastlock save: {e}"))?;
            let v: Vec<u8> = s.split_whitespace().nth(1).unwrap_or("").split(',').filter_map(|x| x.trim().parse().ok()).collect();
            let p: [u8; 16] = v.as_slice().try_into().map_err(|_| format!("fastlock save: unexpected {s:?}"))?;
            self.map.insert(f, (p, Instant::now()));
            n += 1;
        }
        Ok(n)
    }

    pub fn get(&self, f: u64) -> [u8; 16] {
        self.map[&f].0
    }

    pub fn forget(&mut self, f: u64) {
        self.map.remove(&f);
    }

    pub fn stale(&self, freqs: &[u64]) -> bool {
        freqs.iter().any(|f| self.map.get(f).is_none_or(|(_, t)| t.elapsed() >= PROFILE_MAX_AGE))
    }
}

/// The radio while fast mode owns it.
pub struct Fast {
    spi: Spi,
    blocks: Blocks,
    power: PathBuf,
    irq: u64,
    /// The gain table loaded in the chip, and the one the driver believes is loaded.
    chip_band: usize,
    driver_band: usize,
    gain_db: i32,
    /// The ALC word written with the previous visit's profile (the driver's workaround: two consecutive
    /// profiles must not share it, or the synthesizer may not lock).
    last_alc: Option<u8>,
    /// Visits so far: visit k uses profile slot k % 8. Starts at 1: the driver's recall left slot 0 active.
    seq: usize,
    /// What each profile slot holds, as far as we wrote it (only changed words are rewritten).
    slots: [Option<[u8; 16]>; 8],
    /// Capture DMA interrupts moved to CPU1 (next to the sweep loop), with their old affinity.
    irqs: Vec<(String, String)>,
    next_block: usize,
    pub unlocked: u32,
}

impl Fast {
    /// Take the radio: put the synthesizer in fastlock mode through the driver (at `f0`), check the bus is
    /// ours, and open the capture blocks.
    pub fn take(d: &mut impl Driver, f0: u64, gain_db: i32, samples: usize) -> Result<Fast, String> {
        let phy = d.phy().to_path_buf();
        d.stop_capture();
        d.tune(f0)?;
        let w = |a: &str, v: &str| fs::write(phy.join(a), v).map_err(|e| format!("{a}: {e}"));
        w("out_altvoltage0_RX_LO_fastlock_store", "0")?;
        w("out_altvoltage0_RX_LO_fastlock_recall", "0")?;
        let power = spi0_power();
        fs::write(&power, "on").map_err(|e| format!("{}: {e}", power.display()))?;
        let quiet = || -> Result<(Spi, u64), String> {
            let a = spi_irqs();
            std::thread::sleep(Duration::from_millis(30));
            if spi_irqs() != a {
                return Err("another program is using the radio (SPI traffic)".into());
            }
            let spi = Spi::open()?;
            if !spi.idle() {
                return Err("the SPI controller is busy".into());
            }
            Ok((spi, a))
        };
        let (spi, irq) = match quiet() {
            Ok(v) => v,
            Err(e) => {
                let _ = fs::write(&power, "auto");
                return Err(e);
            }
        };
        let blocks = match Blocks::open(d.rx(), samples) {
            Ok(b) => b,
            Err(e) => {
                drop(spi);
                let _ = fs::write(&power, "auto");
                return Err(e);
            }
        };
        let b = band(f0);
        Ok(Fast { spi, blocks, power, irq, chip_band: b, driver_band: b, gain_db, last_alc: None, seq: 1, slots: [None; 8], irqs: dma_irqs_to_cpu1(), next_block: 0, unlocked: 0 })
    }

    /// The bus stayed ours (no kernel SPI traffic since we took it).
    pub fn bus_ok(&self) -> bool {
        spi_irqs() == self.irq
    }

    pub fn set_gain(&mut self, db: i32) -> Result<(), String> {
        self.gain_db = db;
        self.spi.set_gain_index(gain_index(self.chip_band, db))
    }

    /// Write `v` into `slot`, adjusting the ALC word against the previously loaded profile.
    fn load(&mut self, slot: usize, mut v: [u8; 16]) -> Result<(), String> {
        if self.last_alc.is_some_and(|a| a >> 1 == v[15] >> 1) {
            v[15] = v[15].wrapping_add(2);
        }
        self.last_alc = Some(v[15]);
        let words: Vec<u8> = match self.slots[slot] {
            Some(old) => (0..16u8).filter(|&w| old[w as usize] != v[w as usize]).collect(),
            None => (0..16u8).collect(),
        };
        self.slots[slot] = None; // unknown if the write fails part-way
        self.spi.fastlock_load(slot as u8, &v, &words)?;
        self.slots[slot] = Some(v);
        Ok(())
    }

    /// One pass: visit `freqs` in order, `skip` blocks dropped and `dwell` kept per step; `emit(i, block,
    /// iq, locked)` gets each kept block's samples. Returns early (Ok(false)) when `emit` says stop.
    pub fn pass(&mut self, freqs: &[u64], profiles: &Profiles, skip: usize, dwell: usize, mut emit: impl FnMut(usize, u16, &[u8], bool) -> bool) -> Result<bool, String> {
        let n = freqs.len();
        let base = self.seq;
        let slot = |s: usize| (base + s) % 8;
        // the first 7 visits into the 7 slots that aren't active (the 8th is the one in use until visit 0)
        for (s, &f) in freqs.iter().enumerate().take(7) {
            self.load(slot(s), profiles.get(f))?;
        }
        self.seq += n;
        for s in 0..n {
            let b = band(freqs[s]);
            if b != self.chip_band {
                self.spi.load_gain_table(b, gain_index(b, self.gain_db))?;
                self.chip_band = b;
            }
            self.spi.write_reg(REG_RX_FAST_LOCK_SETUP, ((slot(s) as u8) << 5) | 1)?;
            let mut left = skip + dwell;
            let mut kept = 0u16;
            let mut first = true;
            while left > 0 {
                let k = left.min(NBLOCKS);
                let ids: Vec<usize> = (0..k).map(|j| (self.next_block + j) % NBLOCKS).collect();
                for &id in &ids {
                    self.blocks.enqueue(id)?;
                }
                self.next_block = (self.next_block + k) % NBLOCKS;
                if first && s + 7 < n {
                    // while the blocks fill: the profile 7 visits ahead into the slot just left
                    self.load(slot(s + 7), profiles.get(freqs[s + 7]))?;
                }
                let locked = if first { self.spi.locked()? } else { true };
                if !locked {
                    self.unlocked += 1;
                }
                first = false;
                for &id in &ids {
                    let got = self.blocks.dequeue()?;
                    if got != id {
                        return Err(format!("capture blocks out of order ({got} for {id})"));
                    }
                    let done = skip + dwell - left;
                    left -= 1;
                    if done >= skip {
                        let d = self.blocks.data(id);
                        if !emit(s, kept, &d[DISCARD * 4..], locked) {
                            return Ok(false);
                        }
                        kept += 1;
                    }
                }
            }
        }
        Ok(true)
    }

    /// Give the radio back to the driver.
    pub fn release(self, d: &mut impl Driver, resume_hz: u64) -> Result<(), String> {
        let gain = gain_index(self.driver_band, self.gain_db);
        let back = self.spi.set_gain_index(gain);
        let other = BANDS[(self.driver_band + 1) % BANDS.len()].1 - 1_000_000;
        for (irq, aff) in &self.irqs {
            let _ = fs::write(format!("/proc/irq/{irq}/smp_affinity_list"), aff);
        }
        drop(self.blocks);
        drop(self.spi);
        let _ = fs::write(&self.power, "auto");
        // two driver retunes in different bands: out of fastlock mode, and the driver's gain table reloaded
        d.tune(other)?;
        d.tune(resume_hz)?;
        back
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bands_follow_the_driver() {
        assert_eq!(band(70_000_000), 0);
        assert_eq!(band(1_300_000_000), 0);
        assert_eq!(band(1_300_000_001), 1);
        assert_eq!(band(4_000_000_000), 1);
        assert_eq!(band(5_950_000_000), 2);
    }

    #[test]
    fn gain_index_picks_the_nearest_entry() {
        for b in 0..3 {
            for db in [0, 10, 40, 71] {
                let i = gain_index(b, db) as usize;
                let best = (0..SIZE).map(|j| (ABS_GAIN_DB[b][j] as i32 - db).abs()).min().unwrap();
                assert_eq!((ABS_GAIN_DB[b][i] as i32 - db).abs(), best, "band {b} {db} dB");
            }
        }
    }

    #[test]
    fn spi_instruction_words() {
        // write, 4 bytes, from 0x133: 0xB1 0x33
        let cmd: u16 = 0x8000 | ((4 - 1) << 12) | 0x133;
        assert_eq!(cmd.to_be_bytes(), [0xB1, 0x33]);
    }
}
