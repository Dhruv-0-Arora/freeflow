# whisper.cpp's server, configured to speak OpenAI's transcription route.
#
# whisper-server serves `/inference` by default, but `--request-path` and
# `--inference-path` are settable, so pointing them at `/v1/audio` and
# `/transcriptions` makes it answer on `POST /v1/audio/transcriptions` with
# OpenAI's own multipart field names and `{"text": ...}` response shape. That
# is why the daemon needs no local-vs-hosted special case at all: the two
# differ only by base URL.
#
# `--convert` is deliberately not used. It shells out to ffmpeg, and the
# recorder already produces exactly the 16 kHz mono PCM16 WAV the server wants.
{
  lib,
  pkgs,
  cudaSupport ? false,
  # Pinning the architecture cuts build time substantially. null keeps the
  # nixpkgs default, which builds for every supported architecture.
  cudaCapabilities ? null,
}:
let
  # whisper-cpp reads `cudaPackages.flags.cmakeCudaArchitecturesString` for
  # CMAKE_CUDA_ARCHITECTURES, and that string is *derived* from
  # cudaCapabilities rather than read from it at build time. Overriding only
  # cudaCapabilities therefore silently does nothing, and the build compiles
  # every architecture nixpkgs supports - nine of them - instead of the ones
  # asked for. Both have to be set.
  cudaArchitecturesString = lib.concatStringsSep ";" (
    map (c: lib.replaceStrings [ "." ] [ "" ] c) cudaCapabilities
  );

  whisper =
    if cudaSupport && cudaCapabilities == null then
      pkgs.whisper-cpp.override { cudaSupport = true; }
    else if cudaSupport then
      (pkgs.whisper-cpp.override {
        cudaSupport = true;
        cudaPackages = pkgs.cudaPackages // {
          flags = pkgs.cudaPackages.flags // {
            inherit cudaCapabilities;
            cmakeCudaArchitecturesString = cudaArchitecturesString;
          };
        };
      })
    else
      pkgs.whisper-cpp;
in
{
  package = whisper;

  # Model files are large and licensed separately, so they are fetched at
  # first run into the state directory rather than baked into the closure.
  fetchModelScript = pkgs.writeShellApplication {
    name = "freeflow-fetch-whisper-model";
    runtimeInputs = [ whisper ];
    text = ''
      model="''${1:-large-v3-turbo}"
      dir="''${XDG_DATA_HOME:-$HOME/.local/share}/freeflow/models"
      mkdir -p "$dir"
      if [ -f "$dir/ggml-$model.bin" ]; then
        echo "already present: $dir/ggml-$model.bin"
        exit 0
      fi
      cd "$dir"
      whisper-cpp-download-ggml-model "$model"
      echo "downloaded to $dir/ggml-$model.bin"
    '';
  };

  inherit lib;
}
