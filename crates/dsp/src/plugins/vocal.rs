//! Vocal chain: FET Compressor, Opto Compressor and Pitch Correct.
//!
//! The two compressors model the behaviour of the classic circuit types, not any product: a fast
//! FET stage with a fixed threshold driven by input gain and a little output saturation, and a
//! slow optical cell whose release lengthens the longer it has been compressing. Pitch Correct
//! tracks the fundamental (YIN, with buffers sized in `prepare`) and pulls it toward the nearest
//! note of a key and scale through a short granular shifter.

use crate::biquad::{Biquad, FilterType};
use crate::params::{Params, choice, lin, log, param_plumbing};
use crate::plugins::dynamics::compress_gr;
use crate::util::{DelayLine, Smoother, flush, frames_in, ms_to_samples, sane_channels, sane_sr, time_coef};
use crate::{Category, ParamInfo, Plugin, PluginInfo, Unit, db_to_gain, gain_to_db};

#[inline]
fn ballistic(state: f32, target: f32, att: f32, rel: f32) -> f32 {
    let c = if target > state { att } else { rel };
    let v = target + (state - target) * c;
    if v.is_finite() { v } else { 0.0 }
}

/// The loudest finite sample of frame `n` across `ch` channels.
#[inline]
fn peak_at(io: &[Vec<f32>], ch: usize, n: usize) -> f32 {
    io.iter().take(ch).filter_map(|b| b.get(n)).filter(|v| v.is_finite()).fold(0.0f32, |m, v| m.max(v.abs()))
}

// ---------------------------------------------------------------------------------------------
// FET compressor

/// The FET stage's fixed threshold: Input gain decides how hard it is driven.
const FET_THRESHOLD_DB: f32 = -18.0;

static FET_PARAMS: [ParamInfo; 6] = [
    lin("input", "Input", 0.0, 48.0, 18.0, Unit::Db),
    lin("output", "Output", -24.0, 24.0, -6.0, Unit::Db),
    log("attack", "Attack", 0.02, 0.8, 0.2, Unit::Ms),
    log("release", "Release", 50.0, 1100.0, 400.0, Unit::Ms),
    choice("ratio", "Ratio", &["4:1", "8:1", "12:1", "20:1", "All"], 0),
    lin("mix", "Mix", 0.0, 100.0, 100.0, Unit::Percent),
];

