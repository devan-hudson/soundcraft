//! `soundcraft-cli`: SoundCraft without a window. See the library crate for the commands.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

fn main() {
    // A child started to probe one VST3 bundle for a scan answers and exits before anything else.
    if let Some(code) = soundcraft_engine::run_plugin_probe_if_requested() {
        std::process::exit(code);
    }
    // Scans load each plugin in a child copy of this program; `SOUNDCRAFT_VST3_CACHE` names a
    // file to cache the results in between runs.
    soundcraft_engine::configure_plugin_scan(
        std::env::var_os("SOUNDCRAFT_VST3_CACHE").filter(|v| !v.is_empty()).map(Into::into),
        std::env::current_exe().ok(),
    );
    soundcraft_cli::logger::install();
    // `args_os`, not `args`: a non-Unicode argument must reach clap, not panic before anything is printed.
    let code = soundcraft_cli::run(std::env::args_os().skip(1), &mut std::io::stdout(), &mut std::io::stderr());
    // The subcommand's engine and its plugin instances are gone by now. This is process teardown,
    // so it lives here and not in `run`, which tests call many times in one process.
    soundcraft_engine::shutdown_plugin_hosts();
    std::process::exit(code);
}
