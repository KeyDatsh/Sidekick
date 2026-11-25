#!/usr/bin/env bash
# Builds speech-rs: needs CUDA Toolkit 12.9, LLVM (libclang) and VS Build Tools with CMake. Git Bash.
set -euo pipefail
cd "$(dirname "$0")/../.."

cuda='C:\Program Files\NVIDIA GPU Computing Toolkit\CUDA\v12.9'
# MSBuild's CUDA integration reads the versioned variable, whisper-rs and ct2rs read the others.
export CUDA_PATH_V12_9="$cuda" CUDA_PATH="$cuda" CUDA_TOOLKIT_ROOT_DIR="$cuda"
export LIBCLANG_PATH='C:\Program Files\LLVM\bin'
# ct2rs is always built against the static C runtime on Windows, so everything else has to be too.
export RUSTFLAGS='-C target-feature=+crt-static'
# whisper-rs forwards CMAKE_*: without these its CUDA objects still use the dynamic runtime (LNK2038).
# Cargo does not notice a change here: run `cargo clean --release -p whisper-rs-sys --target-dir target/speech`.
export CMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded CMAKE_POLICY_DEFAULT_CMP0091=NEW
export PATH="/c/Program Files (x86)/Microsoft Visual Studio/2022/BuildTools/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin:/c/Program Files/NVIDIA GPU Computing Toolkit/CUDA/v12.9/bin:$PATH"

cargo build --release -p speech-rs --target-dir target/speech "$@"