pub static FET_INFO: PluginInfo = PluginInfo {
    id: "fet_compressor",
    name: "FET Compressor",
    short_name: "FET",
    category: Category::Dynamics,
    params: &FET_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// A fast, aggressive peak compressor: microsecond attacks, a release that slows when it has been
/// working hard, and an "All" mode that over-compresses with extra grit.
pub struct FetCompressor {
    p: Params,
    sr: f32,
    ch: usize,
    gr: f32,
    /// How hard it has been working lately (dB, slow average of the gain reduction).
    work: f32,
    att: f32,
    rel: f32,
    work_coef: f32,
    input: Smoother,
    output: Smoother,
    mix: Smoother,
    gr_meter: f32,
}

impl Default for FetCompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl FetCompressor {
    pub fn new() -> Self {
        let mut c = Self {
            p: Params::new(&FET_PARAMS),
            sr: 48_000.0,
            ch: 2,
            gr: 0.0,
            work: 0.0,
            att: 0.0,
            rel: 0.0,
            work_coef: 0.0,
            input: Smoother::new(1.0),
            output: Smoother::new(1.0),
            mix: Smoother::new(1.0),
            gr_meter: 0.0,
        };
        c.update();
        c.input.snap();
        c.output.snap();
        c.mix.snap();
        c
    }

    /// Ratio, knee and saturation for the ratio switch.
    fn character(&self) -> (f32, f32, f32) {
        match self.p.choice(4) {
            1 => (8.0, 4.0, 0.25),
            2 => (12.0, 3.0, 0.3),
            3 => (20.0, 2.0, 0.35),
            4 => (40.0, 10.0, 0.8),
            _ => (4.0, 6.0, 0.2),
        }
    }

    fn update(&mut self) {
        let sr = self.sr;
        self.att = time_coef(self.p.v(2), sr);
        self.rel = time_coef(self.p.v(3), sr);
        self.work_coef = time_coef(300.0, sr);
        self.input.set(db_to_gain(self.p.v(0)));
        self.output.set(db_to_gain(self.p.v(1)));
        self.mix.set(self.p.v(5) / 100.0);
    }
}

impl Plugin for FetCompressor {
    param_plumbing!(FET_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.ch = sane_channels(channels);
        for s in [&mut self.input, &mut self.output, &mut self.mix] {
            s.set_time(20.0, self.sr);
        }
        self.update();
        self.reset();
    }

    fn reset(&mut self) {
        self.gr = 0.0;
        self.work = 0.0;
        self.gr_meter = 0.0;
        self.input.snap();
        self.output.snap();
        self.mix.snap();
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.ch.min(io.len());
        let frames = frames_in(io, frames, ch);
        let (ratio, knee, sat) = self.character();
        let all = self.p.choice(4) == 4;
        let mut max_gr = 0.0f32;
        for n in 0..frames {
            let ing = self.input.next();
            let level = peak_at(io, ch, n) * ing;
            let target = compress_gr(gain_to_db(level), FET_THRESHOLD_DB, ratio, knee);
            // Program-dependent release: the harder it has been working, the slower it lets go.
            self.work = ballistic(self.work, self.gr, self.work_coef, self.work_coef);
            let slow = 1.0 + (self.work / 12.0).clamp(0.0, 1.5);
            let rel = self.rel.powf(1.0 / slow);
            self.gr = ballistic(self.gr, target, self.att, if all { rel.powf(1.3) } else { rel });
            max_gr = max_gr.max(self.gr);
            let g = db_to_gain(-self.gr);
            let (out, m) = (self.output.next(), self.mix.next());
            for b in io.iter_mut().take(ch) {
                let Some(x) = b.get_mut(n) else { continue };
                let dry = if x.is_finite() { *x } else { 0.0 };
                let v = dry * ing * g;
                // The output stage colours: odd harmonics that rise with drive.
                let shaped = (v * (1.0 + sat)).tanh() / (1.0 + sat).tanh();
                let wet = shaped * out;
                *x = dry * (1.0 - m) + wet * m;
            }
        }
        self.gr_meter = max_gr;
    }

    fn gain_reduction_db(&self) -> f32 {
        self.gr_meter
    }
}

// ---------------------------------------------------------------------------------------------
// Opto compressor

static OPTO_PARAMS: [ParamInfo; 5] = [
    lin("peak_reduction", "Peak Reduction", 0.0, 100.0, 40.0, Unit::Percent),
    lin("gain", "Gain", 0.0, 40.0, 6.0, Unit::Db),
    choice("mode", "Mode", &["Compress", "Limit"], 0),
    lin("emphasis", "Emphasis", 0.0, 100.0, 0.0, Unit::Percent),
    lin("mix", "Mix", 0.0, 100.0, 100.0, Unit::Percent),
];

pub static OPTO_INFO: PluginInfo = PluginInfo {
    id: "opto_compressor",
    name: "Opto Compressor",
    short_name: "Opto",
    category: Category::Dynamics,
    params: &OPTO_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// Optical-cell attack and the two release stages.
const OPTO_ATTACK_MS: f32 = 10.0;
const OPTO_FAST_RELEASE_MS: f32 = 60.0;
const OPTO_SLOW_RELEASE_MS: f32 = 400.0;
const OPTO_MEMORY_RELEASE_MS: f32 = 4000.0;

/// A smooth, program-dependent leveller: one Peak Reduction knob, a soft knee, a release that
/// recovers quickly at first and then slowly, more slowly the longer it has been compressing.
pub struct OptoCompressor {
    p: Params,
    sr: f32,
    ch: usize,
    emphasis: Vec<Biquad>,
    env: f32,
    gr: f32,
    /// 0..1: how long the cell has been lit lately (it "remembers" heavy compression).
    memory: f32,
    att: f32,
    env_rel: f32,
    gain: Smoother,
    mix: Smoother,
    gr_meter: f32,
}

impl Default for OptoCompressor {
    fn default() -> Self {
        Self::new()
    }
}

impl OptoCompressor {
    pub fn new() -> Self {
        let mut c = Self {
            p: Params::new(&OPTO_PARAMS),
            sr: 48_000.0,
            ch: 2,
            emphasis: Vec::new(),
            env: 0.0,
            gr: 0.0,
            memory: 0.0,
            att: 0.0,
            env_rel: 0.0,
            gain: Smoother::new(1.0),
            mix: Smoother::new(1.0),
            gr_meter: 0.0,
        };
        c.update();
        c.gain.snap();
        c.mix.snap();
        c
    }

    fn update(&mut self) {
        let sr = self.sr;
        self.att = time_coef(OPTO_ATTACK_MS, sr);
        self.env_rel = time_coef(30.0, sr);
        self.gain.set(db_to_gain(self.p.v(1)));
        self.mix.set(self.p.v(4) / 100.0);
        // Emphasis tilts the detector toward the highs, so bright sounds compress more.
        let boost = self.p.v(3) / 100.0 * 12.0;
        for b in &mut self.emphasis {
            b.set(FilterType::HighShelf, 2500.0, 0.7, boost, sr);
        }
    }
}

impl Plugin for OptoCompressor {
    param_plumbing!(OPTO_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.ch = sane_channels(channels);
        self.emphasis = vec![Biquad::default(); self.ch];
        self.gain.set_time(20.0, self.sr);
        self.mix.set_time(20.0, self.sr);
        self.update();
        self.reset();
    }

    fn reset(&mut self) {
        self.emphasis.iter_mut().for_each(Biquad::reset);
        self.env = 0.0;
        self.gr = 0.0;
        self.memory = 0.0;
        self.gr_meter = 0.0;
        self.gain.snap();
        self.mix.snap();
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.ch.min(io.len()).min(self.emphasis.len());
        let frames = frames_in(io, frames, ch);
        let sr = self.sr;
        let threshold = -self.p.v(0) * 0.4;
        let (ratio, knee) = if self.p.choice(2) == 1 { (12.0, 6.0) } else { (3.0, 12.0) };
        let mem_up = 1.0 - time_coef(2000.0, sr);
        let mem_down = 1.0 - time_coef(10_000.0, sr);
        let fast = time_coef(OPTO_FAST_RELEASE_MS, sr);
        let mut max_gr = 0.0f32;
        for n in 0..frames {
            let mut level = 0.0f32;
            for (c, f) in self.emphasis.iter_mut().enumerate().take(ch) {
                let x = io.get(c).and_then(|b| b.get(n)).copied().filter(|v| v.is_finite()).unwrap_or(0.0);
                level = level.max(f.process_sample(x).abs());
            }
            // An averaging detector (the cell responds to energy, not single peaks).
            self.env = ballistic(self.env, level, time_coef(5.0, sr), self.env_rel);
            let target = compress_gr(gain_to_db(self.env), threshold, ratio, knee);
            let lit = if self.gr > 1.0 { 1.0 } else { 0.0 };
            self.memory += (lit - self.memory) * if lit > self.memory { mem_up } else { mem_down };
            // Two-stage release: fast for the first part of the way back, then slow, slower still
            // after long, heavy compression.
            let rel = if self.gr - target > 3.0 { fast } else { time_coef(OPTO_SLOW_RELEASE_MS + OPTO_MEMORY_RELEASE_MS * self.memory, sr) };
            self.gr = ballistic(self.gr, target, self.att, rel);
            max_gr = max_gr.max(self.gr);
            let g = db_to_gain(-self.gr) * self.gain.next();
            let m = self.mix.next();
            for b in io.iter_mut().take(ch) {
                let Some(x) = b.get_mut(n) else { continue };
                let dry = if x.is_finite() { *x } else { 0.0 };
                *x = dry * (1.0 - m) + dry * g * m;
            }
        }
        self.gr_meter = max_gr;
    }

    fn gain_reduction_db(&self) -> f32 {
        self.gr_meter
    }
}

// ---------------------------------------------------------------------------------------------
// Pitch correction

const NOTE_NAMES: [&str; 12] = ["C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B"];
const SCALE_NAMES: [&str; 7] = ["Chromatic", "Major", "Minor", "Harmonic Minor", "Major Pentatonic", "Minor Pentatonic", "Blues"];
/// Each scale's notes as a bit mask over the twelve semitones above the key.
const SCALE_MASKS: [u16; 7] =
    [0b1111_1111_1111, 0b1010_1011_0101, 0b0101_1010_1101, 0b1001_1010_1101, 0b0010_1001_0101, 0b0100_1010_1001, 0b0100_1110_1001];

static TUNE_PARAMS: [ParamInfo; 5] = [
    choice("key", "Key", &NOTE_NAMES, 0),
    choice("scale", "Scale", &SCALE_NAMES, 0),
    lin("speed", "Retune Speed", 0.0, 400.0, 20.0, Unit::Ms),
    lin("amount", "Amount", 0.0, 100.0, 100.0, Unit::Percent),
    lin("mix", "Mix", 0.0, 100.0, 100.0, Unit::Percent),
];

pub static TUNE_INFO: PluginInfo = PluginInfo {
    id: "pitch_correct",
    name: "Pitch Correct",
    short_name: "PitchCor",
    category: Category::PitchShift,
    params: &TUNE_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// Tracking range: low male voice to a high soprano.
const MIN_HZ: f32 = 70.0;
const MAX_HZ: f32 = 1000.0;
/// The tracker runs at about this rate (the input is decimated to it).
const TRACK_SR: f32 = 24_000.0;
/// Analysis window and hop, in tracker samples.
const TRACK_WINDOW: usize = 1024;
const TRACK_HOP: usize = 128;
const YIN_THRESHOLD: f32 = 0.15;
/// The shifter's grain: short, as only small corrections are made.
const GRAIN_MS: f32 = 24.0;

/// The nearest note to `midi` that `mask` allows above `key` (0..11), as a fractional MIDI note.
fn snap(midi: f32, key: usize, mask: u16) -> f32 {
    if mask == 0 || !midi.is_finite() {
        return midi;
    }
    let base = midi.round() as i32;
    let mut best = midi;
    let mut best_d = f32::MAX;
    for off in -12..=12 {
        let n = base + off;
        let class = (n - key as i32).rem_euclid(12) as u32;
        if mask & (1u16 << class) == 0 {
            continue;
        }
        let d = (n as f32 - midi).abs();
        if d < best_d {
            best_d = d;
            best = n as f32;
        }
    }
    best
}

/// Pulls a monophonic voice toward the notes of a key and scale. Retune Speed is how quickly the
/// correction follows (0 = at once, the hard "robotic" sound); Amount scales it.
pub struct PitchCorrect {
    p: Params,
    sr: f32,
    ch: usize,
    /// Decimated mono input ring, and the tracker's work buffers (sized in prepare).
    ring: Vec<f32>,
    ring_pos: usize,
    frame: Vec<f32>,
    diff: Vec<f32>,
    decim: usize,
    decim_count: usize,
    decim_acc: f32,
    since_hop: usize,
    /// The correction to apply, in semitones, and its smoother.
    target: f32,
    correction: f32,
    lines: Vec<DelayLine>,
    window: f32,
    phase: f32,
    mix: Smoother,
}

impl Default for PitchCorrect {
    fn default() -> Self {
        Self::new()
    }
}

impl PitchCorrect {
    pub fn new() -> Self {
        let mut p = Self {
            p: Params::new(&TUNE_PARAMS),
            sr: 48_000.0,
            ch: 2,
            ring: Vec::new(),
            ring_pos: 0,
            frame: Vec::new(),
            diff: Vec::new(),
            decim: 2,
            decim_count: 0,
            decim_acc: 0.0,
            since_hop: 0,
            target: 0.0,
            correction: 0.0,
            lines: Vec::new(),
            window: 1152.0,
            phase: 0.0,
            mix: Smoother::new(1.0),
        };
        p.update();
        p.mix.snap();
        p
    }

    fn update(&mut self) {
        self.mix.set(self.p.v(4) / 100.0);
    }

    fn track_sr(&self) -> f32 {
        self.sr / self.decim as f32
    }

    /// YIN over the last window of the ring: the fundamental in Hz, or None when unvoiced.
    fn track(&mut self) -> Option<f32> {
        let len = self.ring.len();
        if len == 0 || self.frame.len() != len {
            return None;
        }
        for (i, f) in self.frame.iter_mut().enumerate() {
            *f = self.ring.get((self.ring_pos + i) % len).copied().unwrap_or(0.0);
        }
        let energy: f32 = self.frame.iter().map(|v| v * v).sum::<f32>() / len as f32;
        if energy < 1.0e-7 {
            return None;
        }
        let sr = self.track_sr();
        let max_tau = ((sr / MIN_HZ) as usize).min(len / 2).min(self.diff.len().saturating_sub(1));
        let min_tau = ((sr / MAX_HZ) as usize).max(2);
        if max_tau <= min_tau + 2 {
            return None;
        }
        let w = len - max_tau;
        let mut running = 0.0f32;
        if let Some(d0) = self.diff.first_mut() {
            *d0 = 1.0;
        }
        for tau in 1..=max_tau {
            let mut sum = 0.0f32;
            for (a, b) in self.frame.iter().take(w).zip(self.frame.iter().skip(tau)) {
                let d = a - b;
                sum += d * d;
            }
            running += sum;
            if let Some(slot) = self.diff.get_mut(tau) {
                *slot = if running > 0.0 { sum * tau as f32 / running } else { 1.0 };
            }
        }
        // The first dip under the threshold, refined to its local minimum.
        let mut tau = min_tau;
        while tau < max_tau {
            if self.diff.get(tau).copied().unwrap_or(1.0) < YIN_THRESHOLD {
                while tau + 1 < max_tau && self.diff.get(tau + 1).copied().unwrap_or(1.0) < self.diff.get(tau).copied().unwrap_or(1.0) {
                    tau += 1;
                }
                let (a, b, c) = (
                    self.diff.get(tau - 1).copied().unwrap_or(1.0),
                    self.diff.get(tau).copied().unwrap_or(1.0),
                    self.diff.get(tau + 1).copied().unwrap_or(1.0),
                );
                let den = a - 2.0 * b + c;
                let shift = if den.abs() > 1.0e-9 { (0.5 * (a - c) / den).clamp(-1.0, 1.0) } else { 0.0 };
                let hz = sr / (tau as f32 + shift);
                return (hz.is_finite() && (MIN_HZ..=MAX_HZ).contains(&hz)).then_some(hz);
            }
            tau += 1;
        }
        None
    }

    /// Runs the tracker and sets the new target correction.
    fn retarget(&mut self) {
        self.target = match self.track() {
            Some(hz) => {
                let midi = 69.0 + 12.0 * (hz / 440.0).log2();
                let key = self.p.choice(0);
                let mask = SCALE_MASKS.get(self.p.choice(1)).copied().unwrap_or(0x0fff);
                ((snap(midi, key, mask) - midi) * self.p.v(3) / 100.0).clamp(-6.0, 6.0)
            }
            // Unvoiced or silent: let go of any correction.
            None => 0.0,
        };
    }
}

impl Plugin for PitchCorrect {
    param_plumbing!(TUNE_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.ch = sane_channels(channels);
        self.decim = ((self.sr / TRACK_SR).round() as usize).clamp(1, 64);
        self.ring = vec![0.0; TRACK_WINDOW];
        self.frame = vec![0.0; TRACK_WINDOW];
        self.diff = vec![1.0; TRACK_WINDOW / 2 + 1];
        self.window = ms_to_samples(GRAIN_MS, self.sr).max(16.0);
        self.lines = (0..self.ch).map(|_| DelayLine::default()).collect();
        let len = self.window as usize + 8;
        self.lines.iter_mut().for_each(|l| l.allocate(len));
        self.mix.set_time(20.0, self.sr);
        self.update();
        self.reset();
    }

    fn reset(&mut self) {
        self.ring.iter_mut().for_each(|v| *v = 0.0);
        self.ring_pos = 0;
        self.decim_count = 0;
        self.decim_acc = 0.0;
        self.since_hop = 0;
        self.target = 0.0;
        self.correction = 0.0;
        self.lines.iter_mut().for_each(DelayLine::clear);
        self.phase = 0.0;
        self.mix.snap();
    }

    fn latency(&self) -> usize {
        (self.window * 0.5 + 1.0).round() as usize
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.ch.min(io.len()).min(self.lines.len());
        if ch == 0 {
            return;
        }
        let frames = frames_in(io, frames, ch);
        let speed = time_coef(self.p.v(2), self.sr);
        let w = self.window;
        let half = w * 0.5 + 2.0;
        for n in 0..frames {
            // Feed the tracker: a mono, averaged and decimated copy of the input.
            let mono = (0..ch).map(|c| io.get(c).and_then(|b| b.get(n)).copied().filter(|v| v.is_finite()).unwrap_or(0.0)).sum::<f32>() / ch as f32;
            self.decim_acc += mono;
            self.decim_count += 1;
            if self.decim_count >= self.decim {
                let len = self.ring.len().max(1);
                if let Some(slot) = self.ring.get_mut(self.ring_pos) {
                    *slot = self.decim_acc / self.decim as f32;
                }
                self.ring_pos = (self.ring_pos + 1) % len;
                self.decim_acc = 0.0;
                self.decim_count = 0;
                self.since_hop += 1;
                if self.since_hop >= TRACK_HOP {
                    self.since_hop = 0;
                    self.retarget();
                }
            }
            self.correction = flush(self.target + (self.correction - self.target) * speed);
            let ratio = 2f32.powf(self.correction / 12.0);
            let m = self.mix.next();
            self.phase += (1.0 - ratio) / w;
            self.phase -= self.phase.floor();
            if !self.phase.is_finite() {
                self.phase = 0.0;
            }
            let p2 = if self.phase >= 0.5 { self.phase - 0.5 } else { self.phase + 0.5 };
            let g1 = (std::f32::consts::PI * self.phase).sin().powi(2);
            let g2 = 1.0 - g1;
            let (d1, d2) = (self.phase * w + 2.0, p2 * w + 2.0);
            for c in 0..ch {
                let (Some(buf), Some(line)) = (io.get_mut(c), self.lines.get_mut(c)) else { continue };
                let Some(x) = buf.get_mut(n) else { continue };
                line.push(if x.is_finite() { *x } else { 0.0 });
                let wet = g1 * line.read(d1) + g2 * line.read(d2);
                let dry = line.read(half);
                *x = dry * (1.0 - m) + wet * m;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapping_follows_key_and_scale() {
        // Chromatic: the nearest semitone.
        assert_eq!(snap(69.3, 0, SCALE_MASKS[0]), 69.0);
        // C major has no A#: a note between A (69) and B (71) goes to the nearer one.
        assert_eq!(snap(69.8, 0, SCALE_MASKS[1]), 69.0);
        assert_eq!(snap(70.6, 0, SCALE_MASKS[1]), 71.0);
        // A minor (key 9) allows G (67) but not G# (68).
        assert_eq!(snap(67.8, 9, SCALE_MASKS[2]), 67.0);
        assert!(snap(f32::NAN, 0, SCALE_MASKS[1]).is_nan());
    }
}
