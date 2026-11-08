//! Sweep plans: the requested range turned into the firmware's step grid, plus the tuner facts the
//! UI shows (VCO divider switch points) and the visiting order the firmware uses.

use serde::{Deserialize, Serialize};

pub const F_MIN_HZ: f64 = 24e6;
pub const F_MAX_HZ: f64 = 1800e6;
/// Samples per DMA block, the unit of the firmware's block counter.
pub const BLOCK_WORDS: usize = 8192;

/// The RFOne sample-rate setting the sweep runs at. The ADC samples the tuner's IF (a quarter of the
/// ADC rate) as real samples; the tuner IF filter is set with it. See docs/measurements.md, "12 MSPS".
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
pub enum Rate {
    /// 20 MSPS real, IF 5 MHz, tuner filter 59. Flat ±1 dB over −4.70…+4.20 MHz.
    M10,
    /// 24 MSPS real, IF 6 MHz, tuner filter 63 (widest). Flat ±1 dB over −4.85…+5.15 MHz, and
    /// each block takes 341 µs instead of 410 µs: +33 % GHz/s at 10 MHz steps.
    #[default]
    M12,
}

impl Rate {
    /// The IQ rate libhydrasdr is asked for.
    pub fn msps(self) -> u32 {
        match self {
            Rate::M10 => 10,
            Rate::M12 => 12,
        }
    }

    pub fn adc_hz(self) -> f64 {
        2e6 * self.msps() as f64
    }

    /// R82x IF; LO = RF + IF.
    pub fn if_hz(self) -> f64 {
        self.adc_hz() / 4.0
    }

    /// One DMA block: the tick of the device's block counter.
    pub fn block_s(self) -> f64 {
        BLOCK_WORDS as f64 / self.adc_hz()
    }

    /// Widest step slice the passband allows (measured; the flatness correction removes the ripple).
    pub fn max_window_hz(self) -> f64 {
        match self {
            Rate::M10 => 9.3e6,
            Rate::M12 => 10.0e6,
        }
    }

