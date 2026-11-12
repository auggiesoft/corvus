//! Sample health, from the raw blocks: headroom and clipping on every board, and on the Pluto the image
//! check, each strong signal against its mirror about the LO.
//!
//! Clipping counts samples at the ADC's rails. The AD936x compresses in its analog stages before its
//! samples reach full scale (on a Pluto+ at 71 dB on the FM band the peak stayed at −4.7 dBFS while the last
//! 6 dB of gain added 4), so a clean clip count there doesn't rule out overload; the peak level says more.
//!
//! The image check: I/Q gain or phase imbalance leaves a copy of a signal at +f at −f, coherent with it.
//! For x = a·s + b·conj(s), E[X(f)·X(−f)] = a·b·E|S(f)|², while two independent signals, or the two ADCs'
//! own noise, average towards nothing. So per step, over its visits, P(k) = Σ|X(k)|² and C(k) = Σ X(k)·X(−k)
//! on a 1024-point FFT. The image rejection is ΣP / Σ|C| over the strong bins whose mirror is quiet, and a
//! visible mirror line that is coherent with the line opposite is flagged: an image, or a spur on one I/Q
//! path, not a signal of its own. (A noise-based estimate, E[I²] against E[Q²], reads the two ADCs'
//! unequal noise as imbalance: 1.4–1.9 dB on a quiet band at 40 dB gain, so it isn't used.)

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::hydra::{BLOCK_WORDS, TAG_WORDS};
use crate::plan::{Plan, Rate};

pub const HFFT: usize = 1024;
/// Per-visit decay of the image sums: about 256 visits' memory (7 s at 37 passes/s).
const DECAY: f32 = 1.0 - 1.0 / 256.0;
/// A strong bin: 30 dB over the step's median bin.
const STRONG: f32 = 1e3;
/// Its mirror counts as quiet 20 dB below it.
const QUIET: f32 = 1e2;
/// A mirror line worth flagging: 10 dB over the median.
const VISIBLE: f32 = 10.0;
/// ...and coherent with the line opposite (squared coherence).
const COHERENT: f32 = 0.5;
/// Visits before a step's image sums are used.
const MIN_VISITS: u32 = 16;
/// FFT segments per Pluto block (4088 samples, nearly disjoint).
const SEGS: usize = 4;
/// Mirror lines listed at most.
const MAX_MIRRORS: usize = 8;

