//! Space and time effects: Tremolo, Ping-Pong Delay, Tape Delay and Convolution Reverb.
//!
//! The convolution reverb's spaces are generated here from a few room figures (decay, size,
//! damping, early reflections): no recorded impulse responses are bundled. Convolution is uniformly
//! partitioned overlap-save in the frequency domain, one block of latency.

use std::sync::Arc;

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};

use crate::biquad::{Biquad, FilterType};
use crate::params::{Params, choice, lin, log, param_plumbing, toggle};
use crate::util::{DelayLine, Lfo, Rng, Smoother, flush, frames_in, ms_to_samples, sane_channels, sane_sr};
use crate::{Category, ParamInfo, Plugin, PluginInfo, Transport, Unit};

/// Tempo-synced note values, as quarter notes, in the order of [`DIVISIONS`].
const DIVISION_QUARTERS: [f32; 8] = [4.0, 2.0, 1.0, 0.5, 1.0 / 3.0, 0.25, 1.0 / 6.0, 0.125];
const DIVISIONS: [&str; 8] = ["1/1", "1/2", "1/4", "1/8", "1/8 T", "1/16", "1/16 T", "1/32"];

/// The host tempo, or 120 BPM when the host gave none (or nonsense).
fn sane_tempo(t: f64) -> f32 {
    if t.is_finite() && t >= 10.0 { t.min(999.0) as f32 } else { 120.0 }
}

/// The length of note value `division` at `bpm`, in milliseconds.
fn division_ms(division: usize, bpm: f32) -> f32 {
    let q = DIVISION_QUARTERS.get(division).copied().unwrap_or(1.0);
    q * 60_000.0 / bpm.max(1.0)
}

/// Input sample `n` of channel `c`, or 0 when missing or not finite.
#[inline]
fn input(io: &[Vec<f32>], c: usize, n: usize) -> f32 {
    io.get(c).and_then(|b| b.get(n)).copied().filter(|v| v.is_finite()).unwrap_or(0.0)
}

// ---------------------------------------------------------------------------------------------
// Tremolo

static TREMOLO_PARAMS: [ParamInfo; 6] = [
    log("rate", "Rate", 0.1, 20.0, 5.0, Unit::Hz),
    lin("depth", "Depth", 0.0, 100.0, 50.0, Unit::Percent),
    choice("shape", "Shape", &["Sine", "Triangle", "Square"], 0),
    toggle("sync", "Sync", false),
    choice("division", "Division", &DIVISIONS, 3),
    lin("stereo", "Stereo Phase", 0.0, 100.0, 0.0, Unit::Percent),
];

