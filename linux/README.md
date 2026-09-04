# FreeFlow for Linux

A Wayland-native reimplementation of FreeFlow's dictation pipeline, in Rust.
Hold a key, speak, and cleaned-up text is typed at your cursor.

The macOS app in `../Sources` is unchanged and unaffected.
This tree does not build on macOS, and the `Makefile` does not build this tree.

## Why a reimplementation and not a port

FreeFlow is 18,900 lines of Swift with no cross-platform abstraction layer, and every load-bearing subsystem is bound to an Apple framework: `CGEvent.tapCreate` for hotkeys, synthetic Cmd-V for pasting, `NSPasteboard`, `AXUIElement` for selected text, `AVCaptureSession` for audio, CoreData for history, SwiftUI for all of the UI.

About 3,000 lines were genuinely portable, and they are the valuable 3,000: the shortcut state machine, the tuned cleanup and edit-mode prompts, model configuration, and the transcript sanitizers.
Those are ported faithfully, with each module naming the Swift file it came from.
The prompts in `crates/freeflow-core/src/prompt_text.rs` are extracted mechanically from `Sources/PostProcessingService.swift` and verified byte-identical, so upstream prompt changes can be re-extracted rather than re-typed.

Everything below the prompts is new, because none of it could be carried over.

## What it does differently from macOS

| | macOS | Linux |
|---|---|---|
| Hotkeys | `CGEvent` tap, needs Accessibility permission | Hyprland `bind`/`bindri` dispatching to `freeflow` |
| Default shortcut | Hold `Fn` | Hold `Super+Alt+D` — `Fn` is handled in keyboard firmware on PCs and never reaches the OS |
| Typing the result | Clipboard + synthetic Cmd-V + restore | `wtype` types it directly; the clipboard is never touched |
| Selected text | Accessibility API | The PRIMARY selection, which Wayland populates on highlight |
| Window context | Accessibility API | `hyprctl activewindow -j` |
| Transcription | Groq Whisper | Local `whisper-server`, or any OpenAI-compatible URL |
| Cleanup | Groq | A local agent CLI, or any OpenAI-compatible URL |
| Autostart | `SMAppService` | systemd user service |
| Updates | In-app DMG updater | `nix flake update` |

Not carried over: the screenshot/vision context (needs a model that will not fit in 8 GB of VRAM), notch-aware overlay geometry, the setup wizard, the in-app updater, and realtime WebSocket streaming transcription.

## Requirements

- A wlroots-based Wayland compositor. Developed against Hyprland 0.55.
  Needs `zwp_virtual_keyboard_manager_v1` for typing and `wlr-data-control` for reading selections without focus. Check with `wayland-info | grep -E 'virtual_keyboard|data_control'`.
- PipeWire.
- `wtype` and `wl-clipboard` — the Nix package wraps both onto `PATH`.
- Optional: an NVIDIA GPU for local transcription, or a Groq/OpenAI-compatible API key.

## Install

Add the flake to your Home Manager configuration:

```nix
{
  inputs.freeflow.url = "path:/home/you/Documents/freeflow";

  # in your home-manager configuration:
  imports = [ inputs.freeflow.homeManagerModules.default ];

  services.freeflow = {
    enable = true;
    whisper.enable = true;
    whisper.cudaSupport = true;    # long, uncached build; see below
    settings = {
      transcription.base_url = "http://127.0.0.1:8080/v1/audio";
      dictation.custom_vocabulary = [ "NixOS" "Hyprland" "PipeWire" ];
    };
  };
}
```

That installs both binaries, starts two user services, and adds the Hyprland bindings.

Without Home Manager, `nix build .#freeflow`, run `freeflowd` yourself, and add the bindings from `freeflow binds` to `hyprland.conf`.

## Bindings

`freeflow binds` prints them. The defaults:

```
bind   = SUPER_ALT, D, exec, freeflow hold press
bindri = SUPER_ALT, D, exec, freeflow hold release
bind   = SUPER_ALT, T, exec, freeflow toggle press
bindri = SUPER_ALT, T, exec, freeflow toggle release
bind   = SUPER_ALT_SHIFT, D, exec, freeflow latch
bind   = SUPER_ALT, C, exec, freeflow copy-again
bind   = SUPER_ALT, Escape, exec, freeflow cancel
```

**Release edges must use `bindri`, never `bindr`.**
Hyprland filters binds against the current modifier mask on release as well as on press, so with a plain `bindr` the release never fires if you let go of `SUPER` before `D` — and recording would continue until the daemon's watchdog stopped it. The `i` flag ignores modifiers. A flake check asserts this.

## Configuration