/// A line that mirrors a stronger one about its step's LO.
#[derive(Clone, Debug, PartialEq)]
pub struct Mirror {
    pub hz: f64,
    pub of_hz: f64,
    /// Its level against the stronger line's (dB, ≤ 0).
    pub below_db: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Summary {
    /// Steps measured (each one's latest visit).
    pub steps: usize,
    /// Highest sample against full scale, and the centre of its step.
    pub peak_dbfs: f32,
    pub peak_hz: f64,
    /// Steps whose latest visit had samples at full scale, their centres (up to 8), and the share of
    /// sample values at full scale over all steps.
    pub clip_steps: usize,
    pub clip_hz: Vec<f64>,
    pub clip_frac: f64,
    /// The image check applies (IQ samples centred on the LO: the Pluto).
    pub iq: bool,
    /// Image rejection over the strong signals (dB); `image_floor`: the check's own floor, so at least this.
    pub image_db: Option<f32>,
    pub image_floor: bool,
    pub image_signals: usize,
    /// The step with the least, and its centre.
    pub image_worst: Option<(f32, f64)>,
    pub mirrors: Vec<Mirror>,
}

impl Summary {
    /// One line for the console.
    pub fn line(&self) -> String {
        if self.steps == 0 {
            return "health: no samples yet".into();
        }
        let mut s = format!("health: peak {:.1} dBFS at {:.1} MHz", self.peak_dbfs, self.peak_hz / 1e6);
        if self.clip_steps > 0 {
            let at: Vec<String> = self.clip_hz.iter().map(|f| format!("{:.0}", f / 1e6)).collect();
            s += &format!(", clipping on {} ({:.4}% of samples; {} MHz)", plural(self.clip_steps, "step"), self.clip_frac * 100.0, at.join(", "));
        } else {
            s += ", no clipping";
        }
        if self.iq {
            match self.image_db {
                Some(db) => {
                    s += &format!("; image rejection {}{db:.0} dB on {} strong bins", if self.image_floor { "≥ " } else { "" }, self.image_signals);
                    if let Some((w, f)) = self.image_worst {
                        s += &format!(" (least {w:.0} dB, step at {:.1} MHz)", f / 1e6);
                    }
                }
                None => s += "; image rejection: no strong signals yet",
            }
            for m in &self.mirrors {
                s += &format!("; mirror line {:.2} MHz ({:.1} dB under {:.2} MHz)", m.hz / 1e6, m.below_db, m.of_hz / 1e6);
            }
        }
        s
    }
}

/// "1 step", "3 steps".
pub fn plural(n: usize, what: &str) -> String {
    if n == 1 { format!("1 {what}") } else { format!("{n} {what}s") }
}

/// Step centres as a short MHz list: "90, 130, 170 MHz", with "…" past `max`.
pub fn mhz_list(hz: &[f64], total: usize, max: usize) -> String {
    let v: Vec<String> = hz.iter().take(max).map(|f| format!("{:.0}", f / 1e6)).collect();
    format!("{}{} MHz", v.join(", "), if total > max { ", …" } else { "" })
}

#[derive(Clone)]
struct Step {
    /// Latest visit: peak |sample| (counts), values at full scale, values seen.
    peak: u32,
    clip: u32,
    n: u32,
    visits: u32,
    p: Vec<f32>,
    c: Vec<Complex32>,
}

pub struct Health {
    rate: Rate,
    f0: f64,
    step_hz: f64,
    /// Half the shown slice: mirror lines are listed when they fall in it.
    show_hz: f64,
    steps: Vec<Step>,
    fft: Arc<dyn Fft<f32>>,
    win: Vec<f32>,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    xi: Vec<Complex32>,
}

impl Health {
    pub fn new(plan: &Plan) -> Health {
        let rate = plan.rate;
        let iq = rate.iq16();
        let fft = FftPlanner::new().plan_fft_forward(HFFT);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        let step = Step { peak: 0, clip: 0, n: 0, visits: 0, p: if iq { vec![0.0; HFFT] } else { Vec::new() }, c: if iq { vec![Complex32::default(); HFFT] } else { Vec::new() } };
        Health {
            rate,
            f0: plan.f0 as f64,
            step_hz: plan.step_hz as f64,
            show_hz: plan.measure_hz() / 2.0,
            steps: vec![step; plan.n()],
            fft,
            win: (0..HFFT).map(|i| (0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / (HFFT - 1) as f64).cos()) as f32).collect(),
            buf: vec![Complex32::default(); HFFT],
            scratch,
            xi: Vec::new(),
        }
    }

    fn full_scale(&self) -> u32 {
        if self.rate.iq16() || !self.rate.complex() { 2048 } else { 128 }
    }

