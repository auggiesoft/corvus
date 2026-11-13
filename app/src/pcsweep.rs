//! What the PC-driven IQ sweeps share (SoapySDR radios, RTL2832U dongles): which step is next, visiting the
//! steps back and forth as the sweep firmware does, and handing a step's samples to the engine as a tagged
//! block in the Pluto's two-word IQ layout.

use crate::hydra::{BLOCK_WORDS, Sink, TAG_WORDS, Tag, tag_block};
use crate::plan::{Order, Plan};

/// IQ samples in a block (the Pluto layout: two words each between the tags).
pub const BLOCK_IQ: usize = (BLOCK_WORDS - 2 * TAG_WORDS) / 2;
/// Full scale in the block's words, as the Pluto's 12-bit samples.
const FULL_SCALE: f32 = 2048.0;

/// Where a sweep is: the step within the pass, its direction, and the pass number the tags carry.
#[derive(Default)]
pub struct Pass {
    pos: usize,
    back: bool,
    pub sweep: u32,
}

impl Pass {
    /// A new pass from the first step (what the engine expects after a change).
    pub fn restart(&mut self) {
        self.pos = 0;
        self.back = false;
        self.sweep = self.sweep.wrapping_add(1);
    }

    /// The step to visit now.
    pub fn step(&self, seq: &[u16]) -> u16 {
        seq[if self.back { seq.len() - 1 - self.pos } else { self.pos }]
    }

    /// On to the next step; at the end of a pass, the next pass (back and forth: it starts where this one ended).
    pub fn advance(&mut self, plan: &Plan, n: usize) {
        self.pos += 1;
        if self.pos >= n {
            self.pos = 0;
            self.sweep = self.sweep.wrapping_add(1);
            if plan.order == Order::VcoContinuous && n > 1 {
                self.back = !self.back;
            }
        }
    }
}

/// One block to the engine: `iq` holds `BLOCK_IQ` interleaved I/Q pairs at full scale 1.0.
pub fn push_block(sink: &Sink, iq: &[f32], step: u16, block: u16, sweep: u32, freq_hz: u64, dma_count: u32) {
    let mut w = vec![0u16; BLOCK_WORDS];
    for (o, v) in w[TAG_WORDS..BLOCK_WORDS - TAG_WORDS].iter_mut().zip(iq) {
        *o = (v * FULL_SCALE).round().clamp(-32768.0, 32767.0) as i16 as u16;
    }
    tag_block(&mut w, &Tag { step, block, sweep, freq_hz: freq_hz as u32, dma_count, lock_polls: 0, lock_timed_out: false });
    sink.push(w);
}
