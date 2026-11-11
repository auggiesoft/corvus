//! One raw sweep block -> power spectrum (dBFS-scaled, linear) on the RF offset grid.
//!
//! R82x boards: the block is real-valued at the ADC rate (12, 20 or 24 MSPS) with the IF at a quarter of
//! it, and the spectrum is inverted: RF offset = -(f_if - IF). HackRF: each word is one 8-bit IQ sample
//! (I in the low byte) at 20 MSPS around the tune's LO; a step is the slice 5 MHz below the LO (side 0)
//! or 5 MHz above it (side 1). Samples are words 8..8184 either way. Pluto: each sample is two words, I then
//! Q (12-bit, sign-extended), 4088 of them, at 61.44 or 30.72 MSPS around the tune's LO; the few bins
//! around DC (the residual LO leakage) are bridged from their neighbours.

use std::sync::Arc;

use rustfft::num_complex::Complex32;
use rustfft::{Fft, FftPlanner};

use crate::hydra::{BLOCK_WORDS, TAG_WORDS};
use crate::plan::Rate;

pub const N_SAMPLES: usize = BLOCK_WORDS - 2 * TAG_WORDS;

/// Samples in a block's payload at this rate.
pub fn n_samples(rate: Rate) -> usize {
    if rate.iq16() { (BLOCK_WORDS - 2 * TAG_WORDS) / 2 } else { N_SAMPLES }
}

/// Pluto: bins this close to DC are bridged (the residual LO leakage after the AD936x's DC tracking).
fn dc_notch_hz(nfft: usize, rate: Rate) -> f64 {
    1.6 * bin_hz(nfft, rate)
}
/// FFT sizes offered as resolution choices. Below 8192, Welch-averaged (50 % overlap) over the block.
pub const NFFTS: [usize; 5] = [8192, 4096, 2048, 1024, 512];
/// Largest |offset| kept on the grid: a little beyond the widest usable step.
fn grid_half_hz(rate: Rate) -> f64 {
    rate.max_window_hz() / 2.0 + 0.25e6
}

/// HackRF slices are centred this far either side of the tune's LO (`hackrf_sweep`'s 7.5 MHz offset).
pub const HACKRF_SLICE_HZ: f64 = 5e6;

pub fn bin_hz(nfft: usize, rate: Rate) -> f64 {
    rate.adc_hz() / nfft as f64
}

/// (RF offset Hz, FFT bin) of the grid, in ascending offset. Shared by the worker and the display.
pub fn grid(nfft: usize, rate: Rate) -> Vec<(f64, usize)> {
    let df = bin_hz(nfft, rate);
    let half = grid_half_hz(rate);
    if rate.complex() {
        // HackRF side 0: the step's centre is 5 MHz below the LO, so offset = bin frequency + 5 MHz.
        // Pluto: the step is centred on the LO.
        let shift = if rate.sliced() { HACKRF_SLICE_HZ } else { 0.0 };
        let mut g: Vec<(f64, usize)> = (0..nfft)
            .map(|k| {
                let f = if k < nfft / 2 { k as f64 } else { k as f64 - nfft as f64 } * df;
                (f + shift, k)
            })
            .filter(|(off, _)| off.abs() <= half)
            .collect();
        g.sort_by(|a, b| a.0.total_cmp(&b.0));
        return g;
    }
    let fi = rate.if_hz();
    let mut g: Vec<(f64, usize)> =
        (0..=nfft / 2).map(|k| (fi - k as f64 * df, k)).filter(|(off, _)| off.abs() <= half).collect();
    g.sort_by(|a, b| a.0.total_cmp(&b.0));
    g
}

/// Grid range a step of this width uses: offsets in [-w/2, w/2).
pub fn window(grid_off: &[f64], width_hz: f64) -> std::ops::Range<usize> {
    let h = width_hz / 2.0;
    grid_off.partition_point(|&o| o < -h)..grid_off.partition_point(|&o| o < h)
}

pub struct Dsp {
    pub nfft: usize,
    pub rate: Rate,
    /// Samples per block payload (8176, or 4088 for the Pluto's two-word IQ).
    n: usize,
    /// Pluto: grid indices whose bins sit on DC (bridged from their neighbours).
    dc: std::ops::Range<usize>,
    fft: Arc<dyn Fft<f32>>,
    win: Vec<f32>,
    seg_len: usize,
    hop: usize,
    n_seg: usize,
    scale: f32,
    buf: Vec<Complex32>,
    scratch: Vec<Complex32>,
    x: Vec<f32>,
    /// FFT bin index for each grid point, in ascending RF offset (HackRF: the slice below the LO).
    grid_k: Vec<usize>,
    /// HackRF: the same grid for the slice above the LO (10 MHz on, half the FFT away).
    grid_k_hi: Vec<usize>,
    xi: Vec<Complex32>,
    /// RF offset (Hz) of each grid point.
    #[cfg_attr(not(test), allow(dead_code))]
    pub grid_off: Vec<f64>,
}

