# Evaluates the Home Manager module against stubbed Home Manager options.
#
# A module that fails to evaluate only shows up during someone's rebuild,
# which is the worst place to find out. This runs as part of `nix flake check`
# instead, without needing Home Manager as a flake input.
{ pkgs, self }:
let
  lib = pkgs.lib;
  module = import ./home-manager-module.nix { inherit self; };

  # The subset of Home Manager's option surface this module writes to.
  stubHomeManager = {
    options = {
      home.packages = lib.mkOption {
        type = lib.types.listOf lib.types.package;
        default = [ ];
      };
      xdg.configFile = lib.mkOption {
        type = lib.types.attrsOf lib.types.attrs;
        default = { };
      };
      xdg.dataHome = lib.mkOption {
        type = lib.types.str;
        default = "/home/stub/.local/share";
      };
      systemd.user.services = lib.mkOption {
        type = lib.types.attrsOf lib.types.attrs;
        default = { };
      };
      wayland.windowManager.hyprland.enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
      };
      wayland.windowManager.hyprland.extraConfig = lib.mkOption {
        type = lib.types.lines;
        default = "";
      };
    };
  };

  evalWith =
    extra:
    (lib.evalModules {
      specialArgs = { inherit pkgs; };
      modules = [
        stubHomeManager
        module
        extra
      ];
    }).config;

  enabled = evalWith {
    services.freeflow.enable = true;
    wayland.windowManager.hyprland.enable = true;
  };
  disabled = evalWith { services.freeflow.enable = false; };
  noWhisper = evalWith {
    services.freeflow.enable = true;
    services.freeflow.whisper.enable = false;
  };

  whisperExec = enabled.systemd.user.services.freeflow-whisper.Service.ExecStart;
  binds = enabled.wayland.windowManager.hyprland.extraConfig;

  assertions = [
    {
      name = "both user services are defined";
      ok = builtins.attrNames enabled.systemd.user.services == [
        "freeflow"
        "freeflow-whisper"
      ];
    }
    {
      name = "whisper-server is given the OpenAI-compatible route flags";
      ok =
        lib.hasInfix "--request-path /v1/audio" whisperExec
        && lib.hasInfix "--inference-path /transcriptions" whisperExec;
    }
    {
      # `--convert` shells out to ffmpeg; the recorder already emits the exact
      # 16 kHz mono PCM16 WAV the server wants.
      name = "whisper-server does not enable ffmpeg conversion";
      ok = !(lib.hasInfix "--convert" whisperExec);
    }
    {
      # A plain `bindr` misses the release when the modifier is lifted first,
      # which would leave the microphone recording.
      name = "release bindings use bindri rather than bindr";
      ok = lib.hasInfix "bindri = " binds && !(lib.hasInfix "\nbindr = " binds);
    }
    {
      name = "every action is bound";
      ok = builtins.all (a: lib.hasInfix "freeflow ${a}" binds) [
        "hold press"
        "hold release"
        "toggle press"
        "toggle release"
        "latch"
        "copy-again"
        "cancel"
      ];
    }
    {
      name = "whisper can be disabled independently of the daemon";
      ok = builtins.attrNames noWhisper.systemd.user.services == [ "freeflow" ];
    }
    {
      name = "nothing is configured when the module is disabled";
      ok = disabled.systemd.user.services == { } && disabled.home.packages == [ ];
    }
  ];

  failures = builtins.filter (a: !a.ok) assertions;
in
if failures == [ ] then
  pkgs.runCommand "freeflow-module-test" { } ''
    echo "${toString (builtins.length assertions)} module assertions passed" > $out
  ''
else
  throw "home-manager module assertions failed: ${toString (map (f: f.name) failures)}"
