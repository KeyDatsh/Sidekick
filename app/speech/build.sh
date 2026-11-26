#!/usr/bin/env bash
# Builds the speech worker and puts it next to the app's exe. Needs CUDA Toolkit 12.9, LLVM
# (libclang) and VS Build Tools with CMake. Git Bash. Why each variable: docs/stage-0.md, section 4.
#   bash app/speech/build.sh            release build (debug whisper.cpp is far too slow)
#   bash app/speech/build.sh test       unit tests
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
cd "$here"

cuda='C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.9'
export CUDA_PATH_V12_9="$cuda" CUDA_PATH="$cuda" CUDA_TOOLKIT_ROOT_DIR="$cuda"
export LIBCLANG_PATH='C:\Program Files\LLVM\bin'
export RUSTFLAGS='-C target-feature=+crt-static'
export CMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded CMAKE_POLICY_DEFAULT_CMP0091=NEW
# RTX 30xx; change for another GPU.
export CUDAARCHS=86
export CMAKE_TOOLCHAIN_FILE="$(cygpath -m "$here")/msvc-release.cmake"
export PATH="/c/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin:/c/Program Files/NVIDIA GPU Computing Toolkit/CUDA/v12.9/bin:$PATH"
# Shared with prototypes/speech-rs: the same flags, so whisper.cpp and CTranslate2 compile once.
target="$here/../../target/speech"

if [ "${1:-}" = test ]; then
  cargo test --release --target-dir "$target"
  exit
fi
cargo build --release --target-dir "$target"
# The app looks for the worker next to its own exe.
for profile in debug release; do
  mkdir -p "$here/../target/$profile"
  cp "$target/release/sidekick-speech.exe" "$here/../target/$profile/"
done
echo "sidekick-speech.exe copied to app/target/{debug,release}"
