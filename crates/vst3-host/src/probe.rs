//! Out-of-process VST3 scanning with a disk cache.
//!
//! Loading a plugin binary runs its static initializers, and some of them hang (waiting on a
//! licence dialog, a network check or a driver), crash or fail. Scanned in-process, one such
//! plugin stalls or kills the whole scan. Here every bundle is probed in a child process (the
//! host program itself, started with [`PROBE_FLAG`]) with a time limit, several at a time, and
//! each result, good or bad, is cached on disk keyed by the binary's size and modification time.
//! A later scan reads unchanged bundles from the cache and probes only new or changed ones, so a
//! bad plugin costs its timeout once, not on every scan. [`crate::rescan`] probes failed bundles
//! again.
//!
//! Without a configured probe program ([`configure`]), bundles are loaded in-process as before.

use crate::{Vst3Descriptor, Vst3Error, scan as paths};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant, UNIX_EPOCH};

/// The argument that makes a host program probe one bundle and exit (see [`run_probe_if_requested`]).
pub const PROBE_FLAG: &str = "--vst3-probe";
/// Prefix of the one stdout line that carries the probe's result (plugins may print their own lines).
const MARKER: &str = "SOUNDCRAFT-VST3-PROBE ";
/// Version of the cache file layout; a file of another version is ignored.
const CACHE_VERSION: u32 = 1;
/// Default time one bundle may take to load and list its classes.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Most child processes probing at once.
const MAX_WORKERS: usize = 8;
/// How long a child that has answered may take to exit before it is killed (module teardown can hang).
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// What probing one bundle found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeResult {
    Ok(Vec<Vst3Descriptor>),
    Err(String),
}

/// A bundle that could not be scanned, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScanFailure {
    pub path: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct CacheEntry {
    size: u64,
    modified: u64,
    result: ProbeResult,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct CacheFile {
    version: u32,
    entries: BTreeMap<String, CacheEntry>,
}

#[derive(Debug, Clone)]
struct Config {
    cache_file: Option<PathBuf>,
    probe_program: Option<PathBuf>,
    timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Config { cache_file: None, probe_program: None, timeout: DEFAULT_TIMEOUT }
    }
}

fn config() -> &'static Mutex<Config> {
    static C: OnceLock<Mutex<Config>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Config::default()))
}

/// Sets where scan results are cached (`None`: no cache) and which program probes bundles out of
/// process (`None`: load them in-process). The program must call [`run_probe_if_requested`] first
/// thing in `main`; normally it is the host program itself (`std::env::current_exe()`).
pub fn configure(cache_file: Option<PathBuf>, probe_program: Option<PathBuf>) {
    let mut c = config().lock().unwrap_or_else(PoisonError::into_inner);
    c.cache_file = cache_file;
    c.probe_program = probe_program;
}

/// Sets the time one bundle may take before its probe is abandoned and the bundle recorded as failed.
pub fn set_timeout(timeout: Duration) {
    config().lock().unwrap_or_else(PoisonError::into_inner).timeout = timeout.max(Duration::from_millis(100));
}

fn snapshot() -> Config {
    config().lock().unwrap_or_else(PoisonError::into_inner).clone()
}

// --- progress -------------------------------------------------------------------------------

static RUNNING: AtomicBool = AtomicBool::new(false);
static DONE: AtomicUsize = AtomicUsize::new(0);
static TOTAL: AtomicUsize = AtomicUsize::new(0);

/// `(done, total)` bundles of the scan in progress, or `None` when no scan runs.
pub fn progress() -> Option<(usize, usize)> {
    RUNNING.load(Ordering::Acquire).then(|| (DONE.load(Ordering::Relaxed), TOTAL.load(Ordering::Relaxed)))
}

struct Running;
impl Running {
    fn start(total: usize) -> Running {
        DONE.store(0, Ordering::Relaxed);
        TOTAL.store(total, Ordering::Relaxed);
        RUNNING.store(true, Ordering::Release);
        Running
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::Release);
    }
}

// --- child side -----------------------------------------------------------------------------