    /// One block of a step (`first`: the visit's first block, which also feeds the image sums).
    pub fn block(&mut self, step: usize, words: &[u16], first: bool) {
        let rate = self.rate;
        let Some(s) = self.steps.get_mut(step) else { return };
        if first {
            s.peak = 0;
            s.clip = 0;
            s.n = 0;
        }
        let pay = &words[TAG_WORDS..BLOCK_WORDS - TAG_WORDS];
        let (mut peak, mut clip) = (s.peak, s.clip);
        if rate.iq16() {
            // 12-bit, sign-extended: the rails are -2048 and 2047
            for &w in pay {
                let v = (w as i16).unsigned_abs() as u32;
                peak = peak.max(v);
                clip += (v >= 2047) as u32;
            }
        } else if rate.complex() {
            // HackRF: 8-bit I and Q in one word
            for &w in pay {
                for b in [(w & 0xFF) as u8 as i8, (w >> 8) as u8 as i8] {
                    let v = b.unsigned_abs() as u32;
                    peak = peak.max(v);
                    clip += (v >= 127) as u32;
                }
            }
        } else {
            // R82x boards: 12-bit offset binary
            for &w in pay {
                let x = (w & 0x0FFF) as i32;
                peak = peak.max((x - 2048).unsigned_abs());
                clip += (x == 0 || x == 4095) as u32;
            }
        }
        s.peak = peak;
        s.clip = clip;
        s.n += if rate.complex() && !rate.iq16() { 2 * pay.len() } else { pay.len() } as u32;
        if first && rate.iq16() {
            self.image_sums(step, pay);
        }
    }

    fn image_sums(&mut self, step: usize, pay: &[u16]) {
        let n = pay.len() / 2;
        self.xi.clear();
        self.xi.extend(pay.as_chunks::<2>().0.iter().map(|w| Complex32::new(w[0] as i16 as f32, w[1] as i16 as f32)));
        let mean = self.xi.iter().sum::<Complex32>() / n as f32;
        let s = &mut self.steps[step];
        s.visits += 1;
        s.p.iter_mut().for_each(|p| *p *= DECAY);
        s.c.iter_mut().for_each(|c| *c *= DECAY);
        for seg in 0..SEGS {
            let at = seg * (n - HFFT) / (SEGS - 1);
            for (b, (x, w)) in self.buf.iter_mut().zip(self.xi[at..at + HFFT].iter().zip(&self.win)) {
                *b = (x - mean) * w;
            }
            self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
            for k in 0..HFFT {
                let x = self.buf[k];
                s.p[k] += x.norm_sqr();
                s.c[k] += x * self.buf[(HFFT - k) % HFFT];
            }
        }
    }

