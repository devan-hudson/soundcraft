//! Reading Pro Tools session files (`.ptx`), clean-room.
//!
//! Everything here was worked out by comparing small sessions saved by Pro Tools itself, one
//! change at a time (an empty session, one track, a renamed track, a clip, the clip moved, fades,
//! clip gain, fader and pan, a stereo track and a marker, a tempo change). No other program's
//! reader was used. What is known of the layout:
//!
//! - The file starts with a 0x03 byte and the ASCII text `0010111100101011`.
//! - Past the first 4096 bytes every byte is XORed with `page × k` (mod 256), where `page` is
//!   the byte's offset / 4096 and `k` is one number per file. `k` is found by trying each value
//!   until the block walk below covers the whole file.
//! - From offset 0x14 the file is a tree of blocks: `0x5a`, a `u16` kind, a `u32` size, then
//!   `size` bytes that begin with a `u16` content type. Child blocks sit inside a parent's bytes.
//!
//! The reader is a pure function of the bytes: it never panics, bounds its recursion and the
//! number of blocks, and treats every length as hostile.

use std::collections::BTreeMap;

/// The magic text after the first byte.
const MAGIC: &[u8; 16] = b"0010111100101011";
/// Where the block tree starts.
const TREE_START: usize = 0x14;
/// Deepest nesting followed.
const MAX_DEPTH: usize = 48;
/// Most blocks read from one file.
const MAX_BLOCKS: usize = 2_000_000;
/// Pro Tools' timeline ticks per quarter note, and the tick that is bar 1 beat 1.
const TICKS_PER_QUARTER: f64 = 960_000.0;
const TICK_ZERO: u64 = 1_000_000_000_000;

/// What a `.ptx` file says, in its own terms (sample positions at the session rate).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PtxSession {
    pub sample_rate: u32,
    pub bit_depth: u16,
    /// The first tempo, when the session has one.
    pub bpm: Option<f64>,
    /// Audio files the session uses, in its own order.
    pub audio_files: Vec<PtxAudioFile>,
    /// Clips ("regions") the session defines.
    pub regions: Vec<PtxRegion>,
    pub tracks: Vec<PtxTrack>,
    pub markers: Vec<PtxMarker>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PtxAudioFile {
    pub name: String,
    /// Where the file was when the session was saved, when the components could be read.
    pub path: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PtxRegion {
    pub name: String,
    /// Offset into the audio file, samples.
    pub offset: u64,
    pub length: u64,
    pub gain_db: f32,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PtxTrack {
    pub name: String,
    pub channels: usize,
    pub volume_db: Option<f32>,
    /// Pan per channel, −1 (left) to 1 (right).
    pub pan: Vec<f32>,
    pub clips: Vec<PtxClip>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PtxClip {
    /// Index into [`PtxSession::regions`].
    pub region: usize,
    /// Timeline start, samples.
    pub start: u64,
    pub fade_in: u64,
    pub fade_out: u64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PtxMarker {
    pub name: String,
    /// Position in quarter notes from the session start.
    pub quarters: f64,
}

/// Why a file could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtxError {
    NotPtx,
    Garbled,
}

impl std::fmt::Display for PtxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PtxError::NotPtx => f.write_str("not a Pro Tools session file (.ptx)"),
            PtxError::Garbled => f.write_str("the Pro Tools session could not be decoded"),
        }
    }
}

/// True when `bytes` start like a `.ptx` file.
pub fn is_ptx(bytes: &[u8]) -> bool {
    bytes.first() == Some(&0x03) && bytes.get(1..17) == Some(MAGIC.as_slice())
}

/// Reads a `.ptx` file.
pub fn parse(bytes: &[u8]) -> Result<PtxSession, PtxError> {
    if !is_ptx(bytes) {
        return Err(PtxError::NotPtx);
    }
    let plain = decode(bytes).ok_or(PtxError::Garbled)?;
    let tree = Tree::read(&plain);
    Ok(tree.session())
}

// ─── Unscrambling ───────────────────────────────────────────────────────────────────────────

fn unscramble(bytes: &[u8], k: u8) -> Vec<u8> {
    bytes
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let page = (i >> 12) as u64;
            b ^ ((page.wrapping_mul(u64::from(k))) & 0xff) as u8
        })
        .collect()
}

