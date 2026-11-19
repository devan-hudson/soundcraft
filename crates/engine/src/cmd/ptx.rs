//! Opening Pro Tools sessions (`.ptx`) as new SoundCraft sessions.
//!
//! The file is read by [`crate::io::parse_ptx`]; this module turns what it found into tracks,
//! clips, fades, clip gain, fader and pan, markers and the tempo. Audio is looked for next to the
//! session, in its `Audio Files` folder, at the path Pro Tools stored, and in the folder above.

use super::{CommandSpec, always, bad, str_param};
use crate::io::{PtxSession, file_found, parse_ptx};
use crate::{Engine, Result, cmd};
use serde_json::{Value, json};
use soundcraft_model::{ChannelFormat, Clip, MarkerKind, Session, SourceId, TrackKind};
use soundcraft_time::{SampleRate, Samples};
use std::path::{Path, PathBuf};

const ID: &str = "file.open_pro_tools_session";

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(noundo "file.open_pro_tools_session", "Pro Tools Session (.ptx)...", ["File", "Import"], None, "{path}", always, |e, p| {
        let path = str_param(p, "path").ok_or_else(|| bad(ID, "`path` required"))?.to_string();
        open(e, &path)
    })]
}

/// True when `path` names a Pro Tools session by its extension.
pub(crate) fn is_ptx_path(path: &str) -> bool {
    Path::new(path).extension().is_some_and(|x| x.eq_ignore_ascii_case("ptx"))
}

/// Reads the `.ptx` at `path` and replaces the session with what it holds.
pub(crate) fn open(e: &mut Engine, path: &str) -> Result<Value> {
    let bytes = crate::io::read_file(path)?;
    let ptx = parse_ptx(&bytes).map_err(|err| bad(ID, format!("{path}: {err}")))?;
    let name = Path::new(path).file_stem().and_then(|s| s.to_str()).unwrap_or("Pro Tools Session").to_string();
    let dir = Path::new(path).parent().map(Path::to_path_buf).unwrap_or_default();
    build(e, &ptx, &name, &dir)
}

fn samples(v: u64) -> Samples {
    Samples::try_from(v).unwrap_or(Samples::MAX)
}

/// The audio file a clip named `region` plays: the file whose name it starts with (Pro Tools
/// names clips after their file, adding `-01`, `.L` and the like), else the only file.
fn file_for(ptx: &PtxSession, region: &str) -> Option<usize> {
    let stem = |n: &str| Path::new(n).file_stem().and_then(|s| s.to_str()).unwrap_or(n).to_ascii_lowercase();
    let r = region.to_ascii_lowercase();
    ptx.audio_files
        .iter()
        .enumerate()
        .filter(|(_, f)| r.starts_with(&stem(&f.name)))
        .max_by_key(|(_, f)| stem(&f.name).len())
        .map(|(i, _)| i)
        .or_else(|| (ptx.audio_files.len() == 1).then_some(0))
}

fn locate(dir: &Path, name: &str, stored: Option<&str>) -> Option<PathBuf> {
    let up = dir.parent().map(Path::to_path_buf).unwrap_or_default();
    let mut c = vec![dir.join(name), dir.join("Audio Files").join(name)];
    if let Some(s) = stored {
        c.push(PathBuf::from(s.replace('\\', std::path::MAIN_SEPARATOR_STR)));
        c.push(PathBuf::from(s));
    }
    c.push(up.join(name));
    c.push(up.join("Audio Files").join(name));
    c.into_iter().find(|p| file_found(p))
}

