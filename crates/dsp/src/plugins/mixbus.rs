//! Mix bus and mastering: Multiband Compressor, Tape Saturation, Stereo Imager and Linear-Phase EQ.

use crate::biquad::{Biquad, Coeffs, FilterType};
use crate::convolver::{Convolver, Plans};
use crate::params::{Params, choice, lin, log, param_plumbing, toggle};
use crate::plugins::dynamics::compress_gr;
use crate::util::{DcBlocker, Smoother, frames_in, sane_channels, sane_sr, time_coef};
use crate::{Category, ParamInfo, Plugin, PluginInfo, Unit, db_to_gain, gain_to_db};

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

const BUTTERWORTH_Q: f32 = std::f32::consts::FRAC_1_SQRT_2;

#[inline]
fn ballistic(state: f32, target: f32, att: f32, rel: f32) -> f32 {
    let c = if target > state { att } else { rel };
    let v = target + (state - target) * c;
    if v.is_finite() { v } else { 0.0 }
}

/// A 4th-order Linkwitz-Riley low- or high-pass: two cascaded Butterworth sections.
#[derive(Clone, Copy, Default)]
struct Lr4 {
    a: Biquad,
    b: Biquad,
}

impl Lr4 {
    fn set(&mut self, kind: FilterType, freq: f32, sr: f32) {
        self.a.set(kind, freq, BUTTERWORTH_Q, 0.0, sr);
        self.b.set(kind, freq, BUTTERWORTH_Q, 0.0, sr);
    }

    fn reset(&mut self) {
        self.a.reset();
        self.b.reset();
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        self.b.process_sample(self.a.process_sample(x))
    }
}

// ---------------------------------------------------------------------------------------------
// Multiband compressor

static MB_PARAMS: [ParamInfo; 14] = [
    log("low_x", "X-Over Low", 40.0, 1000.0, 200.0, Unit::Hz),
    log("high_x", "X-Over High", 1000.0, 12000.0, 2500.0, Unit::Hz),
    lin("low_thresh", "Low Thresh", -60.0, 0.0, -20.0, Unit::Db),
    log("low_ratio", "Low Ratio", 1.0, 20.0, 2.0, Unit::Ratio),
    lin("low_gain", "Low Gain", -12.0, 12.0, 0.0, Unit::Db),
    lin("mid_thresh", "Mid Thresh", -60.0, 0.0, -20.0, Unit::Db),
    log("mid_ratio", "Mid Ratio", 1.0, 20.0, 2.0, Unit::Ratio),
    lin("mid_gain", "Mid Gain", -12.0, 12.0, 0.0, Unit::Db),
    lin("high_thresh", "High Thresh", -60.0, 0.0, -20.0, Unit::Db),
    log("high_ratio", "High Ratio", 1.0, 20.0, 2.0, Unit::Ratio),
    lin("high_gain", "High Gain", -12.0, 12.0, 0.0, Unit::Db),
    log("attack", "Attack", 0.1, 100.0, 10.0, Unit::Ms),
    log("release", "Release", 10.0, 1000.0, 120.0, Unit::Ms),
    lin("output", "Output", -12.0, 12.0, 0.0, Unit::Db),
];