/// Call first thing in `main` of every program passed to [`configure`]. When the arguments are
/// `<PROBE_FLAG> <bundle>`, loads that bundle, prints its classes on one marked line and returns
/// the exit code to end the process with; otherwise returns `None` and does nothing.
pub fn run_probe_if_requested() -> Option<i32> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != PROBE_FLAG {
        return None;
    }
    let Some(path) = args.next() else {
        eprintln!("{PROBE_FLAG} needs a .vst3 path");
        return Some(2);
    };
    let result = match crate::registry::load_bundle(Path::new(&path)) {
        Ok(ds) => ProbeResult::Ok(ds),
        Err(e) => ProbeResult::Err(short_reason(&e)),
    };
    let line = serde_json::to_string(&result).unwrap_or_else(|e| format!("{{\"err\":\"cannot encode the result: {e}\"}}"));
    let mut out = std::io::stdout().lock();
    let written = writeln!(out, "{MARKER}{line}").and_then(|()| out.flush());
    Some(i32::from(written.is_err()))
}

/// The error without the "cannot load VST3 plugin <path>:" prefix (the cache is keyed by path).
fn short_reason(e: &Vst3Error) -> String {
    match e {
        Vst3Error::Load(_, why) => why.clone(),
        other => other.to_string(),
    }
}

// --- parent side ----------------------------------------------------------------------------

/// Probes one bundle in a child process running `program`, giving up after `timeout`.
fn probe_out_of_process(program: &Path, bundle: &Path, timeout: Duration) -> ProbeResult {
    let mut cmd = Command::new(program);
    cmd.arg(PROBE_FLAG).arg(bundle).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: a console host program would otherwise flash a console per probe.
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return ProbeResult::Err(format!("cannot start the scanner process: {e}")),
    };
    let (tx, rx) = mpsc::channel::<String>();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if let Some(json) = line.strip_prefix(MARKER) {
                    let _ = tx.send(json.to_string());
                    return;
                }
            }
        });
    }
    let answer = rx.recv_timeout(timeout);
    // Reap the child: one that answered gets a moment to exit on its own; any other is killed.
    let grace = if answer.is_ok() { EXIT_GRACE } else { Duration::ZERO };
    let deadline = Instant::now() + grace;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    match answer {
        Ok(json) => serde_json::from_str(&json).unwrap_or_else(|e| ProbeResult::Err(format!("unreadable scanner answer: {e}"))),
        Err(RecvTimeoutError::Timeout) => ProbeResult::Err(format!("timed out after {} s (the plugin hung while loading)", timeout.as_secs())),
        Err(RecvTimeoutError::Disconnected) => ProbeResult::Err(match status {
            Some(s) => format!("the plugin crashed while loading ({s})"),
            None => "the plugin crashed while loading".to_string(),
        }),
    }
}

fn probe_in_process(bundle: &Path) -> ProbeResult {
    match crate::registry::load_bundle(bundle) {
        Ok(ds) => ProbeResult::Ok(ds),
        Err(e) => ProbeResult::Err(short_reason(&e)),
    }
}

/// Size and modification time of the file that identifies a bundle's version: its binary when
/// it resolves, else the path itself.
fn stamp(bundle: &Path) -> Option<(u64, u64)> {
    let file = paths::bundle_binary(bundle).unwrap_or_else(|| bundle.to_path_buf());
    let md = std::fs::metadata(file).ok()?;
    let modified = md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some((md.len(), modified))
}

fn load_cache(file: &Path) -> CacheFile {
    let Ok(text) = std::fs::read_to_string(file) else { return CacheFile::default() };
    match serde_json::from_str::<CacheFile>(&text) {
        Ok(c) if c.version == CACHE_VERSION => c,
        Ok(_) => CacheFile::default(),
        Err(e) => {
            log::warn!("ignoring the VST3 scan cache {}: {e}", file.display());
            CacheFile::default()
        }
    }
}

fn save_cache(file: &Path, cache: &CacheFile) {
    let write = || -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = serde_json::to_string(cache).map_err(std::io::Error::other)?;
        let tmp = file.with_extension("tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, file)
    };
    if let Err(e) = write() {
        log::warn!("cannot write the VST3 scan cache {}: {e}", file.display());
    }
}

