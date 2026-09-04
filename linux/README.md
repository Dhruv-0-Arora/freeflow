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
| Hotkeys | `CGEvent` tap, needs Accessibility permission | Hyprland press binds dispatching to `freeflow`; releases watched on `/dev/input` |
| Default shortcut | Hold `Fn` | Hold `Super+Alt+D` — `Fn` is handled in keyboard firmware on PCs and never reaches the OS |
| Typing the result | Clipboard + synthetic Cmd-V + restore | `wtype` types it directly; the clipboard is never touched |
| Selected text | Accessibility API | The PRIMARY selection, read only when the explicit edit bind starts the session |
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
# FreeFlow dictation - press binds only.
# The daemon watches /dev/input for key releases itself while a session is
# active. Compositor release binds are unusable here: Hyprland consumes the
# press of a release-bound key, and the ignore-mods flag needed to catch a
# release after the modifiers are lifted extends that to every unmodified
# press - which would make the bound letters untypable everywhere.

# Hold to talk; release to transcribe.
bind = SUPER_ALT, D, exec, freeflow hold press --key D

# Tap to start, tap again to stop.
bind = SUPER_ALT, T, exec, freeflow toggle press --key T

# Hold to rewrite the highlighted text with what you say (edit mode).
bind = SUPER_ALT, E, exec, freeflow edit press --key E

# Convert an in-progress hold into a toggle that outlives the key release.
bind = SUPER_ALT_SHIFT, D, exec, freeflow latch --key T

# Re-paste the last result, and abandon a dictation.
bind = SUPER_ALT, C, exec, freeflow copy-again
bind = SUPER_ALT, Escape, exec, freeflow cancel
```

**Release edges are never bound in the compositor.**
Hyprland consumes the press of a release-bound key, and the ignore-modifiers flag needed to catch a release after the modifiers are lifted extends that to every unmodified press - a `bindri` on D and T once made plain `d` and `t` untypable in every application.
Instead, each session bind passes its main key via `--key`, and the daemon polls `/dev/input` for that key's release only while a session is active - nothing reads input devices outside a dictation, and nothing is ever grabbed.
This requires the user to be in the `input` group; `freeflow doctor` reports it as the `keywatch` check.
A flake check asserts no release bind can reappear, and that this file, the module template, and `freeflow binds` all carry identical bind text.

**High function keys must be bound by keycode, not name.**
F13-F24 are ideal firmware-remap targets (they exist on no physical board), but xkeyboard-config does not give them their own keysyms: KEY_F20 produces `XF86AudioMicMute` and KEY_F21 `XF86TouchpadToggle`, so a Hyprland bind on the name `F20` never fires.
Bind `code:N` instead, where N is the kernel code plus 8 (KEY_F20 = 190 -> `code:198`); the daemon's `--key` parser accepts the same `code:` form.

**Edit mode has its own binding and is never inferred.**
On Wayland the PRIMARY selection persists from whatever was last highlighted in any application, so "a selection exists" carries no information about what the user wants now - inferring edit mode from it rewrote stale selections instead of typing dictations.
Ordinary dictations never read the selection; only the edit bind captures it, at the moment the key goes down.

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
  Antigravity's `agy` is supported via `protocol = "agy_stream_json"`; its dialect differs from Claude's and it has no system-prompt flag, so the wrapper travels in each turn.
  The `live_agy` ignored suite checks the adapter against the real binary:

  ```toml
  [llm.cli_harness]
  command = "agy"
  args = [
    "--print=", "--input-format", "stream-json", "--output-format", "stream-json",
    "--model", "gemini-3.1-pro", "--effort", "low", "--disable-slash-commands",
  ]
  protocol = "agy_stream_json"
  ```
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

## History

Every completed dictation is stored locally in SQLite at
`~/.local/state/freeflow/history.sqlite3`, created `0600` and capped at 500
entries. Nothing is ever transmitted. Turn it off with
`dictation.history_enabled = false`.

```console
$ freeflow history --limit 3
1788555435    523ms+ 4014ms  foot    Let's meet Wednesday after lunch and ship the NixOS port.
                                raw: Let's meet Wednesday after lunch and ship the Nick Sodes port.
```

The `raw:` line appears when cleanup changed the text, which is the only
practical way to tell whether the prompt and custom vocabulary are earning
their latency. Above, `custom_vocabulary = [ "NixOS" ]` is what turned
"Nick Sodes" back into "NixOS".

`freeflow history --clear` deletes everything.

## Troubleshooting

Run `freeflow doctor` first. It checks the microphone, `wtype`, the compositor, the LLM backend, and the transcription endpoint.

- **Nothing happens on the hotkey.** `systemctl --user status freeflow`, then `journalctl --user -u freeflow -f`.
- **Recording never stops on release.** Check the `keywatch` line in `freeflow doctor`: if no input device is readable, add the user to the `input` group. The watchdog still stops the recording at `audio.max_recording_seconds`.
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

And an end-to-end check that types into a real window:

```bash
scripts/e2e.sh speech.wav ./result/bin/freeflow
```

It uses a recorded WAV rather than the microphone so it is reproducible.
Microphone capture is verified separately with `freeflow devices` and the
overlay's level meter.

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