/// The file with its scrambling undone: the `k` whose top-level block walk ends exactly at the
/// end of the file. Files no longer than a page need no key.
fn decode(bytes: &[u8]) -> Option<Vec<u8>> {
    if covers(bytes) {
        return Some(bytes.to_vec());
    }
    // Most bytes in a session are zero: try keys in order of how many zeros they make of page 1.
    let page = bytes.get(4096..bytes.len().min(8192))?;
    let mut keys: Vec<(usize, u8)> = (1..=255u8)
        // Page 1 is XORed with k itself, so its zeros read as k.
        .map(|k| (page.iter().filter(|b| **b == k).count(), k))
        .collect();
    keys.sort_by_key(|k| std::cmp::Reverse(k.0));
    keys.into_iter().map(|(_, k)| unscramble(bytes, k)).find(|plain| covers(plain))
}

/// True when the top-level blocks run back to back from [`TREE_START`] to the end of `d`.
fn covers(d: &[u8]) -> bool {
    let mut i = TREE_START;
    let mut n = 0usize;
    while i < d.len() {
        match header(d, i, d.len()) {
            Some((_, end)) => i = end,
            None => return false,
        }
        n += 1;
        if n > MAX_BLOCKS {
            return false;
        }
    }
    i == d.len() && n > 0
}

/// The block at `i` within `..end`: its content type and where it ends.
fn header(d: &[u8], i: usize, end: usize) -> Option<(u16, usize)> {
    if *d.get(i)? != 0x5a {
        return None;
    }
    let size = u32_at(d, i.checked_add(3)?)? as usize;
    let stop = i.checked_add(7)?.checked_add(size)?;
    if size < 2 || stop > end {
        return None;
    }
    Some((u16_at(d, i.checked_add(7)?)?, stop))
}

// ─── The block tree ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
struct Node {
    ct: u16,
    /// Bytes after the content type.
    body: (usize, usize),
    parent: Option<usize>,
}

struct Tree<'a> {
    d: &'a [u8],
    nodes: Vec<Node>,
}

impl<'a> Tree<'a> {
    fn read(d: &'a [u8]) -> Tree<'a> {
        let mut t = Tree { d, nodes: Vec::new() };
        t.walk(TREE_START, d.len(), None, 0);
        t
    }

    fn walk(&mut self, start: usize, end: usize, parent: Option<usize>, depth: usize) {
        if depth > MAX_DEPTH {
            return;
        }
        let mut i = start;
        while i.saturating_add(9) <= end && self.nodes.len() < MAX_BLOCKS {
            let Some((ct, stop)) = header(self.d, i, end) else {
                i += 1;
                continue;
            };
            let idx = self.nodes.len();
            self.nodes.push(Node { ct, body: (i + 9, stop), parent });
            self.walk(i + 9, stop, Some(idx), depth + 1);
            i = stop;
        }
    }

    fn body(&self, n: &Node) -> &'a [u8] {
        self.d.get(n.body.0..n.body.1).unwrap_or(&[])
    }

