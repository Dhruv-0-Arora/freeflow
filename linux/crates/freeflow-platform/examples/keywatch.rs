//! Probe for the evdev release watcher.
//!
//! Prints how many devices carry the given key code (default 32, KEY_D) and
//! waits for one release. If the key is up when the watch starts it fires
//! immediately - the same behaviour the daemon relies on for fast taps.
//!
//!     cargo run -p freeflow-platform --example keywatch -- 32

use std::sync::mpsc;

use freeflow_platform::keywatch;

fn main() {
    let code: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);

    println!(
        "{} readable devices carry key code {code}",
        keywatch::watchable_device_count(code)
    );

    let (tx, rx) = mpsc::channel();
    let _watch = keywatch::watch_release(code, move || {
        let _ = tx.send(());
    })
    .expect("could not start the watch");

    println!("watching; hold and release the key, or this fires at once if it is already up");
    rx.recv().expect("watcher thread ended without firing");
    println!("release observed");
}
