#!/usr/bin/env bash
# Downloads everything bench.py needs (about 8 GB) into bin/, models/, audio/ and .venv/. Git Bash.
set -euo pipefail
cd "$(dirname "$0")"
mkdir -p bin/whisper models audio
get() { curl -fSL --retry 3 -o "$1" "$2"; }

get bin/w.zip https://github.com/ggml-org/whisper.cpp/releases/download/b5454/whisper-bin-win-cuda-12.4.0-x64.zip
echo "afef0b881c500958921c3f5523b50e59ee2ec9b6f5cbd25b324c51ed308a957a  bin/w.zip" | sha256sum -c
unzip -qo bin/w.zip -d bin/whisper && rm bin/w.zip

for m in large-v3-turbo large-v3-q5_0 large-v3; do
  get models/ggml-$m.bin https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-$m.bin
done
sha256sum -c <<'SUMS'
1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69  models/ggml-large-v3-turbo.bin
d75795ecff3f83b5faa89d1900604ad8c780abd5739fae406de19f23ecd98ad1  models/ggml-large-v3-q5_0.bin
64d182b440b98d5203c4f9bd541544d84c605196c4f7b845dfa11fb23594d1e2  models/ggml-large-v3.bin
SUMS
get models/ggml-silero-v6.2.0.bin https://huggingface.co/ggml-org/whisper-vad/resolve/main/ggml-silero-v6.2.0.bin

for l in uk_ua en_us; do
  get audio/$l.tsv https://huggingface.co/datasets/google/fleurs/resolve/main/data/$l/dev.tsv
  get audio/$l.tar.gz https://huggingface.co/datasets/google/fleurs/resolve/main/data/$l/audio/dev.tar.gz
  mkdir -p audio/$l && tar xzf audio/$l.tar.gz -C audio/$l --force-local && rm audio/$l.tar.gz
done

py -3.11 -m venv .venv
.venv/Scripts/python -m pip install -q ctranslate2 sentencepiece sacrebleu requests transformers
.venv/Scripts/python -m pip install -q torch --index-url https://download.pytorch.org/whl/cpu
for m in opus-mt-uk-en opus-mt-en-uk opus-mt-tc-big-zle-en opus-mt-tc-big-en-zle; do
  .venv/Scripts/ct2-transformers-converter --model Helsinki-NLP/$m --output_dir models/$m \
    --quantization int8 --copy_files source.spm target.spm --force
done
