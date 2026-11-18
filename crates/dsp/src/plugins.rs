//! The built-in plugins and their registry.

pub mod auto_pan;
pub mod drums;
pub mod dynamics;
pub mod eq;
pub mod freq_shifter;
pub mod harmonic;
pub mod mixbus;
pub mod modulation;
pub mod pitch;
pub mod reverb;
pub mod sampler;
pub mod space;
pub mod strip;
pub mod synth;
pub mod utility;
pub mod vocal;

use crate::{Plugin, PluginInfo};

/// Every built-in plugin, in menu order.
pub(crate) static REGISTRY: &[&PluginInfo] = &[
    &eq::EQ1_INFO,
    &eq::EQ7_INFO,
    &mixbus::LINEQ_INFO,
    &dynamics::COMP_INFO,
    &dynamics::GATE_INFO,
    &dynamics::DEESS_INFO,
    &vocal::FET_INFO,
    &vocal::OPTO_INFO,
    &dynamics::MAX_INFO,
    &mixbus::MB_INFO,
    &strip::STRIP_INFO,
    &reverb::ROOM_INFO,
    &reverb::PLATE_INFO,
    &reverb::HALL_INFO,
    &reverb::CHAMBER_INFO,
    &space::CONV_INFO,
    &modulation::DELAY_INFO,
    &space::PINGPONG_INFO,
    &space::TAPE_DELAY_INFO,
    &auto_pan::AUTOPAN_INFO,
    &modulation::CHORUS_INFO,
    &modulation::FLANGER_INFO,
    &modulation::PHASER_INFO,
    &space::TREMOLO_INFO,
    &modulation::RING_INFO,
    &freq_shifter::FREQ_SHIFTER_INFO,
    &harmonic::SAT_INFO,
    &mixbus::TAPE_INFO,
    &harmonic::LOFI_INFO,
    &harmonic::RECT_INFO,
    &harmonic::SUB_INFO,
    &pitch::PITCH_INFO,
    &vocal::TUNE_INFO,
    &utility::SHIFT_INFO,
    &mixbus::IMAGER_INFO,
    &utility::GAIN_INFO,
    &utility::TRIM_INFO,
    &utility::INVERT_INFO,
    &utility::DC_INFO,
    &utility::GEN_INFO,
    &utility::DITHER_INFO,
    &synth::SYNTH_INFO,
    &drums::DRUM_INFO,
    &sampler::SAMPLER_INFO,
];

/// Constructs an (unprepared) plugin by id.
pub(crate) fn instantiate(id: &str) -> Option<Box<dyn Plugin>> {
    Some(match id {
        "eq_1band" => Box::new(eq::Eq1::new()),
        "eq_7band" => Box::new(eq::Eq7::new()),
        "linear_phase_eq" => Box::new(mixbus::LinearPhaseEq::new()),
        "multiband_compressor" => Box::new(mixbus::Multiband::new()),
        "tape_saturation" => Box::new(mixbus::TapeSaturation::new()),
        "stereo_imager" => Box::new(mixbus::StereoImager::new()),
        "compressor" => Box::new(dynamics::Compressor::new()),
        "expander_gate" => Box::new(dynamics::ExpanderGate::new()),
        "de_esser" => Box::new(dynamics::DeEsser::new()),
        "fet_compressor" => Box::new(vocal::FetCompressor::new()),
        "opto_compressor" => Box::new(vocal::OptoCompressor::new()),
        "pitch_correct" => Box::new(vocal::PitchCorrect::new()),
        "maximizer" => Box::new(dynamics::Maximizer::new()),
        "channel_strip" => Box::new(strip::ChannelStrip::new()),
        "room_reverb" => Box::new(reverb::Reverb::room()),
        "plate_reverb" => Box::new(reverb::Reverb::plate()),
        "hall_reverb" => Box::new(reverb::Reverb::hall()),
        "chamber_reverb" => Box::new(reverb::Reverb::chamber()),
        "convolution_reverb" => Box::new(space::ConvolutionReverb::new()),
        "pingpong_delay" => Box::new(space::PingPong::new()),
        "tape_delay" => Box::new(space::TapeDelay::new()),
        "tremolo" => Box::new(space::Tremolo::new()),
        "mod_delay" => Box::new(modulation::ModDelay::new()),
        "auto_pan" => Box::new(auto_pan::AutoPan::new()),
        "chorus" => Box::new(modulation::ModFx::chorus()),
        "flanger" => Box::new(modulation::ModFx::flanger()),
        "phaser" => Box::new(modulation::Phaser::new()),
        "ring_mod" => Box::new(modulation::RingMod::new()),
        "freq_shifter" => Box::new(freq_shifter::FreqShifter::new()),
        "saturator" => Box::new(harmonic::Saturator::new()),
        "lofi" => Box::new(harmonic::LoFi::new()),
        "rectifier" => Box::new(harmonic::Rectifier::new()),
        "subharmonic" => Box::new(harmonic::SubHarmonic::new()),
        "pitch_shifter" => Box::new(pitch::PitchShifter::new()),
        "time_shift" => Box::new(utility::TimeShift::new()),
        "gain" => Box::new(utility::Gain::new()),
        "trim" => Box::new(utility::Trim::new()),
        "invert" => Box::new(utility::Invert::default()),
        "dc_offset_removal" => Box::new(utility::DcRemoval::new()),
        "signal_generator" => Box::new(utility::SignalGenerator::new()),
        "dither" => Box::new(utility::Dither::new()),
        "subtractive_synth" => Box::new(synth::SubtractiveSynth::new()),
        "drum_synth" => Box::new(drums::DrumSynth::new()),
        "sampler" => Box::new(sampler::Sampler::new()),
        _ => return None,
    })
}