    /// Step sizes offered in the UI; the last one is the widest that stays flat.
    pub fn steps(self) -> &'static [(f64, &'static str)] {
        match self {
            Rate::M10 => &[(4.0, "4"), (6.0, "6"), (8.0, "8"), (9.0, "9 MHz")],
            Rate::M12 => &[(4.0, "4"), (6.0, "6"), (8.0, "8"), (10.0, "10 MHz")],
        }
    }

    pub fn default_step_mhz(self) -> f64 {
        match self {
            Rate::M10 => 8.0,
            Rate::M12 => 10.0,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Order {
    Ascending,
    /// Firmware sweep4+ flag 0x01: steps grouped by tuner VCO divider, odd groups walked backwards,
    /// every pass reversed. Avoids ~10 ms per divider switch and the wrap.
    VcoContinuous,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    /// Requested display range.
    pub start_hz: f64,
    pub stop_hz: f64,
    /// Centre of step 0, centre spacing, number of steps (what the firmware gets).
    pub f0: u32,
    pub step_hz: u32,
    pub n_steps: u16,
    /// Width of each step's slice in the stitched spectrum (= step, or the span for a one-step plan).
    pub window_hz: f64,
    pub dwell: u8,
    pub skip: u8,
    pub lock_reads: u8,
    pub lock_timeout: u16,
    pub order: Order,
    pub rate: Rate,
}

impl Plan {
    #[allow(clippy::too_many_arguments)]
    pub fn new(start_mhz: f64, stop_mhz: f64, step_mhz: f64, dwell: u8, skip: u8, lock_reads: u8, order: Order, rate: Rate) -> Plan {
        let step = (step_mhz.clamp(1.0, rate.max_window_hz() / 1e6) * 1e6).round();
        let mut start = (start_mhz * 1e6).clamp(F_MIN_HZ, F_MAX_HZ - 0.2e6);
        let mut stop = (stop_mhz * 1e6).clamp(F_MIN_HZ + 0.2e6, F_MAX_HZ);
        if stop < start + 0.2e6 {
            stop = (start + 0.2e6).min(F_MAX_HZ);
            start = stop - 0.2e6;
        }
        let span = stop - start;
        let (n, window, mut f0) = if span <= step {
            (1usize, span, start + span / 2.0)
        } else {
            let n = (span / step - 1e-9).ceil() as usize;
            (n, step, start + step / 2.0)
        };
        // Keep every centre inside the tuner's range; the stitched cover may then start a little lower.
        let last = f0 + (n as f64 - 1.0) * step;
        if last > F_MAX_HZ {
            f0 -= last - F_MAX_HZ;
        }
        f0 = f0.max(F_MIN_HZ);
        Plan {
            start_hz: start,
            stop_hz: stop,
            f0: f0.round() as u32,
            step_hz: step as u32,
            n_steps: n.min(u16::MAX as usize) as u16,
            window_hz: window,
            dwell: dwell.max(1),
            skip,
            lock_reads,
            lock_timeout: 200,
            order,
            rate,
        }
    }

    pub fn n(&self) -> usize {
        self.n_steps as usize
    }

    pub fn centre(&self, step: usize) -> f64 {
        self.f0 as f64 + step as f64 * self.step_hz as f64
    }

    /// The tuner VCO divider the firmware uses for this step.
    pub fn divider(&self, step: usize) -> u8 {
        vco_divider(self.centre(step), self.rate.if_hz())
    }

    /// Frequency range the stitched steps cover.
    pub fn cover(&self) -> (f64, f64) {
        let lo = self.f0 as f64 - self.window_hz / 2.0;
        (lo, lo + self.n() as f64 * self.step_hz as f64 - (self.step_hz as f64 - self.window_hz))
    }

    pub fn flags(&self) -> u8 {
        match self.order {
            Order::Ascending => 0,
            Order::VcoContinuous => 0x01,
        }
    }

    /// The firmware's visiting order for one forward pass (the next pass walks it reversed in VCO order).
    pub fn sequence(&self) -> Vec<u16> {
        let n = self.n();
        let mut seq: Vec<u16> = (0..n as u16).collect();
        if self.order == Order::VcoContinuous {
            let mut runs: Vec<(u8, Vec<u16>)> = Vec::new();
            for i in 0..n {
                let d = self.divider(i);
                match runs.last_mut() {
                    Some((rd, idx)) if *rd == d => idx.push(i as u16),
                    _ => runs.push((d, vec![i as u16])),
                }
            }
            // The firmware falls back to ascending with more than 8 runs (cannot happen in 24–1800 MHz).
            if runs.len() <= 8 {
                seq = runs
                    .into_iter()
                    .enumerate()
                    .flat_map(|(r, (_, mut idx))| {
                        if r & 1 == 1 {
                            idx.reverse();
                        }
                        idx
                    })
                    .collect();
            }
        }
        seq
    }
}

/// Tuner VCO frequency for this RF (LO times the divider).
pub fn vco_hz(rf_hz: f64, if_hz: f64) -> f64 {
    (rf_hz + if_hz) * f64::from(1u32 << (vco_divider(rf_hz, if_hz) as u32 + 1))
}

/// A divider change costs ~5.6 ms per GHz the VCO has to move; above this it is a slow (~10 ms) crossing.
const SLOW_VCO_JUMP_HZ: f64 = 0.8e9;

impl Plan {
    /// Divider changes between consecutively visited steps in one pass, including the wrap in
    /// ascending order: (fast joins, slow crossings). See docs/measurements.md, "Visiting order".
    pub fn divider_transitions(&self) -> (usize, usize) {
        let seq = self.sequence();
        let mut pairs: Vec<(u16, u16)> = seq.windows(2).map(|w| (w[0], w[1])).collect();
        if self.order == Order::Ascending && seq.len() > 1 {
            pairs.push((seq[seq.len() - 1], seq[0]));
        }
        let (mut fast, mut slow) = (0, 0);
        let fi = self.rate.if_hz();
        for (a, b) in pairs {
            let (fa, fb) = (self.centre(a as usize), self.centre(b as usize));
            let jump = (vco_hz(fa, fi) - vco_hz(fb, fi)).abs();
            if jump > SLOW_VCO_JUMP_HZ {
                slow += 1;
            } else if vco_divider(fa, fi) != vco_divider(fb, fi) {
                fast += 1;
            }
        }
        (fast, slow)
    }
}

/// The divider index `r82x_set_pll` picks for this RF (6 = none), computed in uint32 like the firmware.
pub fn vco_divider(rf_hz: f64, if_hz: f64) -> u8 {
    let lo = (rf_hz + if_hz) as u64 as u32;
    for d in 0..6u32 {
        let vco = lo.wrapping_shl(d + 1) as u64;
        if (1_770_000_000..=3_900_000_000).contains(&vco) {
            return d as u8;
        }
    }
    6
}

/// RF frequencies (Hz) where the tuner switches VCO divider inside 24–1800 MHz (they move with the IF).
pub fn divider_switches(rate: Rate) -> Vec<f64> {
    let fi = rate.if_hz();
    let mut out = Vec::new();
    let mut prev = vco_divider(F_MIN_HZ, fi);
    let mut f = F_MIN_HZ;
    while f <= F_MAX_HZ {
        let d = vco_divider(f, fi);
        if d != prev {
            // refine to 10 kHz
            let (mut a, mut b) = (f - 0.25e6, f);
            while b - a > 1e4 {
                let m = (a + b) / 2.0;
                if vco_divider(m, fi) == prev { a = m } else { b = m }
            }
            out.push(b);
            prev = d;
        }
        f += 0.25e6;
    }
    out
}

pub struct Preset {
    pub name: &'static str,
    pub start_mhz: f64,
    pub stop_mhz: f64,
}

pub const PRESETS: &[Preset] = &[
    Preset { name: "Full range", start_mhz: 24.0, stop_mhz: 1800.0 },
    Preset { name: "VHF low", start_mhz: 24.0, stop_mhz: 88.0 },
    Preset { name: "FM broadcast", start_mhz: 87.5, stop_mhz: 108.0 },
    Preset { name: "Airband", start_mhz: 108.0, stop_mhz: 137.0 },
    Preset { name: "VHF high", start_mhz: 137.0, stop_mhz: 174.0 },
    Preset { name: "TV VHF", start_mhz: 174.0, stop_mhz: 216.0 },
    Preset { name: "UHF 400", start_mhz: 400.0, stop_mhz: 470.0 },
    Preset { name: "ISM 433", start_mhz: 430.0, stop_mhz: 440.0 },
    Preset { name: "TV UHF", start_mhz: 470.0, stop_mhz: 698.0 },
    Preset { name: "LTE 600/700", start_mhz: 614.0, stop_mhz: 806.0 },
    Preset { name: "Cell 850/900", start_mhz: 824.0, stop_mhz: 960.0 },
    Preset { name: "ISM 868/915", start_mhz: 860.0, stop_mhz: 930.0 },
    Preset { name: "ADS-B 1090", start_mhz: 1084.0, stop_mhz: 1096.0 },
    Preset { name: "L-band", start_mhz: 1500.0, stop_mhz: 1700.0 },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_range_matches_the_reference_plan() {
        let p = Plan::new(24.0, 1800.0, 8.0, 1, 0, 2, Order::VcoContinuous, Rate::M10);
        assert_eq!((p.f0, p.step_hz, p.n_steps), (28_000_000, 8_000_000, 222));
        assert_eq!(p.cover(), (24e6, 1800e6));
        let seq = p.sequence();
        let mut sorted = seq.clone();
        sorted.sort();
        assert_eq!(sorted, (0..222).collect::<Vec<_>>());
        assert_ne!(seq, sorted);
    }

    #[test]
    fn vco_order_removes_the_slow_crossings() {
        let asc = Plan::new(24.0, 1800.0, 8.0, 1, 0, 2, Order::Ascending, Rate::M10);
        let vco = Plan::new(24.0, 1800.0, 8.0, 1, 0, 2, Order::VcoContinuous, Rate::M10);
        assert_eq!(asc.divider_transitions(), (0, 6)); // 5 crossings + the wrap
        assert_eq!(vco.divider_transitions(), (5, 0));
        // a small plan across 105.6 MHz cannot avoid one
        let fm = Plan::new(87.5, 108.0, 8.0, 1, 0, 2, Order::VcoContinuous, Rate::M10);
        assert_eq!(fm.divider_transitions(), (0, 1));
    }

    #[test]
    fn narrow_plan_is_one_step() {
        let p = Plan::new(430.0, 436.0, 8.0, 1, 0, 2, Order::Ascending, Rate::M10);
        assert_eq!((p.n_steps, p.f0), (1, 433_000_000));
        assert_eq!(p.cover(), (430e6, 436e6));
    }

    #[test]
    fn centres_stay_in_range() {
        let p = Plan::new(24.0, 1800.0, 9.0, 1, 0, 2, Order::Ascending, Rate::M10);
        assert!(p.centre(p.n() - 1) <= F_MAX_HZ);
        assert!(p.cover().1 >= 1799e6);
    }

    #[test]
    fn switch_points_match_the_measured_ones() {
        let s: Vec<i64> = divider_switches(Rate::M10).iter().map(|f| (f / 1e6).round() as i64).collect();
        assert_eq!(s.len(), 5, "{s:?}");
        for (got, want) in s.iter().zip([48, 106, 216, 438, 880]) {
            assert!((got - want).abs() <= 6, "{s:?}");
        }
        // the switch points are fixed in LO, so a 1 MHz higher IF moves them 1 MHz down in RF
        let s12 = divider_switches(Rate::M12);
        for (a, b) in divider_switches(Rate::M10).iter().zip(&s12) {
            assert!((a - b - 1e6).abs() < 2e4, "{a} {b}");
        }
    }

    #[test]
    fn twelve_msps_plan_uses_wider_steps() {
        let p = Plan::new(24.0, 1800.0, Rate::M12.default_step_mhz(), 1, 0, 2, Order::VcoContinuous, Rate::M12);
        assert_eq!((p.f0, p.step_hz, p.n_steps), (29_000_000, 10_000_000, 178));
        assert_eq!(p.divider_transitions(), (5, 0));
        // 10 MSPS clamps the same request to its widest flat step
        assert_eq!(Plan::new(24.0, 1800.0, 10.0, 1, 0, 2, Order::VcoContinuous, Rate::M10).step_hz, 9_300_000);
        assert!((Rate::M12.block_s() - 341.333e-6).abs() < 1e-9);
    }
}