impl Dsp {
    pub fn new(nfft: usize, rate: Rate) -> Dsp {
        let n = n_samples(rate);
        let seg_len = nfft.min(n);
        let hop = (seg_len / 2).max(1);
        let n_seg = if seg_len == n { 1 } else { (n - seg_len) / hop + 1 };
        let win: Vec<f32> = (0..seg_len)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / (seg_len - 1) as f64;
                (0.5 - 0.5 * a.cos()) as f32
            })
            .collect();
        let sum: f64 = win.iter().map(|&w| w as f64).sum();
        // A full-scale real tone (amplitude 2048 counts) reads 0 dBFS in its bin; a full-scale complex
        // tone (amplitude 128) likewise.
        let scale = if rate.iq16() {
            ((1.0 / (sum * 2048.0)).powi(2) / n_seg as f64) as f32
        } else if rate.complex() {
            ((1.0 / (sum * 128.0)).powi(2) / n_seg as f64) as f32
        } else {
            ((2.0 / (sum * 2048.0)).powi(2) / n_seg as f64) as f32
        };
        let fft = FftPlanner::new().plan_fft_forward(nfft);
        let scratch = vec![Complex32::default(); fft.get_inplace_scratch_len()];
        let grid = grid(nfft, rate);
        let dc = if rate.iq16() {
            let h = dc_notch_hz(nfft, rate);
            let lo = grid.partition_point(|g| g.0 < -h);
            lo..grid.partition_point(|g| g.0 <= h)
        } else {
            0..0
        };
        Dsp {
            nfft,
            rate,
            n,
            dc,
            fft,
            win,
            seg_len,
            hop,
            n_seg,
            scale,
            buf: vec![Complex32::default(); nfft],
            scratch,
            x: if rate.complex() { Vec::new() } else { vec![0.0; n] },
            xi: if rate.complex() { vec![Complex32::default(); n] } else { Vec::new() },
            grid_k: grid.iter().map(|g| g.1).collect(),
            grid_k_hi: grid.iter().map(|g| (g.1 + nfft / 2) % nfft).collect(),
            grid_off: grid.iter().map(|g| g.0).collect(),
        }
    }

    pub fn grid_len(&self) -> usize {
        self.grid_k.len()
    }

    /// The contiguous grid range a step of this width uses: offsets in [-w/2, w/2).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn window(&self, width_hz: f64) -> std::ops::Range<usize> {
        window(&self.grid_off, width_hz)
    }

    /// Power (linear, dBFS scale) of one block on grid[range] into `out`. `side` picks the HackRF slice
    /// (0 below the LO, 1 above); the R82x boards ignore it.
    pub fn power(&mut self, words: &[u16], range: std::ops::Range<usize>, out: &mut [f32], side: u8) {
        if self.rate.iq16() {
            return self.power_iq16(words, range, out);
        }
        if self.rate.complex() {
            return self.power_iq(words, range, out, side);
        }
        let s = &words[TAG_WORDS..BLOCK_WORDS - TAG_WORDS];
        let mut mean = 0.0f32;
        for (x, &w) in self.x.iter_mut().zip(s) {
            *x = (w & 0x0FFF) as f32;
            mean += *x;
        }
        mean /= self.n as f32;
        out.iter_mut().for_each(|o| *o = 0.0);
        for seg in 0..self.n_seg {
            let x = &self.x[seg * self.hop..seg * self.hop + self.seg_len];
            for (i, b) in self.buf.iter_mut().enumerate() {
                *b = if i < self.seg_len { Complex32::new((x[i] - mean) * self.win[i], 0.0) } else { Complex32::default() };
            }
            self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
            for (o, &k) in out.iter_mut().zip(&self.grid_k[range.clone()]) {
                *o += self.buf[k].norm_sqr();
            }
        }
        out.iter_mut().for_each(|o| *o *= self.scale);
    }

    fn power_iq(&mut self, words: &[u16], range: std::ops::Range<usize>, out: &mut [f32], side: u8) {
        let s = &words[TAG_WORDS..BLOCK_WORDS - TAG_WORDS];
        let mut mean = Complex32::default();
        for (x, &w) in self.xi.iter_mut().zip(s) {
            *x = Complex32::new((w & 0xFF) as u8 as i8 as f32, (w >> 8) as u8 as i8 as f32);
            mean += *x;
        }
        mean /= self.n as f32;
        let ks = if side == 0 { &self.grid_k[range.clone()] } else { &self.grid_k_hi[range.clone()] };
        out.iter_mut().for_each(|o| *o = 0.0);
        for seg in 0..self.n_seg {
            let x = &self.xi[seg * self.hop..seg * self.hop + self.seg_len];
            for (i, b) in self.buf.iter_mut().enumerate() {
                *b = if i < self.seg_len { (x[i] - mean) * self.win[i] } else { Complex32::default() };
            }
            self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
            for (o, &k) in out.iter_mut().zip(ks) {
                *o += self.buf[k].norm_sqr();
            }
        }
        out.iter_mut().for_each(|o| *o *= self.scale);
    }

    /// The Pluto's blocks: I, Q word pairs (12-bit, sign-extended), centred on the LO, DC bridged.
    fn power_iq16(&mut self, words: &[u16], range: std::ops::Range<usize>, out: &mut [f32]) {
        let s = &words[TAG_WORDS..BLOCK_WORDS - TAG_WORDS];
        let mut mean = Complex32::default();
        for (x, w) in self.xi.iter_mut().zip(s.as_chunks::<2>().0) {
            *x = Complex32::new(w[0] as i16 as f32, w[1] as i16 as f32);
            mean += *x;
        }
        mean /= self.n as f32;
        out.iter_mut().for_each(|o| *o = 0.0);
        for seg in 0..self.n_seg {
            let x = &self.xi[seg * self.hop..seg * self.hop + self.seg_len];
            for (i, b) in self.buf.iter_mut().enumerate() {
                *b = if i < self.seg_len { (x[i] - mean) * self.win[i] } else { Complex32::default() };
            }
            self.fft.process_with_scratch(&mut self.buf, &mut self.scratch);
            for (o, &k) in out.iter_mut().zip(&self.grid_k[range.clone()]) {
                *o += self.buf[k].norm_sqr();
            }
        }
        out.iter_mut().for_each(|o| *o *= self.scale);
        // bridge the DC bins that fall inside this range, from the nearest bins outside the notch
        let (a, b) = (self.dc.start.max(range.start), self.dc.end.min(range.end));
        if a < b {
            let left = (self.dc.start > range.start).then(|| out[self.dc.start - 1 - range.start]);
            let right = (self.dc.end < range.end).then(|| out[self.dc.end - range.start]);
            let v = match (left, right) {
                (Some(l), Some(r)) => 0.5 * (l + r),
                (Some(x), None) | (None, Some(x)) => x,
                (None, None) => return,
            };
            out[a - range.start..b - range.start].iter_mut().for_each(|o| *o = v);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone_block(rf_off: f64, amp: f64, rate: Rate) -> Vec<u16> {
        let f_if = rate.if_hz() - rf_off;
        (0..BLOCK_WORDS)
            .map(|n| (2048.0 + amp * (std::f64::consts::TAU * f_if / rate.adc_hz() * n as f64).cos()).round() as u16)
            .collect()
    }

    #[test]
    fn tone_lands_on_its_rf_offset_at_the_right_level() {
        for rate in [Rate::M10, Rate::M12] {
            for nfft in NFFTS {
                let mut d = Dsp::new(nfft, rate);
                let r = d.window(rate.max_window_hz());
                let mut p = vec![0.0; r.len()];
                for off in [1_234_567.0, -0.47 * rate.max_window_hz()] {
                    d.power(&tone_block(off, 1024.0, rate), r.clone(), &mut p, 0);
                    let (i, &max) = p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
                    let got = d.grid_off[r.start + i];
                    assert!((got - off).abs() <= bin_hz(nfft, rate), "{rate:?} nfft {nfft}: {got} vs {off}");
                    let db = 10.0 * max.log10();
                    // -6 dBFS tone; Hann scalloping can cost up to 1.4 dB
                    assert!((-7.6..=-5.8).contains(&db), "{rate:?} nfft {nfft}: {db} dBFS");
                }
            }
        }
    }

    /// An IQ block (I low byte, Q high byte) holding a complex tone `lo_off` Hz from the LO.
    fn iq_tone_block(lo_off: f64, amp: f64) -> Vec<u16> {
        (0..BLOCK_WORDS)
            .map(|n| {
                let ph = std::f64::consts::TAU * lo_off / 20e6 * n as f64;
                let (i, q) = ((amp * ph.cos()).round() as i8, (amp * ph.sin()).round() as i8);
                (i as u8 as u16) | ((q as u8 as u16) << 8)
            })
            .collect()
    }

    #[test]
    fn hackrf_tone_lands_in_the_right_slice_at_the_right_level() {
        let rate = Rate::H20;
        for nfft in NFFTS {
            let mut d = Dsp::new(nfft, rate);
            let r = d.window(rate.max_window_hz());
            let mut p = vec![0.0; r.len()];
            // (offset from the LO, side, expected offset from that slice's centre)
            for (lo_off, side, want) in [(-6.2e6, 0u8, -1.2e6), (-3.0e6, 0, 2.0e6), (6.9e6, 1, 1.9e6), (2.6e6, 1, -2.4e6)] {
                d.power(&iq_tone_block(lo_off, 64.0), r.clone(), &mut p, side);
                let (i, &max) = p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
                let got = d.grid_off[r.start + i];
                assert!((got - want).abs() <= bin_hz(nfft, rate), "nfft {nfft} side {side}: {got} vs {want}");
                let db = 10.0 * max.log10();
                // -6 dBFS tone (64 of 128); Hann scalloping up to 1.4 dB, plus 8-bit rounding
                assert!((-7.8..=-5.5).contains(&db), "nfft {nfft}: {db} dBFS");
            }
        }
    }

    /// A Pluto block (I, Q word pairs) holding a complex tone `lo_off` Hz from the LO, plus a DC offset.
    fn pluto_tone_block(lo_off: f64, amp: f64, rate: Rate, dc: f64) -> Vec<u16> {
        let mut w = vec![0u16; BLOCK_WORDS];
        for n in 0..BLOCK_WORDS / 2 {
            let ph = std::f64::consts::TAU * lo_off / rate.adc_hz() * n as f64;
            w[2 * n] = (dc + amp * ph.cos()).round() as i16 as u16;
            w[2 * n + 1] = (dc + amp * ph.sin()).round() as i16 as u16;
        }
        w
    }

    #[test]
    fn pluto_tone_lands_on_its_offset_at_the_right_level() {
        for rate in [Rate::P61, Rate::P31, Rate::P15, Rate::P7, Rate::P3] {
            for nfft in NFFTS {
                let mut d = Dsp::new(nfft, rate);
                let r = d.window(rate.max_window_hz());
                let mut p = vec![0.0; r.len()];
                for off in [0.1530864 * rate.max_window_hz(), -0.46 * rate.max_window_hz(), 0.31 * rate.max_window_hz()] {
                    d.power(&pluto_tone_block(off, 1024.0, rate, 40.0), r.clone(), &mut p, 0);
                    let (i, &max) = p.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap();
                    let got = d.grid_off[r.start + i];
                    assert!((got - off).abs() <= bin_hz(nfft, rate), "{rate:?} nfft {nfft}: {got} vs {off}");
                    let db = 10.0 * max.log10();
                    assert!((-7.6..=-5.8).contains(&db), "{rate:?} nfft {nfft}: {db} dBFS");
                }
            }
        }
    }

    #[test]
    fn pluto_dc_is_bridged() {
        let rate = Rate::P61;
        for nfft in NFFTS {
            let mut d = Dsp::new(nfft, rate);
            let r = d.window(rate.max_window_hz());
            let mut p = vec![0.0; r.len()];
            // LO leakage just off DC (the mean doesn't remove it)
            d.power(&pluto_tone_block(1.5e3, 200.0, rate, 0.0), r.clone(), &mut p, 0);
            let off = &d.grid_off[r];
            let h = dc_notch_hz(nfft, rate);
            let inside: Vec<f32> = p.iter().zip(off).filter(|(_, o)| o.abs() <= h).map(|(v, _)| *v).collect();
            assert!(!inside.is_empty(), "nfft {nfft}: no DC bins");
            let first = off.iter().position(|o| o.abs() <= h).unwrap();
            let (l, r) = (p[first - 1], p[first + inside.len()]);
            for v in &inside {
                assert!((v - 0.5 * (l + r)).abs() <= 1e-6 * (l + r), "nfft {nfft}: DC bin {v} not bridged from {l}, {r}");
            }
        }
    }

    #[test]
    fn window_is_centred() {
        for (rate, w) in [(Rate::M10, 8e6), (Rate::M12, 10e6)] {
            let d = Dsp::new(8192, rate);
            let r = d.window(w);
            let h = w / 2.0;
            assert!(d.grid_off[r.start] >= -h && d.grid_off[r.start] < -h + bin_hz(8192, rate));
            assert!(d.grid_off[r.end - 1] < h);
        }
    }
}
