//! Records from the microphone and writes a WAV.
//!
//! `cargo run -p freeflow-platform --example record -- [seconds] [out.wav] [device]`
use std::time::Duration;

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(3);
    let out = std::env::args()
        .nth(2)
        .unwrap_or_else(|| "/tmp/freeflow-rec.wav".into());
    let device = std::env::args().nth(3);

    println!("devices:");
    for d in freeflow_platform::audio::list_input_devices().unwrap() {
        println!(
            "  {}{}  ({})",
            if d.is_default { "* " } else { "  " },
            d.id,
            d.description
        );
    }

    let rec = freeflow_platform::audio::start(device.as_deref(), 120).expect("start failed");
    println!("\nrecording {secs}s...");
    let mut peak = 0.0f32;
    for _ in 0..(secs * 10) {
        std::thread::sleep(Duration::from_millis(100));
        peak = peak.max(rec.level());
    }
    let dur = rec.duration();
    let n = rec.sample_count();
    let wav = rec.stop().expect("stop failed");
    std::fs::write(&out, &wav).unwrap();
    println!(
        "samples={n} duration={dur:?} peak_level={peak:.4} wav_bytes={} -> {out}",
        wav.len()
    );
}
