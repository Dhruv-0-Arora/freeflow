# FreeFlow for Linux

Hold a key, speak, and cleaned-up text is typed at your cursor.
A Rust rewrite of FreeFlow for Wayland, packaged with Nix.

## Requirements

- Hyprland (or another wlroots compositor, with binds added by hand).
- PipeWire.
- Your user in the `input` group, so FreeFlow can see when you let go of the key.
  On NixOS: `users.users.<you>.extraGroups = [ "input" ];`
- Optional: the `claude` CLI or an OpenAI-compatible API key for cleanup.
  Without either, you get the raw transcript.

## Install

With Home Manager:

```nix
{
  inputs.freeflow.url = "github:Dhruv-0-Arora/freeflow";
  inputs.freeflow.inputs.nixpkgs.follows = "nixpkgs";

  # in your Home Manager config:
  imports = [ inputs.freeflow.homeManagerModules.default ];
  services.freeflow.enable = true;
}
```

This installs FreeFlow, runs local Whisper transcription, and adds the Hyprland binds.
The first start downloads the Whisper model (about 1.6 GB).

For an NVIDIA GPU, add:

```nix
services.freeflow.whisper.cudaSupport = true;
services.freeflow.whisper.cudaCapabilities = [ "8.6" ]; # your GPU, e.g. 8.6 for RTX 30-series
```

CUDA builds from source and takes a while.

Without Home Manager: `nix build github:Dhruv-0-Arora/freeflow`, run `freeflowd`, and add the output of `freeflow binds` to `hyprland.conf`.

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

Change them with `services.freeflow.keybinds`.
To bind F13-F24, use `code:N` (kernel keycode + 8) instead of the key name.

## Configuration

`~/.config/freeflow/config.toml`, or `services.freeflow.settings` in Nix.
Every field is optional; `freeflow default-config` prints them all.

To use Groq (or any OpenAI-compatible API) for cleanup instead of `claude`:

```toml
[llm]
backend = "openai_compat"

[llm.openai_compat]
api_key_file = "~/.config/freeflow/groq-key"  # must be chmod 600
```

Set `backend = "none"` to skip cleanup.
If cleanup fails, the raw transcript is typed instead.

History is kept locally in `~/.local/state/freeflow/history.sqlite3`.
See it with `freeflow history`, clear it with `freeflow history --clear`, or set `dictation.history_enabled = false`.

## Troubleshooting

Run `freeflow doctor` first.

- Nothing happens: `journalctl --user -u freeflow -f`.
- Recording does not stop on release: you are not in the `input` group.
- Wrong microphone: `freeflow devices`, then set `audio.device`.
- Text missing in one app: set `injection.method_by_class.<WindowClass> = "clipboard"`.

## Development

```bash
nix develop
linux/check.sh
```