    pub fn summary(&self) -> Summary {
        let fs = self.full_scale() as f32;
        let mut out = Summary { iq: self.rate.iq16(), peak_dbfs: f32::NEG_INFINITY, ..Default::default() };
        let (mut clip, mut total) = (0u64, 0u64);
        for (i, s) in self.steps.iter().enumerate().filter(|(_, s)| s.n > 0) {
            out.steps += 1;
            let db = 20.0 * (s.peak.max(1) as f32 / fs).log10();
            if db > out.peak_dbfs {
                out.peak_dbfs = db;
                out.peak_hz = self.centre(i);
            }
            if s.clip > 0 {
                out.clip_steps += 1;
                if out.clip_hz.len() < 8 {
                    out.clip_hz.push(self.centre(i));
                }
            }
            clip += s.clip as u64;
            total += s.n as u64;
        }
        out.clip_frac = if total > 0 { clip as f64 / total as f64 } else { 0.0 };
        if !out.iq {
            return out;
        }
        let df = self.rate.adc_hz() / HFFT as f64;
        let half = self.rate.max_window_hz() / 2.0;
        let guard = (300e3f64).max(2.0 * df);
        let off = |k: usize| if k < HFFT / 2 { k as f64 } else { k as f64 - HFFT as f64 } * df;
        let usable: Vec<usize> = (0..HFFT).filter(|&k| (guard..=half).contains(&off(k).abs())).collect();
        let (mut sum_p, mut sum_c, mut sum_floor) = (0f64, 0f64, 0f64);
        let mut mirrors: Vec<Mirror> = Vec::new();
        let mut med = Vec::with_capacity(usable.len());
        for (i, s) in self.steps.iter().enumerate().filter(|(_, s)| s.visits >= MIN_VISITS) {
            med.clear();
            med.extend(usable.iter().map(|&k| s.p[k]));
            let m = med.len() / 2;
            let median = *med.select_nth_unstable_by(m, |a, b| a.total_cmp(b)).1;
            if median <= 0.0 {
                continue;
            }
            // averages behind the sums: the decayed visits, times the segments
            let neff = (s.visits.min(2 * 256) as usize * SEGS) as f64;
            let (mut sp, mut sc) = (0f64, 0f64);
            for &k in &usable {
                let mk = (HFFT - k) % HFFT;
                let (p, pm, c) = (s.p[k], s.p[mk], s.c[k].norm());
                if p >= STRONG * median && p >= QUIET * pm {
                    out.image_signals += 1;
                    sp += p as f64;
                    sc += c as f64;
                    sum_floor += ((p as f64) * (pm as f64) / neff).sqrt();
                }
                // each pair once, from its stronger side; the weaker line is the mirror
                if p >= pm && p < QUIET * pm && pm >= VISIBLE * median && c * c >= COHERENT * p * pm && off(mk).abs() <= self.show_hz {
                    let lo = self.centre(i);
                    mirrors.push(Mirror { hz: lo + off(mk), of_hz: lo + off(k), below_db: 10.0 * (pm / p).log10() });
                }
            }
            if sp > 0.0 {
                let db = (20.0 * (sp / sc.max(1e-30)).log10()) as f32;
                if out.image_worst.is_none_or(|(w, _)| db < w) {
                    out.image_worst = Some((db, self.centre(i)));
                }
                sum_p += sp;
                sum_c += sc;
            }
        }
        if sum_p > 0.0 {
            let db = (20.0 * (sum_p / sum_c.max(1e-30)).log10()) as f32;
            let floor = (20.0 * (sum_p / sum_floor.max(1e-30)).log10()) as f32;
            // within 3 dB of what uncorrelated mirrors alone would give: the check can't see further
            out.image_floor = db > floor - 3.0;
            out.image_db = Some(if out.image_floor { db.min(floor) } else { db });
        }
        // one entry per pair: neighbouring bins (window leakage), both sides of an equal pair, and
        // overlapping steps all see the same one
        mirrors.sort_by(|a, b| b.below_db.total_cmp(&a.below_db));
        let near = |a: f64, b: f64| (a - b).abs() < 5.0 * df;
        for m in mirrors {
            let seen = out.mirrors.iter().any(|o| (near(o.hz, m.hz) && near(o.of_hz, m.of_hz)) || (near(o.hz, m.of_hz) && near(o.of_hz, m.hz)));
            if out.mirrors.len() < MAX_MIRRORS && !seen {
                out.mirrors.push(m);
            }
        }
        out
    }