    fn all(&self, ct: u16) -> impl Iterator<Item = (usize, &Node)> + '_ {
        self.nodes.iter().enumerate().filter(move |(_, n)| n.ct == ct)
    }

    fn parent_ct(&self, n: &Node) -> Option<u16> {
        n.parent.and_then(|p| self.nodes.get(p)).map(|p| p.ct)
    }

    fn depth(&self, n: &Node) -> usize {
        let mut d = 0;
        let mut p = n.parent;
        while let Some(i) = p {
            d += 1;
            p = self.nodes.get(i).and_then(|x| x.parent);
            if d > MAX_DEPTH {
                break;
            }
        }
        d
    }

    fn session(&self) -> PtxSession {
        let mut s = PtxSession { sample_rate: 48_000, bit_depth: 24, ..Default::default() };
        if let Some((_, n)) = self.all(0x1028).next() {
            let b = self.body(n);
            if let Some(rate) = u32_at(b, 2).filter(|r| (8_000..=768_000).contains(r)) {
                s.sample_rate = rate;
            }
            if let Some(&bits) = b.get(1).filter(|b| matches!(**b, 16 | 24 | 32)) {
                s.bit_depth = u16::from(bits);
            }
        }
        s.bpm = self.tempo();
        s.audio_files = self.audio_files();
        s.regions = self.regions();
        s.tracks = self.tracks(&s.regions);
        s.markers = self.markers();
        s
    }

    fn tempo(&self) -> Option<f64> {
        self.all(0x2028).find_map(|(_, n)| {
            let b = self.body(n);
            // The tempo list: the first event's tick (bar 1 is TICK_ZERO), two bytes, its bpm.
            let at = find(b, b"TMS")?;
            let after = b.get(at..)?;
            let tick = find(after, &TICK_ZERO.to_le_bytes())?;
            (8..=16).find_map(|k| f64_at(after, tick.checked_add(k)?).filter(|bpm| bpm.is_finite() && (1.0..=999.0).contains(bpm)))
        })
    }

    fn audio_files(&self) -> Vec<PtxAudioFile> {
        let mut out = Vec::new();
        for (_, n) in self.all(0x103a) {
            let b = self.body(n);
            // Length-prefixed names; a file name is followed by its four-letter type.
            let strings = strings_in(b);
            for (k, (end, name)) in strings.iter().enumerate() {
                let kind = b.get(*end..end.saturating_add(4)).unwrap_or(&[]);
                if !matches!(kind, b"EVAW" | b"FFIA" | b"WAVE" | b"AIFF" | b"CFIA") {
                    continue;
                }
                // The folder holding the file comes just before it; the root and the folders
                // leading down to that one follow it.
                let dir = k.checked_sub(1).and_then(|j| strings.get(j)).map(|(_, s)| s.clone());
                let rest: Vec<&str> = strings.iter().skip(k + 1).map(|(_, s)| s.as_str()).collect();
                let path = match (dir, rest.split_first()) {
                    (Some(dir), Some((root, mid))) if root.ends_with('\\') || root.ends_with('/') || root.ends_with(':') => {
                        let sep = if root.contains('\\') { "\\" } else { "/" };
                        let mut p = root.to_string();
                        for part in mid.iter().chain(std::iter::once(&dir.as_str())) {
                            if !p.ends_with(sep) {
                                p.push_str(sep);
                            }
                            p.push_str(part);
                        }
                        p.push_str(sep);
                        p.push_str(name);
                        Some(p)
                    }
                    _ => None,
                };
                if !out.iter().any(|f: &PtxAudioFile| f.name == *name) {
                    out.push(PtxAudioFile { name: name.clone(), path });
                }
            }
        }
        out
    }

    fn gains(&self) -> Vec<f32> {
        let Some((_, n)) = self.all(0x2637).next() else { return Vec::new() };
        let b = self.body(n);
        let count = u32_at(b, 0).unwrap_or(0).min(100_000) as usize;
        let mut out = Vec::new();
        let mut i = 4usize;
        for _ in 0..count {
            let Some(len) = u32_at(b, i + 4).map(|l| l as usize) else { break };
            let start = i + 8;
            let Some(end) = start.checked_add(len).filter(|e| *e <= b.len()) else { break };
            out.push(end.checked_sub(4).and_then(|g| f32_at(b, g)).filter(|g| g.is_finite()).unwrap_or(0.0));
            i = end;
        }
        out
    }

    fn regions(&self) -> Vec<PtxRegion> {
        let gains = self.gains();
        let Some((list, _)) = self.all(0x262a).next() else { return Vec::new() };
        let mut out = Vec::new();
        for (_, n) in self.descendants(list) {
            if n.ct != 0x2628 {
                continue;
            }
            let b = self.body(n);
            let Some((name, end)) = string_at(b, 0) else { continue };
            let (offset, length) = packed(b, end + 2).unwrap_or((0, 0));
            let gain_db = b.len().checked_sub(22).and_then(|g| u32_at(b, g)).and_then(|g| gains.get(g as usize).copied()).unwrap_or(0.0);
            out.push(PtxRegion { name, offset, length, gain_db });
        }
        out
    }

    fn is_inside(&self, mut i: usize, ancestor: usize) -> bool {
        for _ in 0..=MAX_DEPTH {
            match self.nodes.get(i).and_then(|n| n.parent) {
                Some(p) if p == ancestor => return true,
                Some(p) => i = p,
                None => return false,
            }
        }
        false
    }

    /// The blocks inside block `idx`, in file order (they follow it contiguously).
    fn descendants(&self, idx: usize) -> impl Iterator<Item = (usize, &Node)> + '_ {
        self.nodes.iter().enumerate().skip(idx.saturating_add(1)).take_while(move |(k, _)| self.is_inside(*k, idx))
    }

    /// Fade lengths, by index.
    fn fades(&self) -> Vec<u64> {
        self.all(0x262f)
            .map(|(_, n)| {
                let b = self.body(n);
                // An empty name, one byte, then the packed fields.
                let len = u32_at(b, 0).unwrap_or(0) as usize;
                packed(b, 4usize.saturating_add(len).saturating_add(1)).map_or(0, |(_, l)| l)
            })
            .collect()
    }

    fn tracks(&self, regions: &[PtxRegion]) -> Vec<PtxTrack> {
        let fades = self.fades();
        let mut tracks: Vec<PtxTrack> = self
            .all(0x1014)
            .filter(|(_, n)| self.parent_ct(n) == Some(0x1015))
            .filter_map(|(_, n)| {
                let b = self.body(n);
                let (name, end) = string_at(b, 0)?;
                let channels = u32_at(b, end + 1).filter(|c| (1..=64).contains(c)).unwrap_or(1) as usize;
                Some(PtxTrack { name, channels, ..Default::default() })
            })
            .collect();

        // Playlists, by track name: the clips and fades on each.
        let mut playlists: BTreeMap<String, Vec<PtxClip>> = BTreeMap::new();
        for (idx, n) in self.all(0x1052) {
            let parent = n.parent.and_then(|p| self.nodes.get(p));
            if parent.map(|p| p.ct) != Some(0x1054) || parent.is_some_and(|p| self.depth(p) != 0) {
                continue;
            }
            let Some((name, _)) = string_at(self.body(n), 0) else { continue };
            if playlists.contains_key(&name) {
                continue;
            }
            let mut clips = Vec::new();
            let mut fade_marks = Vec::new();
            for (_, m) in self.descendants(idx) {
                if m.ct != 0x104f {
                    continue;
                }
                let b = self.body(m);
                let (Some(index), Some(start), Some(&kind)) = (u32_at(b, 2), u64_at(b, 7), b.get(15)) else { continue };
                match kind {
                    3 if (index as usize) < regions.len() => clips.push(PtxClip { region: index as usize, start, fade_in: 0, fade_out: 0 }),
                    1 => fade_marks.push((start, fades.get(index as usize).copied().unwrap_or(0))),
                    _ => {}
                }
            }
            for (at, len) in fade_marks {
                // A fade at a clip's start is its fade-in; one at (or near) its end, its fade-out.
                if let Some(c) = clips.iter_mut().find(|c| c.start == at) {
                    c.fade_in = len;
                } else if let Some(c) = clips.iter_mut().find(|c| {
                    let end = c.start.saturating_add(regions.get(c.region).map_or(0, |r| r.length));
                    at == end || at.saturating_add(len) == end
                }) {
                    c.fade_out = len;
                }
            }
            playlists.insert(name, clips);
        }
        for t in &mut tracks {
            t.clips = playlists.remove(&t.name).unwrap_or_default();
        }

        // Mixer strips, in track order: fader in tenths of a dB, pans in hundredths.
        let strips: Vec<usize> = self.all(0x261b).map(|(i, _)| i).collect();
        for (t, strip) in tracks.iter_mut().zip(strips) {
            let Some((_, n)) = self.descendants(strip).find(|(_, n)| n.ct == 0x1029) else { continue };
            let b = self.body(n);
            let int = |at: usize| u32_at(b, at).map(|v| v as i32);
            t.volume_db = int(1).map(|v| v as f32 / 10.0).filter(|v| (-200.0..=24.0).contains(v));
            let pan = |v: i32| (v as f32 / 100.0).clamp(-1.0, 1.0);
            t.pan =
                if t.channels == 2 { [int(17), int(21)].into_iter().flatten().map(pan).collect() } else { int(13).map(pan).into_iter().collect() };
        }
        tracks
    }

    fn markers(&self) -> Vec<PtxMarker> {
        self.all(0x2077)
            .filter_map(|(_, n)| {
                let b = self.body(n);
                let (name, end) = string_at(b, 6)?;
                let tick = u64_at(b, end)? & 0x0000_ffff_ffff_ffff;
                let quarters = tick.checked_sub(TICK_ZERO)? as f64 / TICKS_PER_QUARTER;
                Some(PtxMarker { name, quarters })
            })
            .collect()
    }
}

