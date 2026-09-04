//! Contract test against a real `whisper-server`.
//!
//! Ignored by default because it needs a running server. It exists to prove
//! the load-bearing claim behind this crate's single code path: that
//! whisper.cpp can be made to serve OpenAI's route and response shape with
//! nothing more than two command-line flags.
//!
//! ```text
//! whisper-server -m ggml-tiny.en.bin --port 18080 \
//!     --request-path /v1/audio --inference-path /transcriptions
//! FREEFLOW_TEST_ASR_URL=http://127.0.0.1:18080/v1/audio \
//!     cargo test -p freeflow-asr --test live_whisper -- --ignored --nocapture
//! ```

use freeflow_asr::TranscriptionClient;

fn base_url() -> String {
    std::env::var("FREEFLOW_TEST_ASR_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:18080/v1/audio".to_string())
}

/// A second of silence: enough to prove the round trip without shipping audio
/// fixtures, which the repository's privacy rules forbid anyway.
fn silent_wav() -> Vec<u8> {
    let samples = 16_000u32;
    let data_len = samples * 2;
    let mut wav = Vec::with_capacity(44 + data_len as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&16_000u32.to_le_bytes());
    wav.extend_from_slice(&32_000u32.to_le_bytes()); // byte rate
    wav.extend_from_slice(&2u16.to_le_bytes()); // block align
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.resize(44 + data_len as usize, 0);
    wav
}

#[tokio::test]
#[ignore = "requires a running whisper-server"]
async fn real_whisper_server_answers_on_the_openai_route() {
    let client = TranscriptionClient::new(base_url(), "whisper-large-v3-turbo");
    let text = client
        .transcribe_bytes(silent_wav(), "audio.wav".into())
        .await
        .expect("whisper-server rejected the OpenAI-shaped request");

    println!("transcript: {text:?}");
    // Silence must not produce pasteable text. whisper.cpp marks it with
    // `[BLANK_AUDIO]`, which the sanitizer is responsible for removing.
    assert!(
        text.trim().is_empty(),
        "silence produced pasteable text: {text:?}"
    );
}