    fn centre(&self, step: usize) -> f64 {
        self.f0 + step as f64 * self.step_hz
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::Order;

    /// A Pluto block: tones at (offset Hz, amplitude), with I/Q gain `g` (Q scaled) and phase error `phi`
    /// (rad), plus a little noise.
    fn pluto_block(tones: &[(f64, f64)], g: f64, phi: f64, seed: &mut u64, t0: usize) -> Vec<u16> {
        let mut w = vec![0u16; BLOCK_WORDS];
        let n = (BLOCK_WORDS - 2 * TAG_WORDS) / 2;
        for i in 0..n {
            let t = (t0 + i) as f64 / 61.44e6;
            let (mut re, mut im) = (0.0, 0.0);
            for &(f, a) in tones {
                let ph = std::f64::consts::TAU * f * t;
                re += a * ph.cos();
                im += a * g * (ph + phi).sin();
            }
            // xorshift noise, about ±2 counts
            for v in [&mut re, &mut im] {
                *seed ^= *seed << 13;
                *seed ^= *seed >> 7;
                *seed ^= *seed << 17;
                *v += (*seed % 5) as f64 - 2.0;
            }
            w[TAG_WORDS + 2 * i] = (re.round().clamp(-2048.0, 2047.0) as i16) as u16;
            w[TAG_WORDS + 2 * i + 1] = (im.round().clamp(-2048.0, 2047.0) as i16) as u16;
        }
        w
    }

    fn plan() -> Plan {
        Plan::new(980.0, 1020.0, 40.0, 1, 0, 0, Order::VcoContinuous, Rate::P61)
    }

    #[test]
    fn image_rejection_matches_the_imbalance() {
        let p = plan();
        let mut h = Health::new(&p);
        let mut seed = 1u64;
        // 0.1 dB and 0.5°: IRR = 10 log10((1 + 2g cos φ + g²) / (1 - 2g cos φ + g²)) ≈ 45.9 dB
        let (g, phi) = (10f64.powf(0.1 / 20.0), 0.5f64.to_radians());
        let want = 10.0 * ((1.0 + 2.0 * g * phi.cos() + g * g) / (1.0 - 2.0 * g * phi.cos() + g * g)).log10();
        for v in 0..200 {
            let b = pluto_block(&[(7.3e6, 600.0), (-13.1e6, 300.0)], g, phi, &mut seed, v * 5000);
            h.block(0, &b, true);
        }
        let s = h.summary();
        let got = s.image_db.unwrap();
        assert!(!s.image_floor, "{s:?}");
        assert!((got as f64 - want).abs() < 1.5, "image {got:.1} dB, want {want:.1}");
        assert!(s.mirrors.is_empty(), "{:?}", s.mirrors);
    }

    #[test]
    fn a_balanced_receiver_reads_at_least_the_floor() {
        let p = plan();
        let mut h = Health::new(&p);
        let mut seed = 7u64;
        for v in 0..200 {
            h.block(0, &pluto_block(&[(7.3e6, 600.0)], 1.0, 0.0, &mut seed, v * 5000), true);
        }
        let s = h.summary();
        assert!(s.image_floor && s.image_db.unwrap() > 50.0, "{s:?}");
    }

    #[test]
    fn a_spur_on_one_path_is_flagged_as_a_mirror_line() {
        let p = plan();
        let mut h = Health::new(&p);
        let mut seed = 3u64;
        let n = (BLOCK_WORDS - 2 * TAG_WORDS) / 2;
        for v in 0..100 {
            let mut b = pluto_block(&[(5.0e6, 400.0)], 1.0, 0.0, &mut seed, v * 5000);
            // a real-valued line on I only at 20 MHz: equal halves at ±20 MHz, fully coherent
            for i in 0..n {
                let t = (v * 5000 + i) as f64 / 61.44e6;
                let x = b[TAG_WORDS + 2 * i] as i16 as f64 + 200.0 * (std::f64::consts::TAU * 20e6 * t).cos();
                b[TAG_WORDS + 2 * i] = (x.round() as i16) as u16;
            }
            h.block(0, &b, true);
        }
        let s = h.summary();
        assert_eq!(s.mirrors.len(), 1, "{:?}", s.mirrors);
        let m = &s.mirrors[0];
        assert!((m.hz - m.of_hz).abs() > 39e6 && (m.hz + m.of_hz - 2e9).abs() < 0.2e6, "{m:?}");
        // the real tone at +5 MHz has a quiet mirror and no image
        assert!(s.image_floor, "{s:?}");
    }

    #[test]
    fn clipping_is_counted_at_the_rails() {
        let p = plan();
        let mut h = Health::new(&p);
        let mut seed = 5u64;
        h.block(0, &pluto_block(&[(3e6, 1000.0)], 1.0, 0.0, &mut seed, 0), true);
        let s = h.summary();
        assert_eq!(s.clip_steps, 0);
        assert!((s.peak_dbfs + 6.2).abs() < 0.3, "{}", s.peak_dbfs);
        h.block(0, &pluto_block(&[(3e6, 2600.0)], 1.0, 0.0, &mut seed, 0), true);
        let s = h.summary();
        assert_eq!(s.clip_steps, 1);
        assert!(s.clip_frac > 0.1 && s.peak_dbfs > -0.01, "{s:?}");
    }
}