`~/.config/freeflow/config.toml`. Every field has a default, so the file is optional.
`freeflow default-config` prints a fully-populated example.

Secrets are never inlined: `api_key_file` points at a separate file, which must be `0600` or the daemon refuses to start. That keeps the config safe to commit into a Nix repository.

### Choosing backends

Transcription and cleanup are independent, and each is a configuration choice rather than a code path.

```toml
[llm]
backend = "cli_harness"           # or "openai_compat", or "none"
fallback_backend = "openai_compat"
```

- `cli_harness` drives a locally installed agent CLI (`claude` by default) over its streaming protocol, reusing an existing subscription instead of an API key. Swap harnesses by changing `command`, `args`, and `protocol`.
- `openai_compat` is any OpenAI-compatible endpoint: Groq, Gemini's compatibility layer, Ollama, llama.cpp, vLLM.
- `none` skips cleanup entirely and types the raw transcript.

When the primary backend fails the fallback runs, and if that fails too the raw transcript is typed anyway. **A provider failure never costs you your words.**

### Local transcription

The module runs `whisper-server` with `--request-path /v1/audio --inference-path /transcriptions`, which makes whisper.cpp answer on `POST /v1/audio/transcriptions` using OpenAI's own field names. That is why local and hosted transcription share one code path.

`whisper.cudaSupport = true` is much faster but the build is long and uncached, because CUDA is unfree. `whisper.cudaCapabilities` defaults to `[ "8.6" ]` (Ampere, RTX 30-series); change it for other hardware. Leave it off to start on CPU.

## Measured latency

On an i5-12600K with an RTX 3070, warm, using the `claude` harness for cleanup:

```
transcription  ~570ms    cleanup  ~1500ms    total  ~2000ms
```

Reproduce with `freeflow bench some.wav`, which runs a fixed recording through the real pipeline and reports per-stage timings.

The cleanup number depends heavily on the backend. Groq is roughly 500ms; a local 8B model on this GPU is 1.5–2.5s.

The harness is only viable because workers are **pre-warmed on key-down**: a cold `claude -p` invocation costs about 5.8s, of which only 0.07s is process startup — the rest is agent-session bootstrap. Starting that work when you press the key hides it behind the time you spend speaking. Workers are then used once and discarded, so no dictation's transcript is ever visible to the next one.

## Troubleshooting

Run `freeflow doctor` first. It checks the microphone, `wtype`, the compositor, the LLM backend, and the transcription endpoint.

- **Nothing happens on the hotkey.** `systemctl --user status freeflow`, then `journalctl --user -u freeflow -f`.
- **Recording never stops.** A release binding is using `bindr` instead of `bindri`.
- **Silence produces nothing.** Correct. whisper.cpp emits `[BLANK_AUDIO]`, which is filtered, and an empty transcript skips the LLM entirely.
- **Wrong microphone.** `freeflow devices`, then set `audio.device`. The overlay's level meter is the fastest way to spot this.
- **Text does not appear in one specific app.** Some Electron, Java, and XWayland clients mishandle synthesised keys. Set a per-app override:
  ```toml
  [injection.method_by_class]
  Chromium = "clipboard"
  ```
- **Cleanup is slow or fails.** Set `fallback_backend`, or `backend = "none"` to bypass it. Quota problems surface in the log as a usage-limit warning.

## Development

```bash
cd linux
nix develop
./check.sh          # fmt, clippy -D warnings, tests, and the module check
```

Two suites need real services and are ignored by default:

```bash
cargo test -p freeflow-llm --test live_harness -- --ignored   # a real `claude` round trip
cargo test -p freeflow-asr --test live_whisper -- --ignored   # a running whisper-server
```

The first catches upstream protocol drift in the harness; the second proves the whisper route flags still work.

### Layout

| Crate | Contents |
|---|---|
| `freeflow-core` | All the ported logic. Pure, no I/O, no async — so it tests without a microphone, a compositor, or a network |
| `freeflow-config` | TOML schema, XDG paths, secret file handling |
| `freeflow-asr` | OpenAI-compatible transcription client |
| `freeflow-llm` | `LlmBackend` trait, the OpenAI client, the CLI harness and its worker pool, and the fallback router |
| `freeflow-platform` | Audio capture, clipboard, text injection, compositor context |
| `freeflow-daemon` | `freeflowd` — orchestration and the control socket |
| `freeflow-cli` | `freeflow` — the thin client Hyprland binds dispatch to |

The split exists so that the interesting logic is testable and the untestable parts are thin. `freeflow-llm` additionally ships a `freeflow-mock-harness` fixture that speaks the agent CLI protocol with injectable faults, which is how the worker pool's hang, crash, and authentication-failure paths are tested offline without spending quota.