fn key(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Probes every bundle, using the cache where the bundle has not changed (and, unless
/// `retry_failed`, where it failed before). Returns the results in `bundles` order.
pub(crate) fn probe_all(bundles: &[PathBuf], retry_failed: bool) -> Vec<ProbeResult> {
    let cfg = snapshot();
    let mut cache = cfg.cache_file.as_deref().map(load_cache).unwrap_or_default();
    let _running = Running::start(bundles.len());
    let mut results: Vec<Option<ProbeResult>> = vec![None; bundles.len()];
    let mut stamps: Vec<Option<(u64, u64)>> = Vec::with_capacity(bundles.len());
    let mut todo: Vec<usize> = Vec::new();
    for (i, b) in bundles.iter().enumerate() {
        let s = stamp(b);
        stamps.push(s);
        let hit = cache.entries.get(&key(b)).filter(|e| Some((e.size, e.modified)) == s);
        match hit {
            Some(e) if !(retry_failed && matches!(e.result, ProbeResult::Err(_))) => {
                results[i] = Some(e.result.clone());
                DONE.fetch_add(1, Ordering::Relaxed);
            }
            _ => todo.push(i),
        }
    }
    if !todo.is_empty() {
        log::info!("scanning {} VST3 bundle(s) ({} from the cache)", todo.len(), bundles.len() - todo.len());
    }
    let fresh = run_probes(bundles, &todo, &cfg);
    for (i, r) in fresh {
        results[i] = Some(r);
    }
    let results: Vec<ProbeResult> = results.into_iter().map(|r| r.unwrap_or_else(|| ProbeResult::Err("not scanned".into()))).collect();
    if let Some(file) = cfg.cache_file.as_deref() {
        let mut next = CacheFile { version: CACHE_VERSION, entries: BTreeMap::new() };
        for ((b, s), r) in bundles.iter().zip(&stamps).zip(&results) {
            if let Some((size, modified)) = *s {
                next.entries.insert(key(b), CacheEntry { size, modified, result: r.clone() });
            }
        }
        // Folders outside this scan (added search dirs, VST3_PATH changes) keep their entries.
        for (k, e) in std::mem::take(&mut cache.entries) {
            next.entries.entry(k).or_insert(e);
        }
        save_cache(file, &next);
    }
    results
}

fn run_probes(bundles: &[PathBuf], todo: &[usize], cfg: &Config) -> Vec<(usize, ProbeResult)> {
    let Some(program) = cfg.probe_program.as_deref() else {
        return todo
            .iter()
            .map(|&i| {
                let r = probe_in_process(&bundles[i]);
                DONE.fetch_add(1, Ordering::Relaxed);
                (i, r)
            })
            .collect();
    };
    let workers = std::thread::available_parallelism().map_or(4, std::num::NonZero::get).clamp(1, MAX_WORKERS).min(todo.len().max(1));
    let queue = Mutex::new(todo.to_vec());
    let out = Mutex::new(Vec::with_capacity(todo.len()));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let next = queue.lock().unwrap_or_else(PoisonError::into_inner).pop();
                    let Some(i) = next else { break };
                    let r = probe_out_of_process(program, &bundles[i], cfg.timeout);
                    if let ProbeResult::Err(e) = &r {
                        log::warn!("VST3 {}: {e}", bundles[i].display());
                    }
                    out.lock().unwrap_or_else(PoisonError::into_inner).push((i, r));
                    DONE.fetch_add(1, Ordering::Relaxed);
                }
            });
        }
    });
    out.into_inner().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soundcraft-vst3-probe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn results_round_trip_through_json() {
        let r = ProbeResult::Err("boom".into());
        let j = serde_json::to_string(&r).unwrap();
        assert_eq!(j, r#"{"err":"boom"}"#);
        assert_eq!(serde_json::from_str::<ProbeResult>(&j).unwrap(), r);
        assert_eq!(serde_json::from_str::<ProbeResult>(r#"{"ok":[]}"#).unwrap(), ProbeResult::Ok(vec![]));
    }

    #[test]
    fn cache_survives_a_save_and_ignores_other_versions() {
        let d = tmp("cache");
        let f = d.join("vst3-cache.json");
        let mut c = CacheFile { version: CACHE_VERSION, entries: BTreeMap::new() };
        c.entries.insert("a".into(), CacheEntry { size: 1, modified: 2, result: ProbeResult::Err("x".into()) });
        save_cache(&f, &c);
        assert_eq!(load_cache(&f).entries.len(), 1);
        std::fs::write(&f, r#"{"version":999,"entries":{}}"#).unwrap();
        assert!(load_cache(&f).entries.is_empty());
        std::fs::write(&f, "not json").unwrap();
        assert!(load_cache(&f).entries.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_probe_program_is_a_failure_not_a_hang() {
        let r = probe_out_of_process(Path::new("/definitely/not/here/soundcraft"), Path::new("x.vst3"), Duration::from_secs(1));
        assert!(matches!(r, ProbeResult::Err(ref e) if e.contains("cannot start")), "{r:?}");
    }
}
