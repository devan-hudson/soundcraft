//! Uniformly partitioned overlap-save convolution, sample in / sample out, one block of latency.
//!
//! Everything is allocated in [`Convolver::new`]; [`Convolver::set_ir`] and
//! [`Convolver::process`] only write into those buffers, so both are safe on the audio thread.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::util::flush;

const ZERO: Complex<f32> = Complex { re: 0.0, im: 0.0 };

/// Shared FFT plans for convolvers of one block size (planning allocates; share it).
#[derive(Clone)]
pub(crate) struct Plans {
    block: usize,
    fwd: Arc<dyn Fft<f32>>,
    inv: Arc<dyn Fft<f32>>,
}

impl Plans {
    pub(crate) fn new(block: usize) -> Self {
        let block = block.clamp(16, 1 << 16);
        let mut planner = FftPlanner::<f32>::new();
        Self { block, fwd: planner.plan_fft_forward(block * 2), inv: planner.plan_fft_inverse(block * 2) }
    }
}

/// One channel's convolution with an impulse response of up to `max_ir` samples.
pub(crate) struct Convolver {
    plans: Plans,
    scratch: Vec<Complex<f32>>,
    work: Vec<Complex<f32>>,
    acc: Vec<Complex<f32>>,
    /// The IR's spectra by partition, and the input spectra history (a ring).
    ir: Vec<Vec<Complex<f32>>>,
    history: Vec<Vec<Complex<f32>>>,
    /// The last two input blocks; the current block's output.
    input: Vec<f32>,
    output: Vec<f32>,
    parts: usize,
    head: usize,
    fill: usize,
}

impl Convolver {
    pub(crate) fn new(plans: &Plans, max_ir: usize) -> Self {
        let b = plans.block;
        let n = b * 2;
        let max_parts = max_ir.div_ceil(b).max(1);
        let scratch = plans.fwd.get_inplace_scratch_len().max(plans.inv.get_inplace_scratch_len());
        Self {
            plans: plans.clone(),
            scratch: vec![ZERO; scratch],
            work: vec![ZERO; n],
            acc: vec![ZERO; n],
            ir: vec![vec![ZERO; n]; max_parts],
            history: vec![vec![ZERO; n]; max_parts],
            input: vec![0.0; n],
            output: vec![0.0; b],
            parts: 1,
            head: 0,
            fill: 0,
        }
    }

    /// The latency: one block.
    pub(crate) fn latency(&self) -> usize {
        self.plans.block
    }

    /// The longest IR this convolver takes, in samples.
    pub(crate) fn max_ir(&self) -> usize {
        self.ir.len() * self.plans.block
    }

    /// Loads an impulse response (longer ones are cut to [`Convolver::max_ir`]).
    pub(crate) fn set_ir(&mut self, ir: &[f32]) {
        let b = self.plans.block;
        let used = ir.len().min(self.max_ir()).div_ceil(b).clamp(1, self.ir.len().max(1));
        for (k, spec) in self.ir.iter_mut().enumerate() {
            if k >= used {
                spec.iter_mut().for_each(|c| *c = ZERO);
                continue;
            }
            for (j, c) in spec.iter_mut().enumerate() {
                let v = if j < b { ir.get(k * b + j).copied().filter(|v| v.is_finite()).unwrap_or(0.0) } else { 0.0 };
                *c = Complex::new(v, 0.0);
            }
            self.plans.fwd.process_with_scratch(spec, &mut self.scratch);
        }
        self.parts = used;
    }

    /// Forgets all input (the IR stays).
    pub(crate) fn clear(&mut self) {
        self.history.iter_mut().for_each(|h| h.iter_mut().for_each(|c| *c = ZERO));
        self.input.iter_mut().for_each(|v| *v = 0.0);
        self.output.iter_mut().for_each(|v| *v = 0.0);
        self.head = 0;
        self.fill = 0;
    }

    /// Takes one input sample, returns the output from one block earlier.
    #[inline]
    pub(crate) fn process(&mut self, x: f32) -> f32 {
        let b = self.plans.block;
        if let Some(slot) = self.input.get_mut(b + self.fill) {
            *slot = if x.is_finite() { x } else { 0.0 };
        }
        let y = self.output.get(self.fill).copied().unwrap_or(0.0);
        self.fill += 1;
        if self.fill >= b {
            self.fill = 0;
            self.convolve();
        }
        y
    }

    fn convolve(&mut self) {
        let b = self.plans.block;
        let ring = self.history.len();
        if ring == 0 {
            return;
        }
        for (w, x) in self.work.iter_mut().zip(self.input.iter()) {
            *w = Complex::new(*x, 0.0);
        }
        self.plans.fwd.process_with_scratch(&mut self.work, &mut self.scratch);
        if let Some(slot) = self.history.get_mut(self.head) {
            slot.copy_from_slice(&self.work);
        }
        self.acc.iter_mut().for_each(|c| *c = ZERO);
        for k in 0..self.parts.min(ring) {
            let idx = (self.head + ring - k) % ring;
            let (Some(x), Some(h)) = (self.history.get(idx), self.ir.get(k)) else { continue };
            for ((a, xv), hv) in self.acc.iter_mut().zip(x.iter()).zip(h.iter()) {
                *a += xv * hv;
            }
        }
        self.plans.inv.process_with_scratch(&mut self.acc, &mut self.scratch);
        let norm = 1.0 / (2 * b) as f32;
        for (o, c) in self.output.iter_mut().zip(self.acc.iter().skip(b)) {
            *o = flush(c.re * norm);
        }
        // The block just used becomes the older half of the next window.
        self.input.copy_within(b.., 0);
        self.head = (self.head + 1) % ring;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_direct_convolution_one_block_late() {
        let plans = Plans::new(64);
        let ir: Vec<f32> = (0..300).map(|i| ((i * 37 % 17) as f32 - 8.0) / 8.0 * (-(i as f32) / 80.0).exp()).collect();
        let mut c = Convolver::new(&plans, 512);
        c.set_ir(&ir);
        let x: Vec<f32> = (0..1000).map(|i| ((i * 13 % 7) as f32 - 3.0) / 3.0).collect();
        let y: Vec<f32> = x.iter().map(|v| c.process(*v)).collect();
        for n in 0..(1000 - 64) {
            let direct: f32 = ir.iter().enumerate().filter(|(k, _)| *k <= n).map(|(k, h)| h * x[n - k]).sum();
            assert!((y[n + 64] - direct).abs() < 1e-3, "sample {n}: {} vs {direct}", y[n + 64]);
        }
    }

    #[test]
    fn hostile_input_and_oversized_irs_are_contained() {
        let plans = Plans::new(32);
        let mut c = Convolver::new(&plans, 64);
        c.set_ir(&[f32::NAN, 1.0, f32::INFINITY]);
        c.set_ir(&vec![0.5; 10_000]);
        for _ in 0..200 {
            assert!(c.process(f32::NAN).is_finite());
        }
        c.clear();
        assert_eq!(c.process(1.0), 0.0);
    }
}