// ─── Field helpers ──────────────────────────────────────────────────────────────────────────

fn u16_at(d: &[u8], i: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(i..i.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(d: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(i..i.checked_add(4)?)?.try_into().ok()?))
}

fn u64_at(d: &[u8], i: usize) -> Option<u64> {
    Some(u64::from_le_bytes(d.get(i..i.checked_add(8)?)?.try_into().ok()?))
}

fn f32_at(d: &[u8], i: usize) -> Option<f32> {
    u32_at(d, i).map(f32::from_bits)
}

fn f64_at(d: &[u8], i: usize) -> Option<f64> {
    u64_at(d, i).map(f64::from_bits)
}

/// Little-endian unsigned number of `n` bytes (at most 8).
fn uint(d: &[u8], i: usize, n: usize) -> Option<u64> {
    let bytes = d.get(i..i.checked_add(n.min(8))?)?;
    Some(bytes.iter().rev().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A `u32` length and that many bytes of text at `i`: the text and where it ends.
fn string_at(d: &[u8], i: usize) -> Option<(String, usize)> {
    let len = u32_at(d, i)? as usize;
    if len > 4096 {
        return None;
    }
    let start = i.checked_add(4)?;
    let end = start.checked_add(len)?;
    let text = d.get(start..end)?;
    Some((String::from_utf8_lossy(text).into_owned(), end))
}

/// Every plausible length-prefixed name in `d`, with where each ends.
fn strings_in(d: &[u8]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + 4 < d.len() {
        if let Some(len) = u32_at(d, i).map(|l| l as usize).filter(|l| (1..=255).contains(l))
            && let Some(text) = d.get(i + 4..i + 4 + len)
            && text.iter().all(|b| (0x20..0x7f).contains(b) || *b >= 0x80)
            && std::str::from_utf8(text).is_ok()
        {
            out.push((i + 4 + len, String::from_utf8_lossy(text).into_owned()));
            i += 4 + len;
            continue;
        }
        i += 1;
    }
    out
}

/// Packed numbers: three bytes whose nibbles give the byte counts of the fields that follow
/// (low nibble first). Returns the first two fields: (offset, length).
fn packed(d: &[u8], at: usize) -> Option<(u64, u64)> {
    let head = d.get(at..at.checked_add(3)?)?;
    let mut i = at + 3;
    let mut fields = [0u64; 6];
    for (k, nib) in head.iter().flat_map(|b| [b & 0x0f, b >> 4]).enumerate() {
        let n = usize::from(nib);
        if n > 8 {
            return None;
        }
        if let Some(f) = fields.get_mut(k) {
            *f = uint(d, i, n)?;
        }
        i += n;
    }
    Some((fields[0], fields[1]))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A block: marker, kind 1, size, content type, body.
    fn block(ct: u16, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0x5a, 1, 0];
        v.extend(((body.len() + 2) as u32).to_le_bytes());
        v.extend(ct.to_le_bytes());
        v.extend(body);
        v
    }

    fn file(blocks: &[Vec<u8>], k: u8) -> Vec<u8> {
        let mut d = vec![0x03];
        d.extend(MAGIC);
        d.extend([0, 5, 0]);
        for b in blocks {
            d.extend(b);
        }
        unscramble(&d, k)
    }

    fn lp(s: &str) -> Vec<u8> {
        let mut v = (s.len() as u32).to_le_bytes().to_vec();
        v.extend(s.as_bytes());
        v
    }

    #[test]
    fn refuses_other_files() {
        assert_eq!(parse(b"RIFF....WAVE"), Err(PtxError::NotPtx));
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn hostile_bytes_never_panic() {
        let mut d = vec![0x03];
        d.extend(MAGIC);
        for seed in 0..64u32 {
            let mut x = seed.wrapping_mul(2_654_435_761);
            let mut f = d.clone();
            for _ in 0..9000 {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                f.push(if x % 7 == 0 { 0x5a } else { x as u8 });
            }
            let _ = parse(&f);
        }
    }

    #[test]
    fn unscrambles_pages_past_the_first() {
        // Enough filler to reach page 2, so the key matters.
        let filler = block(0x9999, &vec![0u8; 9000]);
        let tempo = {
            let mut b = b"Tempo".to_vec();
            b.extend(b"TMS");
            b.extend([1, 0, 0x14, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0]);
            b.extend(TICK_ZERO.to_le_bytes());
            b.extend([0, 1]);
            b.extend(140.0f64.to_le_bytes());
            block(0x2028, &b)
        };
        let d = file(&[filler, tempo], 0x1c);
        let s = parse(&d).unwrap();
        assert_eq!(s.bpm, Some(140.0));
    }

    #[test]
    fn reads_tracks_clips_fades_gain_mixer_and_markers() {
        // Session rate.
        let rate = block(0x1028, &[3, 24, 0x80, 0xbb, 0, 0]);
        // Two tracks: mono Kick, stereo Pad.
        let mut kick = lp("Kick");
        kick.extend([0, 1, 0, 0, 0]);
        let mut pad = lp("Pad");
        pad.extend([1, 2, 0, 0, 0]);
        let tracks = block(0x1015, &[block(0x1014, &kick), block(0x1014, &pad)].concat());
        // One region, 48000 long, with clip gain entry 0.
        let mut region = lp("tone");
        region.extend([0, 0, 0x20, 0, 0, 0x80, 0xbb]);
        region.extend([0u8; 8]);
        region.extend(0u32.to_le_bytes());
        region.extend([0u8; 18]);
        let regions = block(0x262a, &block(0x2629, &block(0x2628, &region)));
        let mut gain = 1u32.to_le_bytes().to_vec();
        gain.extend([1, 0x46, 1, 0]);
        gain.extend(8u32.to_le_bytes());
        gain.extend([0u8; 4]);
        gain.extend((-6.0f32).to_le_bytes());
        let gains = block(0x2637, &gain);
        // One fade, 4800 long.
        let mut fade = vec![0u8; 5];
        fade.extend([0x20, 0, 0, 0xc0, 0x12, 3, 2]);
        let fades = block(0x2630, &block(0x262f, &fade));
        // Kick's playlist: the region at bar 3 (192000) with the fade-in at its start.
        let place = |idx: u32, start: u64, kind: u8| {
            let mut b = vec![0, 0];
            b.extend(idx.to_le_bytes());
            b.push(0);
            b.extend(start.to_le_bytes());
            b.push(kind);
            b.extend([0xfe, 0xff]);
            block(0x1050, &block(0x104f, &b))
        };
        let mut pl = lp("Kick");
        pl.extend(2u32.to_le_bytes());
        pl.extend(place(0, 192_000, 3));
        pl.extend(place(0, 192_000, 1));
        let playlists = block(0x1054, &block(0x1052, &pl));
        // Mixer: Kick at -11.4 dB panned hard left.
        let strip = |vol: i32, pan: [i32; 3]| {
            let mut b = vec![1];
            b.extend(vol.to_le_bytes());
            b.extend(0i32.to_le_bytes());
            b.extend((-1440i32).to_le_bytes());
            for p in pan {
                b.extend(p.to_le_bytes());
            }
            block(0x261b, &block(0x260d, &block(0x1029, &b)))
        };
        let mixer = block(0x2624, &[strip(-114, [-100, 0, 100]), strip(0, [-100, -100, 100])].concat());
        // A marker at bar 5 (16 quarters).
        let mut m = vec![1, 0, 1, 9, 0, 0];
        m.extend(lp("Verse"));
        m.extend(((TICK_ZERO + 16 * 960_000) | 0x4000_0000_0000_0000).to_le_bytes());
        let markers = block(0x2030, &block(0x2077, &m));
        let filler = block(0x9999, &vec![0u8; 5000]);
        let d = file(&[rate, tracks, regions, gains, fades, playlists, mixer, markers, filler], 0xbe);
        let s = parse(&d).unwrap();
        assert_eq!((s.sample_rate, s.bit_depth), (48_000, 24));
        assert_eq!(s.regions, vec![PtxRegion { name: "tone".into(), offset: 0, length: 48_000, gain_db: -6.0 }]);
        assert_eq!(s.tracks.len(), 2);
        let k = &s.tracks[0];
        assert_eq!((k.name.as_str(), k.channels), ("Kick", 1));
        assert_eq!(k.clips, vec![PtxClip { region: 0, start: 192_000, fade_in: 4800, fade_out: 0 }]);
        assert_eq!(k.volume_db, Some(-11.4));
        assert_eq!(k.pan, vec![-1.0]);
        assert_eq!((s.tracks[1].channels, s.tracks[1].pan.clone()), (2, vec![-1.0, 1.0]));
        assert_eq!(s.markers, vec![PtxMarker { name: "Verse".into(), quarters: 16.0 }]);
    }
}
