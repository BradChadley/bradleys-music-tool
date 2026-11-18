#![windows_subsystem = "windows"]
//! Cadence's hidden engine: no window, no console. Cadence starts it and talks to it over its standard
//! input and output, one JSON message per line (`ENGINE-PROTOCOL.md`). Everything it does is in the
//! library (`service`).

fn main() {
    // the nightly backup task starts it this way, with no Cadence to talk to
    if let Some(code) = bradleys_music_tool::service::nightly_if_requested() {
        std::process::exit(code);
    }
    std::process::exit(bradleys_music_tool::service::run());
}