pub static TREMOLO_INFO: PluginInfo = PluginInfo {
    id: "tremolo",
    name: "Tremolo",
    short_name: "Tremolo",
    category: Category::Modulation,
    params: &TREMOLO_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// Amplitude modulation by a sine, triangle or (softened) square LFO, free or tempo-synced; the
/// right channel can run up to half a cycle behind the left for a stereo throb.
pub struct Tremolo {
    p: Params,
    sr: f32,
    ch: usize,
    lfo: Lfo,
    depth: Smoother,
    tempo: f32,
}

impl Default for Tremolo {
    fn default() -> Self {
        Self::new()
    }
}

impl Tremolo {
    pub fn new() -> Self {
        let mut t = Self { p: Params::new(&TREMOLO_PARAMS), sr: 48_000.0, ch: 2, lfo: Lfo::new(), depth: Smoother::new(0.5), tempo: 120.0 };
        t.update();
        t.depth.snap();
        t
    }

    fn update(&mut self) {
        self.depth.set(self.p.v(1) / 100.0);
    }

    fn rate_hz(&self) -> f32 {
        if self.p.on(3) { 1000.0 / division_ms(self.p.choice(4), self.tempo) } else { self.p.v(0) }
    }

    /// The LFO shape at `phase` (0..1), from -1 to 1.
    fn shape(&self, phase: f32) -> f32 {
        match self.p.choice(2) {
            1 => 1.0 - 4.0 * (phase - 0.5).abs(),
            2 => (Lfo::sin_at(phase) * 6.0).tanh(),
            _ => Lfo::sin_at(phase),
        }
    }
}

impl Plugin for Tremolo {
    param_plumbing!(TREMOLO_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.ch = sane_channels(channels);
        self.depth.set_time(20.0, self.sr);
        self.reset();
    }

    fn reset(&mut self) {
        self.lfo.reset();
        self.depth.snap();
    }

    fn set_transport(&mut self, transport: &Transport) {
        self.tempo = sane_tempo(transport.tempo);
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.ch.min(io.len());
        if ch == 0 {
            return;
        }
        let frames = frames_in(io, frames, ch);
        let inc = self.rate_hz() / self.sr;
        let spread = self.p.v(5) / 100.0 * 0.5;
        for n in 0..frames {
            let phase = self.lfo.tick(inc);
            let depth = self.depth.next();
            for (c, b) in io.iter_mut().take(ch).enumerate() {
                let ph = if c % 2 == 1 { (phase + spread).fract() } else { phase };
                let gain = 1.0 - depth * (0.5 - 0.5 * self.shape(ph));
                if let Some(x) = b.get_mut(n) {
                    *x = if x.is_finite() { *x * gain } else { 0.0 };
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Ping-pong delay

const MAX_ECHO_MS: f32 = 2000.0;

static PINGPONG_PARAMS: [ParamInfo; 8] = [
    log("time", "Time", 1.0, MAX_ECHO_MS, 375.0, Unit::Ms),
    toggle("sync", "Sync", false),
    choice("division", "Division", &DIVISIONS, 3),
    lin("feedback", "Feedback", 0.0, 95.0, 40.0, Unit::Percent),
    lin("mix", "Mix", 0.0, 100.0, 30.0, Unit::Percent),
    lin("width", "Width", 0.0, 100.0, 100.0, Unit::Percent),
    log("low_cut", "Low Cut", 20.0, 2000.0, 80.0, Unit::Hz),
    log("high_cut", "High Cut", 1000.0, 20000.0, 8000.0, Unit::Hz),
];

pub static PINGPONG_INFO: PluginInfo = PluginInfo {
    id: "pingpong_delay",
    name: "Ping-Pong Delay",
    short_name: "PingPong",
    category: Category::Delay,
    params: &PINGPONG_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// A stereo delay whose echoes bounce between the left and right channels: the input (summed to
/// mono) enters the left line, each line feeds the other, with low and high cuts in the loop.
pub struct PingPong {
    p: Params,
    sr: f32,
    ch: usize,
    lines: [DelayLine; 2],
    low: [Biquad; 2],
    high: [Biquad; 2],
    time: Smoother,
    fb: Smoother,
    mix: Smoother,
    width: Smoother,
    tempo: f32,
}

impl Default for PingPong {
    fn default() -> Self {
        Self::new()
    }
}

impl PingPong {
    pub fn new() -> Self {
        let mut d = Self {
            p: Params::new(&PINGPONG_PARAMS),
            sr: 48_000.0,
            ch: 2,
            lines: Default::default(),
            low: [Biquad::default(); 2],
            high: [Biquad::default(); 2],
            time: Smoother::new(375.0),
            fb: Smoother::new(0.4),
            mix: Smoother::new(0.3),
            width: Smoother::new(1.0),
            tempo: 120.0,
        };
        d.update();
        d.snap();
        d
    }

    fn snap(&mut self) {
        self.time.snap();
        self.fb.snap();
        self.mix.snap();
        self.width.snap();
    }

    fn time_ms(&self) -> f32 {
        let ms = if self.p.on(1) { division_ms(self.p.choice(2), self.tempo) } else { self.p.v(0) };
        ms.clamp(1.0, MAX_ECHO_MS)
    }

    fn update(&mut self) {
        self.time.set(self.time_ms());
        self.fb.set(self.p.v(3) / 100.0);
        self.mix.set(self.p.v(4) / 100.0);
        self.width.set(self.p.v(5) / 100.0);
        let (lc, hc, sr) = (self.p.v(6), self.p.v(7).min(self.sr * 0.45), self.sr);
        for b in &mut self.low {
            b.set(FilterType::HighPass, lc, std::f32::consts::FRAC_1_SQRT_2, 0.0, sr);
        }
        for b in &mut self.high {
            b.set(FilterType::LowPass, hc, std::f32::consts::FRAC_1_SQRT_2, 0.0, sr);
        }
    }
}

impl Plugin for PingPong {
    param_plumbing!(PINGPONG_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.ch = sane_channels(channels);
        let max = ms_to_samples(MAX_ECHO_MS, self.sr) as usize + 8;
        self.lines.iter_mut().for_each(|l| l.allocate(max));
        self.time.set_time(80.0, self.sr);
        self.fb.set_time(20.0, self.sr);
        self.mix.set_time(20.0, self.sr);
        self.width.set_time(20.0, self.sr);
        self.update();
        self.reset();
    }

    fn reset(&mut self) {
        self.lines.iter_mut().for_each(DelayLine::clear);
        self.low.iter_mut().chain(self.high.iter_mut()).for_each(Biquad::reset);
        self.snap();
    }

    fn set_transport(&mut self, transport: &Transport) {
        self.tempo = sane_tempo(transport.tempo);
        if self.p.on(1) {
            self.time.set(self.time_ms());
        }
    }

    fn tail_samples(&self) -> usize {
        let fb = (self.p.v(3) / 100.0).clamp(0.0, 0.95);
        // Echoes until they are 60 dB down, two lines a round.
        let rounds = if fb <= 1.0e-3 { 2.0 } else { (-3.0 / fb.log10()).min(200.0) * 2.0 };
        (ms_to_samples(self.time_ms() * rounds, self.sr) as usize).min(self.sr as usize * 60)
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.ch.min(io.len());
        if ch == 0 {
            return;
        }
        let frames = frames_in(io, frames, ch);
        let sr = self.sr;
        for n in 0..frames {
            let d = ms_to_samples(self.time.next(), sr).max(1.0);
            let (fb, m, w) = (self.fb.next(), self.mix.next(), self.width.next());
            let (xl, xr) = (input(io, 0, n), if ch > 1 { input(io, 1, n) } else { input(io, 0, n) });
            let [l, r] = &mut self.lines;
            let (el, er) = (l.read_cubic(d), r.read_cubic(d));
            let back_r = self.high[1].process_sample(self.low[1].process_sample(er));
            let back_l = self.high[0].process_sample(self.low[0].process_sample(el));
            l.push(flush((xl + xr) * 0.5 + back_r * fb));
            r.push(flush(back_l * fb));
            if ch == 1 {
                if let Some(x) = io.first_mut().and_then(|b| b.get_mut(n)) {
                    *x = xl * (1.0 - m) + (el + er) * m;
                }
                continue;
            }
            // Width narrows the bounce toward the middle.
            let mid = (el + er) * 0.5;
            let (wl, wr) = (mid + (el - mid) * w, mid + (er - mid) * w);
            for (c, b) in io.iter_mut().take(ch).enumerate() {
                let (dry, wet) = if c % 2 == 0 { (xl, wl) } else { (xr, wr) };
                if let Some(x) = b.get_mut(n) {
                    let dry = if c < 2 {
                        dry
                    } else if x.is_finite() {
                        *x
                    } else {
                        0.0
                    };
                    *x = dry * (1.0 - m) + wet * m;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Tape delay

static TAPE_DELAY_PARAMS: [ParamInfo; 10] = [
    log("time", "Time", 20.0, MAX_ECHO_MS, 300.0, Unit::Ms),
    toggle("sync", "Sync", false),
    choice("division", "Division", &DIVISIONS, 3),
    lin("feedback", "Feedback", 0.0, 99.0, 45.0, Unit::Percent),
    lin("wow", "Wow", 0.0, 100.0, 20.0, Unit::Percent),
    lin("flutter", "Flutter", 0.0, 100.0, 15.0, Unit::Percent),
    lin("drive", "Drive", 0.0, 24.0, 6.0, Unit::Db),
    log("tone", "Tone", 1000.0, 12000.0, 4500.0, Unit::Hz),
    log("low_cut", "Low Cut", 20.0, 1000.0, 120.0, Unit::Hz),
    lin("mix", "Mix", 0.0, 100.0, 30.0, Unit::Percent),
];

pub static TAPE_DELAY_INFO: PluginInfo = PluginInfo {
    id: "tape_delay",
    name: "Tape Delay",
    short_name: "TapeDly",
    category: Category::Delay,
    params: &TAPE_DELAY_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// Wow: a slow drift of the tape speed. Flutter: a faster, smaller wobble.
const WOW_HZ: f32 = 0.6;
const WOW_MS: f32 = 3.0;
const FLUTTER_HZ: f32 = 7.3;
const FLUTTER_MS: f32 = 0.35;

/// An echo through a worn tape loop: the read head wanders (wow and flutter), and every repeat is
/// darker, thinner and more saturated than the last.
pub struct TapeDelay {
    p: Params,
    sr: f32,
    ch: usize,
    lines: Vec<DelayLine>,
    tone: Vec<Biquad>,
    low: Vec<Biquad>,
    wow: Lfo,
    flutter: Lfo,
    drift: f32,
    rng: Rng,
    time: Smoother,
    fb: Smoother,
    mix: Smoother,
    tempo: f32,
}

impl Default for TapeDelay {
    fn default() -> Self {
        Self::new()
    }
}

impl TapeDelay {
    pub fn new() -> Self {
        let mut d = Self {
            p: Params::new(&TAPE_DELAY_PARAMS),
            sr: 48_000.0,
            ch: 2,
            lines: Vec::new(),
            tone: Vec::new(),
            low: Vec::new(),
            wow: Lfo::new(),
            flutter: Lfo::new(),
            drift: 0.0,
            rng: Rng::new(0x7a9e),
            time: Smoother::new(300.0),
            fb: Smoother::new(0.45),
            mix: Smoother::new(0.3),
            tempo: 120.0,
        };
        d.update();
        d.time.snap();
        d.fb.snap();
        d.mix.snap();
        d
    }

    fn time_ms(&self) -> f32 {
        let ms = if self.p.on(1) { division_ms(self.p.choice(2), self.tempo) } else { self.p.v(0) };
        ms.clamp(20.0, MAX_ECHO_MS)
    }

    fn update(&mut self) {
        self.time.set(self.time_ms());
        self.fb.set(self.p.v(3) / 100.0);
        self.mix.set(self.p.v(9) / 100.0);
        let (tone, lc, sr) = (self.p.v(7).min(self.sr * 0.45), self.p.v(8), self.sr);
        for b in &mut self.tone {
            b.set(FilterType::LowPass, tone, 0.6, 0.0, sr);
        }
        for b in &mut self.low {
            b.set(FilterType::HighPass, lc, 0.6, 0.0, sr);
        }
    }
}

impl Plugin for TapeDelay {
    param_plumbing!(TAPE_DELAY_INFO);

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.ch = sane_channels(channels);
        let max = ms_to_samples(MAX_ECHO_MS + WOW_MS + FLUTTER_MS, self.sr) as usize + 8;
        self.lines = (0..self.ch).map(|_| DelayLine::default()).collect();
        self.lines.iter_mut().for_each(|l| l.allocate(max));
        self.tone = vec![Biquad::default(); self.ch];
        self.low = vec![Biquad::default(); self.ch];
        self.time.set_time(150.0, self.sr);
        self.fb.set_time(20.0, self.sr);
        self.mix.set_time(20.0, self.sr);
        self.update();
        self.reset();
    }

    fn reset(&mut self) {
        self.lines.iter_mut().for_each(DelayLine::clear);
        self.tone.iter_mut().chain(self.low.iter_mut()).for_each(Biquad::reset);
        self.wow.reset();
        self.flutter.reset();
        self.drift = 0.0;
        self.time.snap();
        self.fb.snap();
        self.mix.snap();
    }

    fn set_transport(&mut self, transport: &Transport) {
        self.tempo = sane_tempo(transport.tempo);
        if self.p.on(1) {
            self.time.set(self.time_ms());
        }
    }

    fn tail_samples(&self) -> usize {
        let fb = (self.p.v(3) / 100.0).clamp(0.0, 0.99);
        let rounds = if fb <= 1.0e-3 { 1.0 } else { (-3.0 / fb.log10()).min(400.0) };
        (ms_to_samples(self.time_ms() * rounds, self.sr) as usize).min(self.sr as usize * 60)
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.ch.min(io.len()).min(self.lines.len());
        if ch == 0 {
            return;
        }
        let frames = frames_in(io, frames, ch);
        let sr = self.sr;
        let (wow_amt, flutter_amt) = (self.p.v(4) / 100.0, self.p.v(5) / 100.0);
        let drive = crate::db_to_gain(self.p.v(6));
        let makeup = 1.0 / drive.tanh().max(0.1);
        for n in 0..frames {
            let base = ms_to_samples(self.time.next(), sr);
            let (fb, m) = (self.fb.next(), self.mix.next());
            // A slow random drift on top of the periodic wow keeps it from sounding like a chorus.
            self.drift += (self.rng.bipolar() - self.drift) * (2.0 / sr);
            let w = Lfo::sin_at(self.wow.tick(WOW_HZ / sr)) * 0.7 + self.drift * 30.0;
            let f = Lfo::sin_at(self.flutter.tick(FLUTTER_HZ / sr));
            let wobble = ms_to_samples(WOW_MS, sr) * wow_amt * (1.0 + w.clamp(-1.5, 1.5)) * 0.5
                + ms_to_samples(FLUTTER_MS, sr) * flutter_amt * (1.0 + f) * 0.5;
            let d = (base + wobble).max(1.0);
            for c in 0..ch {
                let x = input(io, c, n);
                let (Some(line), Some(tone), Some(low)) = (self.lines.get_mut(c), self.tone.get_mut(c), self.low.get_mut(c)) else { continue };
                let echo = line.read_cubic(d);
                let back = (low.process_sample(tone.process_sample(echo)) * drive).tanh() * makeup;
                line.push(flush(x + back * fb));
                if let Some(o) = io.get_mut(c).and_then(|b| b.get_mut(n)) {
                    *o = x * (1.0 - m) + echo * m;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Convolution reverb

/// Convolution block: one block of latency; cost per sample grows with IR length / block.
const BLOCK: usize = 512;
const FFT_LEN: usize = BLOCK * 2;
/// The longest impulse response, in seconds, and the sample rate it is budgeted at: above it,
/// fewer seconds fit (the partition count is fixed at prepare).
const MAX_IR_SECS: f32 = 6.0;
const IR_BUDGET_SR: f32 = 96_000.0;
const MAX_PREDELAY_MS: f32 = 250.0;

/// A space's figures: RT60 (s), size (scales the early reflections, ms), damping (Hz at the start
/// and the end of the tail), early-reflection level, and the onset of the diffuse tail (ms).
struct Space {
    rt60: f32,
    size_ms: f32,
    bright_hz: f32,
    dark_hz: f32,
    early: f32,
    onset_ms: f32,
}

static SPACES: [Space; 6] = [
    // Vocal booth: short, dead, close reflections.
    Space { rt60: 0.35, size_ms: 4.0, bright_hz: 9000.0, dark_hz: 3000.0, early: 0.6, onset_ms: 3.0 },
    // Studio room.
    Space { rt60: 0.8, size_ms: 9.0, bright_hz: 11000.0, dark_hz: 3500.0, early: 0.5, onset_ms: 6.0 },
    // Chamber: hard walls, bright.
    Space { rt60: 1.6, size_ms: 14.0, bright_hz: 13000.0, dark_hz: 5000.0, early: 0.45, onset_ms: 8.0 },
    // Concert hall.
    Space { rt60: 2.6, size_ms: 28.0, bright_hz: 10000.0, dark_hz: 2500.0, early: 0.35, onset_ms: 18.0 },
    // Cathedral: huge, dark, slow.
    Space { rt60: 4.8, size_ms: 45.0, bright_hz: 8000.0, dark_hz: 1500.0, early: 0.3, onset_ms: 30.0 },
    // Plate: no early reflections, dense at once, bright.
    Space { rt60: 2.2, size_ms: 0.0, bright_hz: 16000.0, dark_hz: 6000.0, early: 0.0, onset_ms: 0.5 },
];

static CONV_PARAMS: [ParamInfo; 7] = [
    choice("space", "Space", &["Vocal Booth", "Studio Room", "Chamber", "Concert Hall", "Cathedral", "Plate"], 1),
    lin("length", "Length", 25.0, 200.0, 100.0, Unit::Percent),
    lin("pre_delay", "Pre-Delay", 0.0, MAX_PREDELAY_MS, 0.0, Unit::Ms),
    log("low_cut", "Low Cut", 20.0, 1000.0, 80.0, Unit::Hz),
    log("high_cut", "High Cut", 1000.0, 20000.0, 14000.0, Unit::Hz),
    lin("width", "Width", 0.0, 100.0, 100.0, Unit::Percent),
    lin("mix", "Mix", 0.0, 100.0, 25.0, Unit::Percent),
];

pub static CONV_INFO: PluginInfo = PluginInfo {
    id: "convolution_reverb",
    name: "Convolution Reverb",
    short_name: "ConvVerb",
    category: Category::Reverb,
    params: &CONV_PARAMS,
    is_instrument: false,
    audiosuite: true,
};

/// One channel's convolution state: the IR's spectra by partition, the input spectra history
/// (a ring), and the last two input blocks.
struct Channel {
    ir: Vec<Vec<Complex<f32>>>,
    history: Vec<Vec<Complex<f32>>>,
    input: Vec<f32>,
}

/// Convolves with a generated impulse response of the chosen space. The dry signal is delayed by
/// the same block, so dry and wet stay aligned and the reported latency covers both.
pub struct ConvolutionReverb {
    p: Params,
    sr: f32,
    ch: usize,
    fwd: Option<Arc<dyn Fft<f32>>>,
    inv: Option<Arc<dyn Fft<f32>>>,
    scratch: Vec<Complex<f32>>,
    work: Vec<Complex<f32>>,
    acc: Vec<Complex<f32>>,
    sides: Vec<Channel>,
    /// Time-domain IR being built (one side at a time).
    ir: Vec<f32>,
    /// Partitions in use for the current IR, and the ring's write position.
    parts: usize,
    head: usize,
    /// Position within the current block, and the block's wet output.
    fill: usize,
    wet: [Vec<f32>; 2],
    dry: Vec<DelayLine>,
    pre: [DelayLine; 2],
    low: [Biquad; 2],
    high: [Biquad; 2],
    mix: Smoother,
    width: Smoother,
    pre_ms: Smoother,
    /// The IR must be rebuilt (space or length changed) before the next block.
    stale: bool,
}

impl Default for ConvolutionReverb {
    fn default() -> Self {
        Self::new()
    }
}

impl ConvolutionReverb {
    pub fn new() -> Self {
        let mut r = Self {
            p: Params::new(&CONV_PARAMS),
            sr: 48_000.0,
            ch: 2,
            fwd: None,
            inv: None,
            scratch: Vec::new(),
            work: Vec::new(),
            acc: Vec::new(),
            sides: Vec::new(),
            ir: Vec::new(),
            parts: 0,
            head: 0,
            fill: 0,
            wet: [Vec::new(), Vec::new()],
            dry: Vec::new(),
            pre: Default::default(),
            low: [Biquad::default(); 2],
            high: [Biquad::default(); 2],
            mix: Smoother::new(0.25),
            width: Smoother::new(1.0),
            pre_ms: Smoother::new(0.0),
            stale: true,
        };
        r.update();
        r.mix.snap();
        r.width.snap();
        r.pre_ms.snap();
        r
    }

    fn space(&self) -> &'static Space {
        SPACES.get(self.p.choice(0)).unwrap_or(&SPACES[1])
    }

    fn rt60(&self) -> f32 {
        (self.space().rt60 * self.p.v(1) / 100.0).max(0.05)
    }

    fn update(&mut self) {
        self.mix.set(self.p.v(6) / 100.0);
        self.width.set(self.p.v(5) / 100.0);
        self.pre_ms.set(self.p.v(2));
        let (lc, hc, sr) = (self.p.v(3), self.p.v(4).min(self.sr * 0.45), self.sr);
        for b in &mut self.low {
            b.set(FilterType::HighPass, lc, std::f32::consts::FRAC_1_SQRT_2, 0.0, sr);
        }
        for b in &mut self.high {
            b.set(FilterType::LowPass, hc, std::f32::consts::FRAC_1_SQRT_2, 0.0, sr);
        }
    }

    fn max_parts(&self) -> usize {
        let samples = MAX_IR_SECS * self.sr.min(IR_BUDGET_SR);
        (samples as usize).div_ceil(BLOCK).max(1)
    }

    /// Writes side `side`'s impulse response into `self.ir` and returns its length.
    fn generate(&mut self, side: usize) -> usize {
        let sp = self.space();
        let sr = self.sr;
        let rt60 = self.rt60();
        let cap = self.ir.len();
        let len = ((rt60 * 1.15 * sr) as usize).clamp(BLOCK, cap.max(BLOCK)).min(cap);
        let mut rng = Rng::new(0x51ce + side as u32 * 7919 + self.p.choice(0) as u32 * 104_729);
        let onset = ms_to_samples(sp.onset_ms, sr).max(1.0);
        let mut lp = 0.0f32;
        let mut energy = 0.0f64;
        for (n, out) in self.ir.iter_mut().enumerate() {
            if n >= len {
                *out = 0.0;
                continue;
            }
            let t = n as f32 / sr;
            // Damping sweeps from bright to dark over the decay, as air and walls absorb highs.
            let k = (t / rt60).min(1.0);
            let hz = sp.bright_hz * (sp.dark_hz / sp.bright_hz).powf(k);
            let a = (-std::f32::consts::TAU * hz.min(sr * 0.45) / sr).exp();
            lp = flush(rng.bipolar() * (1.0 - a) + lp * a);
            let decay = (-6.907_755 * t / rt60).exp();
            let build = (n as f32 / onset).min(1.0);
            // The low-passed noise is quieter the darker it gets; keep its level even.
            let norm = (1.0 / (1.0 - a).max(1.0e-3)).sqrt();
            let v = lp * norm * decay * build;
            *out = v;
            energy += f64::from(v) * f64::from(v);
        }
        // Early reflections: a few discrete taps spread over the room's size, louder first.
        if sp.early > 0.0 {
            for i in 0..12u32 {
                let jitter = 0.6 + 0.8 * (rng.next_u32() % 1000) as f32 / 1000.0;
                let ms = sp.size_ms * (0.3 + i as f32 * 0.35) * jitter;
                let at = ms_to_samples(ms, sr) as usize;
                let sign = if rng.next_u32() & 1 == 0 { 1.0 } else { -1.0 };
                let gain = sp.early * sign * (-0.25 * i as f32).exp() * 4.0;
                if let Some(s) = self.ir.get_mut(at) {
                    *s += gain;
                    energy += f64::from(gain) * f64::from(gain);
                }
            }
        }
        // Same overall energy for every space, so switching doesn't jump in level.
        let scale = if energy > 0.0 { (0.35 / energy.sqrt()) as f32 } else { 0.0 };
        self.ir.iter_mut().take(len).for_each(|v| *v *= scale);
        len
    }

    /// Rebuilds both sides' IR spectra. Runs on the audio thread at a block boundary when the
    /// space or length changed; everything it writes was allocated in `prepare`.
    fn rebuild(&mut self) {
        self.stale = false;
        let Some(fwd) = self.fwd.clone() else { return };
        let mut parts = 0;
        for side in 0..self.sides.len().min(2) {
            let len = self.generate(side);
            let used = len.div_ceil(BLOCK).min(self.max_parts());
            parts = parts.max(used);
            let Some(ch) = self.sides.get_mut(side) else { continue };
            for (k, spec) in ch.ir.iter_mut().enumerate() {
                if k >= used {
                    spec.iter_mut().for_each(|c| *c = Complex::new(0.0, 0.0));
                    continue;
                }
                for (j, c) in spec.iter_mut().enumerate() {
                    let v = if j < BLOCK { self.ir.get(k * BLOCK + j).copied().unwrap_or(0.0) } else { 0.0 };
                    *c = Complex::new(v, 0.0);
                }
                fwd.process_with_scratch(spec, &mut self.scratch);
            }
        }
        self.parts = parts.max(1);
    }

    /// Convolves the block just filled into `self.wet`.
    fn convolve(&mut self) {
        let (Some(fwd), Some(inv)) = (self.fwd.clone(), self.inv.clone()) else { return };
        let parts = self.parts.min(self.max_parts()).max(1);
        let ring = self.sides.first().map_or(0, |c| c.history.len());
        if ring == 0 {
            return;
        }
        for side in 0..self.sides.len().min(2) {
            let Some(ch) = self.sides.get_mut(side) else { continue };
            // This block's input spectrum: the last two input blocks (overlap-save).
            for (w, x) in self.work.iter_mut().zip(ch.input.iter()) {
                *w = Complex::new(*x, 0.0);
            }
            fwd.process_with_scratch(&mut self.work, &mut self.scratch);
            if let Some(slot) = ch.history.get_mut(self.head) {
                slot.copy_from_slice(&self.work);
            }
            self.acc.iter_mut().for_each(|c| *c = Complex::new(0.0, 0.0));
            for k in 0..parts {
                let idx = (self.head + ring - k % ring) % ring;
                let (Some(x), Some(h)) = (ch.history.get(idx), ch.ir.get(k)) else { continue };
                for ((a, xv), hv) in self.acc.iter_mut().zip(x.iter()).zip(h.iter()) {
                    *a += xv * hv;
                }
            }
            inv.process_with_scratch(&mut self.acc, &mut self.scratch);
            let norm = 1.0 / FFT_LEN as f32;
            if let Some(out) = self.wet.get_mut(side) {
                for (o, c) in out.iter_mut().zip(self.acc.iter().skip(BLOCK)) {
                    *o = flush(c.re * norm);
                }
            }
            // Slide the input window: the block just used becomes the older half.
            ch.input.copy_within(BLOCK.., 0);
        }
        self.head = (self.head + 1) % ring;
    }
}

impl Plugin for ConvolutionReverb {
    fn info(&self) -> &'static PluginInfo {
        &CONV_INFO
    }

    fn param(&self, id: &str) -> Option<f32> {
        self.p.get(id)
    }

    fn prepare(&mut self, sample_rate: f32, _max_block: usize, channels: usize) {
        self.sr = sane_sr(sample_rate);
        self.ch = sane_channels(channels);
        let mut planner = FftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(FFT_LEN);
        let inv = planner.plan_fft_inverse(FFT_LEN);
        let scratch = fwd.get_inplace_scratch_len().max(inv.get_inplace_scratch_len());
        self.scratch = vec![Complex::new(0.0, 0.0); scratch];
        self.fwd = Some(fwd);
        self.inv = Some(inv);
        let parts = self.max_parts();
        self.work = vec![Complex::new(0.0, 0.0); FFT_LEN];
        self.acc = vec![Complex::new(0.0, 0.0); FFT_LEN];
        let sides = self.ch.clamp(1, 2);
        self.sides = (0..sides)
            .map(|_| Channel {
                ir: vec![vec![Complex::new(0.0, 0.0); FFT_LEN]; parts],
                history: vec![vec![Complex::new(0.0, 0.0); FFT_LEN]; parts],
                input: vec![0.0; FFT_LEN],
            })
            .collect();
        self.ir = vec![0.0; parts * BLOCK];
        self.wet = [vec![0.0; BLOCK], vec![0.0; BLOCK]];
        self.dry = (0..self.ch).map(|_| DelayLine::default()).collect();
        self.dry.iter_mut().for_each(|d| d.allocate(BLOCK + 2));
        let pre_max = ms_to_samples(MAX_PREDELAY_MS, self.sr) as usize + 4;
        self.pre.iter_mut().for_each(|d| d.allocate(pre_max));
        self.mix.set_time(30.0, self.sr);
        self.width.set_time(30.0, self.sr);
        self.pre_ms.set_time(100.0, self.sr);
        self.update();
        self.stale = true;
        self.reset();
    }

    fn set_param(&mut self, id: &str, value: f32) -> bool {
        let Some(idx) = self.p.set(id, value) else { return false };
        // Space and length change the impulse response itself.
        if idx <= 1 {
            self.stale = true;
        }
        self.update();
        true
    }

    fn reset(&mut self) {
        for ch in &mut self.sides {
            ch.history.iter_mut().for_each(|h| h.iter_mut().for_each(|c| *c = Complex::new(0.0, 0.0)));
            ch.input.iter_mut().for_each(|v| *v = 0.0);
        }
        self.wet.iter_mut().for_each(|w| w.iter_mut().for_each(|v| *v = 0.0));
        self.dry.iter_mut().for_each(DelayLine::clear);
        self.pre.iter_mut().for_each(DelayLine::clear);
        self.low.iter_mut().chain(self.high.iter_mut()).for_each(Biquad::reset);
        self.head = 0;
        self.fill = 0;
        self.mix.snap();
        self.width.snap();
        self.pre_ms.snap();
    }

    fn latency(&self) -> usize {
        BLOCK
    }

    fn tail_samples(&self) -> usize {
        let secs = (self.rt60() * 1.15).min(MAX_IR_SECS) + self.p.v(2) * 0.001;
        (secs * self.sr) as usize + BLOCK
    }

    fn process(&mut self, io: &mut [Vec<f32>], frames: usize) {
        let ch = self.ch.min(io.len()).min(self.dry.len());
        if ch == 0 || self.sides.is_empty() {
            return;
        }
        if self.stale {
            self.rebuild();
        }
        let frames = frames_in(io, frames, ch);
        let sr = self.sr;
        for n in 0..frames {
            let (xl, xr) = (input(io, 0, n), if ch > 1 { input(io, 1, n) } else { input(io, 0, n) });
            let pre = ms_to_samples(self.pre_ms.next(), sr);
            // Into the convolution: pre-delayed input, one side per channel (mono feeds both).
            for (side, x) in [xl, xr].into_iter().enumerate().take(self.sides.len()) {
                let d = &mut self.pre[side];
                d.push(x);
                let v = if pre < 1.0 { x } else { d.read(pre) };
                if let Some(slot) = self.sides.get_mut(side).and_then(|c| c.input.get_mut(BLOCK + self.fill)) {
                    *slot = v;
                }
            }
            // Out of it: the previous block's wet signal, filtered and widened.
            let w = self.wet[0].get(self.fill).copied().unwrap_or(0.0);
            let wr = if self.sides.len() > 1 { self.wet[1].get(self.fill).copied().unwrap_or(0.0) } else { w };
            let (wl, wr) = (self.high[0].process_sample(self.low[0].process_sample(w)), self.high[1].process_sample(self.low[1].process_sample(wr)));
            let width = self.width.next();
            let mid = (wl + wr) * 0.5;
            let (wl, wr) = (mid + (wl - mid) * width, mid + (wr - mid) * width);
            let m = self.mix.next();
            for c in 0..ch {
                let x = input(io, c, n);
                let Some(line) = self.dry.get_mut(c) else { continue };
                line.push(x);
                // `tap(1)` is the sample just pushed: `BLOCK + 1` is the block's latency.
                let dry = line.tap(BLOCK + 1);
                let wet = if ch == 1 {
                    (wl + wr) * 0.5
                } else if c % 2 == 0 {
                    wl
                } else {
                    wr
                };
                if let Some(o) = io.get_mut(c).and_then(|b| b.get_mut(n)) {
                    // Channels past the first two pass through, delayed with the rest.
                    *o = if c < 2 { dry * (1.0 - m) + wet * m } else { dry };
                }
            }
            self.fill += 1;
            if self.fill >= BLOCK {
                self.fill = 0;
                self.convolve();
            }
        }
    }
}