fn build(e: &mut Engine, ptx: &PtxSession, name: &str, dir: &Path) -> Result<Value> {
    let rate = SampleRate::new(ptx.sample_rate).map_err(|err| bad(ID, err.to_string()))?;
    let mut s = Session::new(name, rate);
    s.bit_depth = match ptx.bit_depth {
        16 => soundcraft_model::BitDepthSetting::Int16,
        32 => soundcraft_model::BitDepthSetting::Float32,
        _ => soundcraft_model::BitDepthSetting::Int24,
    };
    let bpm = ptx.bpm.unwrap_or(120.0);
    if ptx.bpm.is_some() {
        s.tempo.set_tempo(0, bpm).map_err(|err| bad(ID, err.to_string()))?;
    }
    e.replace_session(s);
    e.path = None;

    // Audio files, decoded once each.
    let mut missing = Vec::new();
    let mut sources: Vec<Option<(SourceId, Samples)>> = Vec::with_capacity(ptx.audio_files.len());
    for f in &ptx.audio_files {
        let found = locate(dir, &f.name, f.path.as_deref());
        let added = found.as_ref().and_then(|p| match crate::io::add_file_source(e, &p.to_string_lossy()) {
            Ok(id) => Some(id),
            Err(err) => {
                missing.push(format!("{}: {err}", f.name));
                None
            }
        });
        if found.is_none() {
            missing.push(format!("{}: not found", f.name));
        }
        let frames = added.and_then(|id| e.session().source(id)).map_or(0, |x| samples(x.frames));
        sources.push(added.map(|id| (id, frames)));
    }

    let mut clips = 0usize;
    let s = e.session_mut();
    for t in &ptx.tracks {
        let track = s.add_track(TrackKind::Audio, ChannelFormat::for_channels(t.channels.clamp(1, 2)), Some(&t.name));
        if let Some(tr) = s.track_mut(track) {
            if let Some(db) = t.volume_db {
                tr.mixer.volume_db = db;
            }
            if !t.pan.is_empty() {
                tr.mixer.pan = t.pan.clone();
            }
        }
        for c in &t.clips {
            let Some(region) = ptx.regions.get(c.region) else { continue };
            let Some((src, frames)) = file_for(ptx, &region.name).and_then(|i| sources.get(i).copied().flatten()) else { continue };
            let offset = samples(region.offset).min(frames);
            let length = samples(region.length).min(frames.saturating_sub(offset));
            if length <= 0 {
                continue;
            }
            let mut clip = Clip::audio(s.new_clip_id(), region.name.clone(), src, offset, samples(c.start), length);
            clip.gain_db = region.gain_db.clamp(-144.0, 36.0);
            clip.fade_in.len = samples(c.fade_in).min(length);
            clip.fade_out.len = samples(c.fade_out).min(length);
            clip.clamp_fades();
            crate::edit::place_clip(s, track, clip);
            clips += 1;
        }
    }
    let per_quarter = 60.0 / bpm * f64::from(ptx.sample_rate);
    for m in &ptx.markers {
        let at = (m.quarters * per_quarter).round();
        if at.is_finite() && (0.0..9.0e15).contains(&at) {
            s.add_marker(&m.name, MarkerKind::Marker, at as Samples, at as Samples);
        }
    }
    s.folder = Some(dir.to_path_buf());
    Ok(json!({
        "name": s.name,
        "sample_rate": ptx.sample_rate,
        "tracks": ptx.tracks.len(),
        "clips": clips,
        "markers": ptx.markers.len(),
        "bpm": ptx.bpm,
        "missing": missing,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::io::{PtxAudioFile, PtxClip, PtxMarker, PtxRegion, PtxTrack};

    fn tone(dir: &Path) {
        let buf = craft_audio::AudioBuffer { sample_rate: 48_000, channels: vec![vec![0.25; 48_000]] };
        let bytes = craft_audio::encode(&buf, &craft_audio::EncodeOptions::default()).unwrap();
        std::fs::write(dir.join("tone.wav"), bytes).unwrap();
    }

    #[test]
    fn builds_tracks_clips_mixer_markers_and_tempo() {
        let dir = std::env::temp_dir().join(format!("sc-ptx-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        tone(&dir);
        let ptx = PtxSession {
            sample_rate: 48_000,
            bit_depth: 24,
            bpm: Some(140.0),
            audio_files: vec![PtxAudioFile { name: "tone.wav".into(), path: None }],
            regions: vec![PtxRegion { name: "tone-01".into(), offset: 0, length: 48_000, gain_db: -6.0 }],
            tracks: vec![PtxTrack {
                name: "Kick".into(),
                channels: 1,
                volume_db: Some(-10.0),
                pan: vec![-1.0],
                clips: vec![PtxClip { region: 0, start: 96_000, fade_in: 4800, fade_out: 9600 }],
            }],
            markers: vec![PtxMarker { name: "Verse".into(), quarters: 7.0 }],
        };
        let mut e = Engine::new(Session::default());
        let r = build(&mut e, &ptx, "Song", &dir).unwrap();
        assert_eq!(r["missing"], json!([]));
        let s = e.session();
        let t = s.tracks.iter().find(|t| t.name == "Kick").unwrap();
        assert_eq!((t.mixer.volume_db, t.mixer.pan.clone()), (-10.0, vec![-1.0]));
        let c = &t.playlists[t.active_playlist].clips[0];
        assert_eq!((c.start, c.length, c.gain_db, c.fade_in.len, c.fade_out.len), (96_000, 48_000, -6.0, 4800, 9600));
        assert_eq!(s.markers[0].start, (7.0 * 60.0 / 140.0 * 48_000.0f64).round() as Samples);
        assert_eq!(s.tempo.tempos()[0].bpm, 140.0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_audio_file_is_reported_not_fatal() {
        let ptx = PtxSession {
            sample_rate: 48_000,
            audio_files: vec![PtxAudioFile { name: "gone.wav".into(), path: None }],
            regions: vec![PtxRegion { name: "gone".into(), offset: 0, length: 10, gain_db: 0.0 }],
            tracks: vec![PtxTrack { name: "A".into(), channels: 1, clips: vec![PtxClip { region: 0, ..Default::default() }], ..Default::default() }],
            ..Default::default()
        };
        let mut e = Engine::new(Session::default());
        let r = build(&mut e, &ptx, "S", Path::new("/nonexistent-dir-for-test")).unwrap();
        assert_eq!(r["clips"], json!(0));
        assert_eq!(r["missing"].as_array().map(Vec::len), Some(1));
    }

    #[test]
    fn open_refuses_a_file_that_is_not_ptx() {
        let dir = std::env::temp_dir().join(format!("sc-ptx-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("x.ptx");
        std::fs::write(&f, b"hello").unwrap();
        let mut e = Engine::new(Session::default());
        assert!(e.execute(ID, &json!({"path": f.to_string_lossy()})).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }
}