pub static MB_INFO: PluginInfo = PluginInfo {
    id: "multiband_compressor",
    name: "Multiband Compressor",
    short_name: "MultiBnd",
    category: Category::Dynamics,
    params: &MB_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// One channel's crossover: low/rest split, mid/high split, and the all-pass that keeps the low
/// band in phase with the other two (the second crossover's low- plus high-pass).
#[derive(Clone, Copy, Default)]
struct Split {
    lp1: Lr4,
    hp1: Lr4,
    lp2: Lr4,
    hp2: Lr4,
    ap_lp: Lr4,
    ap_hp: Lr4,
}

impl Split {
    fn set(&mut self, low: f32, high: f32, sr: f32) {
        self.lp1.set(FilterType::LowPass, low, sr);
        self.hp1.set(FilterType::HighPass, low, sr);
        self.lp2.set(FilterType::LowPass, high, sr);
        self.hp2.set(FilterType::HighPass, high, sr);
        self.ap_lp.set(FilterType::LowPass, high, sr);
        self.ap_hp.set(FilterType::HighPass, high, sr);
    }

    fn reset(&mut self) {
        for f in [&mut self.lp1, &mut self.hp1, &mut self.lp2, &mut self.hp2, &mut self.ap_lp, &mut self.ap_hp] {
            f.reset();
        }
    }

    /// Low, mid and high bands of `x`; they sum back to an all-pass copy of it.
    #[inline]
    fn bands(&mut self, x: f32) -> [f32; 3] {
        let low = self.lp1.process(x);
        let rest = self.hp1.process(x);
        let low = self.ap_lp.process(low) + self.ap_hp.process(low);
        [low, self.lp2.process(rest), self.hp2.process(rest)]
    }
}

/// Three-band compressor with Linkwitz-Riley crossovers, stereo-linked per band.
pub struct Multiband {
    p: Params,
    sr: f32,
    ch: usize,
    splits: Vec<Split>,
    bands: Vec<[f32; 3]>,
    gr: [f32; 3],
    att: f32,
    rel: f32,
    gains: [Smoother; 3],
    output: Smoother,
    gr_meter: f32,
}

impl Default for Multiband {
    fn default() -> Self {
        Self::new()
    }
}

impl Multiband {
    pub fn new() -> Self {
        let mut m = Self {
            p: Params::new(&MB_PARAMS),
            sr: 48_000.0,
            ch: 2,
            splits: Vec::new(),
            bands: Vec::new(),
            gr: [0.0; 3],
            att: 0.0,
            rel: 0.0,
            gains: [Smoother::new(1.0); 3],
            output: Smoother::new(1.0),
            gr_meter: 0.0,
        };
        m.update();
        m.gains.iter_mut().for_each(Smoother::snap);
        m.output.snap();
        m
    }

    fn update(&mut self) {
        let sr = self.sr;
        self.att = time_coef(self.p.v(11), sr);
        self.rel = time_coef(self.p.v(12), sr);
        for (b, g) in self.gains.iter_mut().enumerate() {
            g.set(db_to_gain(self.p.v(4 + b * 3)));
        }
        self.output.set(db_to_gain(self.p.v(13)));
        // The high crossover stays above the low one.
        let low = self.p.v(0).min(sr * 0.2);
        let high = self.p.v(1).max(low * 1.5).min(sr * 0.45);
        for s in &mut self.splits {
            s.set(low, high, sr);
        }
    }
}

impl Plugin for Multiband {
    param_plumbing!(MB_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.ch = sane_channels(channels);
        self.splits = vec![Split::default(); self.ch];
        self.bands = vec![[0.0; 3]; self.ch];
        self.gains.iter_mut().for_each(|g| g.set_time(20.0, self.sr));
        self.output.set_time(20.0, self.sr);
        self.update();
        self.reset();
    }

    fn reset(&mut self) {
        self.splits.iter_mut().for_each(Split::reset);
        self.gr = [0.0; 3];
        self.gr_meter = 0.0;
        self.gains.iter_mut().for_each(Smoother::snap);
        self.output.snap();
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.ch.min(io.len()).min(self.splits.len());
        let frames = frames_in(io, frames, ch);
        let mut max_gr = 0.0f32;
        for n in 0..frames {
            let mut level = [0.0f32; 3];
            for c in 0..ch {
                let x = io.get(c).and_then(|b| b.get(n)).copied().filter(|v| v.is_finite()).unwrap_or(0.0);
                let (Some(split), Some(slot)) = (self.splits.get_mut(c), self.bands.get_mut(c)) else { continue };
                *slot = split.bands(x);
                for (l, v) in level.iter_mut().zip(slot.iter()) {
                    *l = l.max(v.abs());
                }
            }
            let mut g = [1.0f32; 3];
            for b in 0..3 {
                let target = compress_gr(gain_to_db(level[b]), self.p.v(2 + b * 3), self.p.v(3 + b * 3), 6.0);
                self.gr[b] = ballistic(self.gr[b], target, self.att, self.rel);
                max_gr = max_gr.max(self.gr[b]);
                g[b] = db_to_gain(-self.gr[b]) * self.gains[b].next();
            }
            let out = self.output.next();
            for c in 0..ch {
                let Some(bands) = self.bands.get(c) else { continue };
                let y = (bands[0] * g[0] + bands[1] * g[1] + bands[2] * g[2]) * out;
                if let Some(x) = io.get_mut(c).and_then(|b| b.get_mut(n)) {
                    *x = y;
                }
            }
        }
        self.gr_meter = max_gr;
    }

    fn gain_reduction_db(&self) -> f32 {
        self.gr_meter
    }
}

// ---------------------------------------------------------------------------------------------
// Tape saturation

static TAPE_PARAMS: [ParamInfo; 5] = [
    lin("drive", "Drive", 0.0, 24.0, 6.0, Unit::Db),
    lin("bias", "Bias", 0.0, 100.0, 30.0, Unit::Percent),
    choice("speed", "Speed", &["7.5 ips", "15 ips", "30 ips"], 1),
    lin("output", "Output", -12.0, 12.0, 0.0, Unit::Db),
    lin("mix", "Mix", 0.0, 100.0, 100.0, Unit::Percent),
];

pub static TAPE_INFO: PluginInfo = PluginInfo {
    id: "tape_saturation",
    name: "Tape Saturation",
    short_name: "TapeSat",
    category: Category::Harmonic,
    params: &TAPE_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// Head bump (Hz, dB) and top-end roll-off (Hz) for each tape speed.
const SPEEDS: [(f32, f32, f32); 3] = [(50.0, 2.5, 10_000.0), (70.0, 2.0, 16_000.0), (100.0, 1.5, 20_000.0)];

/// Per-channel filters of the tape path.
#[derive(Clone, Copy)]
struct TapeChannel {
    emphasis: Biquad,
    de_emphasis: Biquad,
    bump: Biquad,
    top: Biquad,
    dc: DcBlocker,
}

/// Recording to tape and back: pre-emphasis, a soft, slightly asymmetric magnetic curve (Bias adds
/// even harmonics), de-emphasis, the low-end head bump and the top-end loss of the chosen speed.
/// Level is compensated at a typical mix level (about -10 dBFS), so Drive changes the colour more
/// than the loudness there; louder material is squeezed, as tape does.
pub struct TapeSaturation {
    p: Params,
    sr: f32,
    ch: Vec<TapeChannel>,
    drive: Smoother,
    output: Smoother,
    mix: Smoother,
}

impl Default for TapeSaturation {
    fn default() -> Self {
        Self::new()
    }
}

impl TapeSaturation {
    pub fn new() -> Self {
        let mut t = Self {
            p: Params::new(&TAPE_PARAMS),
            sr: 48_000.0,
            ch: Vec::new(),
            drive: Smoother::new(2.0),
            output: Smoother::new(1.0),
            mix: Smoother::new(1.0),
        };
        t.update();
        t.drive.snap();
        t.output.snap();
        t.mix.snap();
        t
    }

    fn update(&mut self) {
        let sr = self.sr;
        self.drive.set(db_to_gain(self.p.v(0)));
        self.output.set(db_to_gain(self.p.v(3)));
        self.mix.set(self.p.v(4) / 100.0);
        let (bump_hz, bump_db, top_hz) = SPEEDS.get(self.p.choice(2)).copied().unwrap_or(SPEEDS[1]);
        for c in &mut self.ch {
            c.emphasis.set(FilterType::HighShelf, 3000.0, 0.7, 6.0, sr);
            c.de_emphasis.set(FilterType::HighShelf, 3000.0, 0.7, -6.0, sr);
            c.bump.set(FilterType::Peak, bump_hz, 1.0, bump_db, sr);
            c.top.set(FilterType::LowPass, top_hz.min(sr * 0.45), BUTTERWORTH_Q, 0.0, sr);
        }
    }
}

impl Plugin for TapeSaturation {
    param_plumbing!(TAPE_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        let c = TapeChannel {
            emphasis: Biquad::default(),
            de_emphasis: Biquad::default(),
            bump: Biquad::default(),
            top: Biquad::default(),
            dc: DcBlocker::new(10.0, self.sr),
        };
        self.ch = vec![c; sane_channels(channels)];
        for s in [&mut self.drive, &mut self.output, &mut self.mix] {
            s.set_time(20.0, self.sr);
        }
        self.update();
        self.reset();
    }

    fn reset(&mut self) {
        for c in &mut self.ch {
            for b in [&mut c.emphasis, &mut c.de_emphasis, &mut c.bump, &mut c.top] {
                b.reset();
            }
            c.dc.reset();
        }
        self.drive.snap();
        self.output.snap();
        self.mix.snap();
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.ch.len().min(io.len());
        let frames = frames_in(io, frames, ch);
        let bias = self.p.v(1) / 100.0 * 0.35;
        let offset = bias.tanh();
        const REFERENCE: f32 = 0.3;
        for n in 0..frames {
            let (drive, out, m) = (self.drive.next(), self.output.next(), self.mix.next());
            // Unity gain for a signal at the reference level, whatever the drive.
            let makeup = REFERENCE / (REFERENCE * drive).tanh().max(1.0e-6);
            for (c, tc) in self.ch.iter_mut().enumerate().take(ch) {
                let Some(x) = io.get_mut(c).and_then(|b| b.get_mut(n)) else { continue };
                let dry = if x.is_finite() { *x } else { 0.0 };
                let v = tc.emphasis.process_sample(dry) * drive;
                let v = (v + bias).tanh() - offset;
                let v = tc.dc.process(tc.de_emphasis.process_sample(v) * makeup);
                let wet = tc.top.process_sample(tc.bump.process_sample(v)) * out;
                *x = dry * (1.0 - m) + wet * m;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Stereo imager

static IMAGER_PARAMS: [ParamInfo; 3] = [
    lin("width", "Width", 0.0, 200.0, 100.0, Unit::Percent),
    log("mono_below", "Mono Below", 20.0, 500.0, 20.0, Unit::Hz),
    lin("output", "Output", -12.0, 12.0, 0.0, Unit::Db),
];

pub static IMAGER_INFO: PluginInfo = PluginInfo {
    id: "stereo_imager",
    name: "Stereo Imager",
    short_name: "Imager",
    category: Category::Other,
    params: &IMAGER_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// Mono Below at its lowest is off.
const MONO_OFF_HZ: f32 = 20.5;

/// Mid/side width from mono (0 %) through untouched (100 %) to extra wide (200 %), and an
/// optional mono low end below a frequency (bass stays centred on any system).
pub struct StereoImager {
    p: Params,
    sr: f32,
    ch: usize,
    side_high: Lr4,
    width: Smoother,
    output: Smoother,
}

impl Default for StereoImager {
    fn default() -> Self {
        Self::new()
    }
}

impl StereoImager {
    pub fn new() -> Self {
        let mut s = Self {
            p: Params::new(&IMAGER_PARAMS),
            sr: 48_000.0,
            ch: 2,
            side_high: Lr4::default(),
            width: Smoother::new(1.0),
            output: Smoother::new(1.0),
        };
        s.update();
        s.width.snap();
        s.output.snap();
        s
    }

    fn update(&mut self) {
        self.width.set(self.p.v(0) / 100.0);
        self.output.set(db_to_gain(self.p.v(2)));
        self.side_high.set(FilterType::HighPass, self.p.v(1).min(self.sr * 0.45), self.sr);
    }
}

impl Plugin for StereoImager {
    param_plumbing!(IMAGER_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.ch = sane_channels(channels);
        self.width.set_time(30.0, self.sr);
        self.output.set_time(20.0, self.sr);
        self.update();
        self.reset();
    }

    fn reset(&mut self) {
        self.side_high.reset();
        self.width.snap();
        self.output.snap();
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.ch.min(io.len());
        let frames = frames_in(io, frames, ch);
        let mono_low = self.p.v(1) > MONO_OFF_HZ;
        for n in 0..frames {
            let (w, out) = (self.width.next(), self.output.next());
            if ch < 2 {
                if let Some(x) = io.first_mut().and_then(|b| b.get_mut(n)) {
                    *x = if x.is_finite() { *x * out } else { 0.0 };
                }
                continue;
            }
            let l = io.first().and_then(|b| b.get(n)).copied().filter(|v| v.is_finite()).unwrap_or(0.0);
            let r = io.get(1).and_then(|b| b.get(n)).copied().filter(|v| v.is_finite()).unwrap_or(0.0);
            let (mid, side) = ((l + r) * 0.5, (l - r) * 0.5);
            // Mono Below: only the side signal's highs stay (24 dB/oct below the frequency).
            let side = if mono_low { self.side_high.process(side) } else { side };
            let side = side * w;
            if let Some(x) = io.first_mut().and_then(|b| b.get_mut(n)) {
                *x = (mid + side) * out;
            }
            if let Some(x) = io.get_mut(1).and_then(|b| b.get_mut(n)) {
                *x = (mid - side) * out;
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Linear-phase EQ

static LINEQ_PARAMS: [ParamInfo; 17] = [
    toggle("hpf", "HPF", false),
    log("hpf_freq", "HPF Freq", 10.0, 500.0, 30.0, Unit::Hz),
    log("low_freq", "Low Freq", 20.0, 500.0, 100.0, Unit::Hz),
    lin("low_gain", "Low Gain", -18.0, 18.0, 0.0, Unit::Db),
    log("lm_freq", "LM Freq", 100.0, 2000.0, 400.0, Unit::Hz),
    lin("lm_gain", "LM Gain", -18.0, 18.0, 0.0, Unit::Db),
    log("lm_q", "LM Q", 0.3, 6.0, 1.0, Unit::None),
    log("hm_freq", "HM Freq", 800.0, 10000.0, 2500.0, Unit::Hz),
    lin("hm_gain", "HM Gain", -18.0, 18.0, 0.0, Unit::Db),
    log("hm_q", "HM Q", 0.3, 6.0, 1.0, Unit::None),
    log("high_freq", "High Freq", 2000.0, 20000.0, 8000.0, Unit::Hz),
    lin("high_gain", "High Gain", -18.0, 18.0, 0.0, Unit::Db),
    toggle("lpf", "LPF", false),
    log("lpf_freq", "LPF Freq", 2000.0, 22000.0, 20000.0, Unit::Hz),
    lin("output", "Output", -18.0, 18.0, 0.0, Unit::Db),
    // How steep the HPF and LPF are.
    choice("slope", "Slope", &["12 dB/oct", "24 dB/oct"], 1),
    lin("mix", "Mix", 0.0, 100.0, 100.0, Unit::Percent),
];

pub static LINEQ_INFO: PluginInfo = PluginInfo {
    id: "linear_phase_eq",
    name: "Linear-Phase EQ",
    short_name: "LinEQ",
    category: Category::Eq,
    params: &LINEQ_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// FIR length: the filter's resolution and half its latency.
const FIR_LEN: usize = 4096;
const LINEQ_BLOCK: usize = 512;

/// The EQ's sections for parameter values `v` (indexed like [`LINEQ_PARAMS`]), as biquad
/// coefficients whose magnitudes multiply (their phase is discarded).
fn lineq_sections(v: &[f32], sr: f32) -> [Option<Coeffs>; 8] {
    let at = |i: usize| v.get(i).copied().unwrap_or(0.0);
    let top = sr * 0.45;
    let steep = at(15) >= 0.5;
    let hp = (at(0) >= 0.5).then(|| Coeffs::design(FilterType::HighPass, at(1).min(top), BUTTERWORTH_Q, 0.0, sr));
    let lp = (at(12) >= 0.5).then(|| Coeffs::design(FilterType::LowPass, at(13).min(top), BUTTERWORTH_Q, 0.0, sr));
    [
        hp,
        if steep { hp } else { None },
        Some(Coeffs::design(FilterType::LowShelf, at(2).min(top), 0.7, at(3), sr)),
        Some(Coeffs::design(FilterType::Peak, at(4).min(top), at(6), at(5), sr)),
        Some(Coeffs::design(FilterType::Peak, at(7).min(top), at(9), at(8), sr)),
        Some(Coeffs::design(FilterType::HighShelf, at(10).min(top), 0.7, at(11), sr)),
        lp,
        if steep { lp } else { None },
    ]
}

fn lineq_db(sections: &[Option<Coeffs>; 8], output_db: f32, f: f32, sr: f32) -> f32 {
    output_db + sections.iter().flatten().map(|c| c.magnitude_db(f, sr)).sum::<f32>()
}

/// Linear-Phase EQ magnitude response in dB at `freqs`, for curve drawing. `params` overrides
/// the defaults by id; unknown ids are ignored.
pub fn linear_eq_response(params: &[(&str, f32)], freqs: &[f32], sr: f32) -> Vec<f32> {
    let mut p = Params::new(&LINEQ_PARAMS);
    for (id, v) in params {
        let _ = p.set(id, *v);
    }
    let v: Vec<f32> = (0..LINEQ_PARAMS.len()).map(|i| p.v(i)).collect();
    let sr = sane_sr(sr);
    let sections = lineq_sections(&v, sr);
    freqs.iter().map(|&f| lineq_db(&sections, p.v(14), f, sr)).collect()
}

/// An EQ with no phase shift: the bands' combined magnitude response becomes a symmetric FIR
/// (frequency sampling, Blackman window), applied by partitioned convolution. Latency is half the
/// FIR plus one block; the dry path is delayed to match for Mix.
pub struct LinearPhaseEq {
    p: Params,
    sr: f32,
    ch: usize,
    convs: Vec<Convolver>,
    dry: Vec<crate::util::DelayLine>,
    design: Option<Arc<dyn Fft<f32>>>,
    spectrum: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    fir: Vec<f32>,
    values: Vec<f32>,
    mix: Smoother,
    stale: bool,
}

impl Default for LinearPhaseEq {
    fn default() -> Self {
        Self::new()
    }
}

impl LinearPhaseEq {
    pub fn new() -> Self {
        let mut e = Self {
            p: Params::new(&LINEQ_PARAMS),
            sr: 48_000.0,
            ch: 2,
            convs: Vec::new(),
            dry: Vec::new(),
            design: None,
            spectrum: Vec::new(),
            scratch: Vec::new(),
            fir: Vec::new(),
            values: vec![0.0; LINEQ_PARAMS.len()],
            mix: Smoother::new(1.0),
            stale: true,
        };
        e.mix.set(e.p.v(16) / 100.0);
        e.mix.snap();
        e
    }

    fn latency_samples() -> usize {
        LINEQ_BLOCK + FIR_LEN / 2
    }

    /// Designs the FIR for the current parameters and loads it into every channel's convolver.
    /// Writes only buffers allocated in `prepare`.
    fn rebuild(&mut self) {
        self.stale = false;
        let Some(ifft) = self.design.clone() else { return };
        if self.spectrum.len() != FIR_LEN || self.fir.len() != FIR_LEN {
            return;
        }
        for (i, v) in self.values.iter_mut().enumerate() {
            *v = self.p.v(i);
        }
        let sr = self.sr;
        let sections = lineq_sections(&self.values, sr);
        let out_db = self.p.v(14);
        let n = FIR_LEN;
        for (k, c) in self.spectrum.iter_mut().enumerate() {
            let bin = if k <= n / 2 { k } else { n - k };
            let f = bin as f32 * sr / n as f32;
            *c = Complex::new(db_to_gain(lineq_db(&sections, out_db, f, sr).clamp(-120.0, 40.0)), 0.0);
        }
        ifft.process_with_scratch(&mut self.spectrum, &mut self.scratch);
        // Zero phase → centred, windowed, linear phase.
        let half = n / 2;
        for (i, h) in self.fir.iter_mut().enumerate() {
            let src = (i + n - half) % n;
            let t = i as f32 / (n - 1) as f32;
            let w = 0.42 - 0.5 * (std::f32::consts::TAU * t).cos() + 0.08 * (2.0 * std::f32::consts::TAU * t).cos();
            *h = self.spectrum.get(src).map_or(0.0, |c| c.re) / n as f32 * w;
        }
        for c in &mut self.convs {
            c.set_ir(&self.fir);
        }
    }
}

impl Plugin for LinearPhaseEq {
    fn info(&self) -> &'static PluginInfo {
        &LINEQ_INFO
    }

    fn param(&self, id: &str) -> Option<f32> {
        self.p.get(id)
    }

    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some(idx) = self.p.set(id, value) else { return false };
        if idx == 16 {
            self.mix.set(self.p.v(16) / 100.0);
        } else {
            self.stale = true;
        }
        true
    }

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.ch = sane_channels(channels);
        let plans = Plans::new(LINEQ_BLOCK);
        self.convs = (0..self.ch).map(|_| Convolver::new(&plans, FIR_LEN)).collect();
        self.dry = (0..self.ch)
            .map(|_| {
                let mut d = crate::util::DelayLine::default();
                d.allocate(Self::latency_samples() + 2);
                d
            })
            .collect();
        let ifft = FftPlanner::<f32>::new().plan_fft_inverse(FIR_LEN);
        self.scratch = vec![Complex::new(0.0, 0.0); ifft.get_inplace_scratch_len()];
        self.design = Some(ifft);
        self.spectrum = vec![Complex::new(0.0, 0.0); FIR_LEN];
        self.fir = vec![0.0; FIR_LEN];
        self.mix.set_time(20.0, self.sr);
        self.stale = true;
        self.reset();
    }

    fn reset(&mut self) {
        self.convs.iter_mut().for_each(Convolver::clear);
        self.dry.iter_mut().for_each(crate::util::DelayLine::clear);
        self.mix.snap();
    }

    fn latency(&self) -> usize {
        self.convs.first().map_or(LINEQ_BLOCK, Convolver::latency) + FIR_LEN / 2
    }

    fn tail_samples(&self) -> usize {
        FIR_LEN / 2
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.ch.min(io.len()).min(self.convs.len()).min(self.dry.len());
        if ch == 0 {
            return;
        }
        if self.stale {
            self.rebuild();
        }
        let frames = frames_in(io, frames, ch);
        let latency = self.latency();
        for n in 0..frames {
            let m = self.mix.next();
            for c in 0..ch {
                let (Some(conv), Some(line)) = (self.convs.get_mut(c), self.dry.get_mut(c)) else { continue };
                let Some(x) = io.get_mut(c).and_then(|b| b.get_mut(n)) else { continue };
                let input = if x.is_finite() { *x } else { 0.0 };
                let wet = conv.process(input);
                line.push(input);
                let dry = line.tap(latency + 1);
                *x = dry * (1.0 - m) + wet * m;
            }
        }
    }
}
