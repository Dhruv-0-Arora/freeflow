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
  # Pinning the architecture cuts build time substantially and guarantees the
  # GPU is actually targeted. 8.6 is Ampere, which covers the RTX 3070.
  cudaCapabilities ? [ "8.6" ],
}:
let
  whisper =
    if cudaSupport then
      (pkgs.whisper-cpp.override {
        cudaSupport = true;
        cudaPackages = pkgs.cudaPackages // {
          flags = pkgs.cudaPackages.flags // {
            inherit cudaCapabilities;
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
