"""Stage 0 measurements: latency and quality of local speech translation on the GPU.

Uses FLEURS dev recordings: the same sentences exist in Ukrainian and English, so a
translation of one language can be scored (chrF) against the human text of the other.
Every model is loaded once and kept warm, as it will be in the app.
"""
import argparse
import csv
import io
import json
import statistics
import subprocess
import threading
import time
import wave
from pathlib import Path

import ctranslate2
import numpy as np
import requests
import sacrebleu
import sentencepiece

ROOT = Path(__file__).resolve().parent
EXE = ROOT / "bin/whisper/Release/whisper-server.exe"
MODELS = ROOT / "models"
RATE = 16000


def read_wav(path):
    """FLEURS files are 16 kHz mono, float or int16; returns float32 samples."""
    data = path.read_bytes()
    fmt = data.index(b"fmt ") + 8
    tag = int.from_bytes(data[fmt : fmt + 2], "little")
    rate = int.from_bytes(data[fmt + 4 : fmt + 8], "little")
    bits = int.from_bytes(data[fmt + 14 : fmt + 16], "little")
    assert rate == RATE, (path, rate)
    start = data.index(b"data", fmt) + 8
    if tag == 3 and bits == 32:
        return np.frombuffer(data[start:], "<f4").copy()
    return np.frombuffer(data[start : len(data) - (len(data) - start) % 2], "<i2").astype(np.float32) / 32768


def wav_bytes(x):
    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes((np.clip(x, -1, 1) * 32767).astype("<i2").tobytes())
    return buf.getvalue()


def load(lang):
    out = {}
    with open(ROOT / f"audio/{lang}.tsv", encoding="utf8") as f:
        for row in csv.reader(f, delimiter="\t", quoting=csv.QUOTE_NONE):
            path = ROOT / f"audio/{lang}/dev/{row[1]}"
            if path.exists():
                out.setdefault(int(row[0]), (path, row[2].strip()))
    return out


def vram():
    out = subprocess.run(
        ["nvidia-smi", "--query-gpu=memory.used", "--format=csv,noheader,nounits"], capture_output=True, text=True
    )
    return int(out.stdout.split()[0])


class Whisper:
    def __init__(self, model, port, *extra):
        before = vram()
        self.url = f"http://127.0.0.1:{port}"
        cmd = [str(EXE), "-m", str(MODELS / model), "--port", str(port), "-nt", "-bo", "1", *extra]
        self.proc = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        start = time.perf_counter()
        while True:
            try:
                requests.get(self.url, timeout=1)
                break
            except requests.RequestException:
                assert self.proc.poll() is None, "whisper-server died"
                assert time.perf_counter() - start < 120, "whisper-server did not start"
                time.sleep(0.1)
        self.session = requests.Session()
        for _ in range(3):  # warm up
            self.run(np.zeros(RATE * 3, np.float32), "en")
        self.vram = vram() - before

    def run(self, x, lang, translate=False, short_ctx=False):
        form = {"language": lang, "translate": str(translate).lower(), "response_format": "json", "temperature": "0"}
        if short_ctx:
            # 50 encoder frames per second instead of always encoding 30 s of padding.
            form["audio_ctx"] = str(min(1500, int(len(x) / RATE * 50) + 128))
        data = wav_bytes(x)
        t = time.perf_counter()
        r = self.session.post(self.url + "/inference", files={"file": ("a.wav", data)}, data=form, timeout=60)
        dt = time.perf_counter() - t
        r.raise_for_status()
        return " ".join(json.loads(r.text)["text"].split()), dt

    def close(self):
        self.proc.kill()
        self.proc.wait()


class Opus:
    def __init__(self, name, prefix=None):
        d = MODELS / name
        self.tr = ctranslate2.Translator(str(d), device="cpu", compute_type="int8", intra_threads=4)
        self.src = sentencepiece.SentencePieceProcessor(str(d / "source.spm"))
        self.tgt = sentencepiece.SentencePieceProcessor(str(d / "target.spm"))
        self.prefix = [prefix] if prefix else []
        for _ in range(3):
            self.run("test")

    def run(self, text):
        t = time.perf_counter()
        if not text.strip():
            return "", 0.0
        tokens = self.prefix + self.src.encode(text, out_type=str) + ["</s>"]
        hyp = self.tr.translate_batch([tokens], beam_size=2)[0].hypotheses[0]
        return self.tgt.decode(hyp), time.perf_counter() - t


def stats(ts):
    ts = sorted(1000 * t for t in ts)
    return f"p50={statistics.median(ts):4.0f} ms  p95={ts[int(len(ts) * 0.95) - 1]:4.0f} ms"


def report(name, dts, hyps, refs, extra=""):
    chrf = sacrebleu.corpus_chrf(hyps, [refs]).score
    print(f"{name:46} {stats(dts)}  chrF={chrf:4.1f} {extra}", flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("-n", type=int, default=60)
    ap.add_argument("--samples", type=int, default=4)
    ap.add_argument("--short-ctx", action="store_true", help="also try a reduced audio_ctx (breaks recognition)")
    args = ap.parse_args()
    contexts = (False, True) if args.short_ctx else (False,)

    uk, en = load("uk_ua"), load("en_us")
    ids = sorted(set(uk) & set(en))[: args.n]
    clips_uk = [read_wav(uk[i][0]) for i in ids]
    clips_en = [read_wav(en[i][0]) for i in ids]
    ref_uk = [uk[i][1] for i in ids]
    ref_en = [en[i][1] for i in ids]
    mean = lambda c: sum(map(len, c)) / len(c) / RATE
    print(f"{len(ids)} sentences, mean clip {mean(clips_uk):.1f} s uk / {mean(clips_en):.1f} s en")

    print("\n== Pause UA->EN: latency from key release, full clips ==")
    samples = {}
    for model in ("ggml-large-v3.bin", "ggml-large-v3-q5_0.bin"):
        w = Whisper(model, 8771)
        for short in contexts:
            got = [w.run(x, "uk", True, short) for x in clips_uk]
            name = f"{model[5:-4]} translate" + (" short ctx" if short else "")
            report(name, [d for _, d in got], [t for t, _ in got], ref_en, f"VRAM={w.vram} MiB")
            samples[name] = [t for t, _ in got]
        w.close()

    turbo = Whisper("ggml-large-v3-turbo.bin", 8772)
    got = [turbo.run(x, "uk", True) for x in clips_uk]
    report("large-v3-turbo translate (not trained for it)", [d for _, d in got], [t for t, _ in got], ref_en)
    print(f"   e.g. {got[0][0]}")
    for short in contexts:
        asr = [turbo.run(x, "uk", False, short) for x in clips_uk]
        label = "turbo short ctx" if short else "turbo"
        report(f"{label}, uk text only", [d for _, d in asr], [t for t, _ in asr], ref_uk, f"VRAM={turbo.vram} MiB")
        for m in ("opus-mt-uk-en", "opus-mt-tc-big-zle-en"):
            o = Opus(m)
            mt = [o.run(t) for t, _ in asr]
            name = f"{label} + {m}"
            report(name, [a[1] + b[1] for a, b in zip(asr, mt)], [t for t, _ in mt], ref_en, f"(MT {stats([b[1] for b in mt])})")
            samples[name] = [t for t, _ in mt]

    print("\n== Pause UA->EN: 3 s phrases (latency only, clips cut to 3 s) ==")
    cut = [x[RATE // 2 : RATE // 2 + 3 * RATE] for x in clips_uk]
    o = Opus("opus-mt-tc-big-zle-en")
    for short in contexts:
        d = []
        for x in cut:
            t, a = turbo.run(x, "uk", False, short)
            d.append(a + o.run(t)[1])
        print(f"turbo{' short ctx' if short else ''} + {'opus-mt-tc-big-zle-en':30} {stats(d)}")

    print("\n== Subtitles EN->UA ==")
    for short in contexts:
        asr = [turbo.run(x, "en", False, short) for x in clips_en]
        label = "turbo short ctx" if short else "turbo"
        report(f"{label}, en text only", [d for _, d in asr], [t for t, _ in asr], ref_en)
        for m, prefix in (("opus-mt-en-uk", None), ("opus-mt-tc-big-en-zle", ">>ukr<<")):
            o = Opus(m, prefix)
            mt = [o.run(t) for t, _ in asr]
            name = f"{label} + {m}"
            report(name, [a[1] + b[1] for a, b in zip(asr, mt)], [t for t, _ in mt], ref_uk, f"(MT {stats([b[1] for b in mt])})")
            samples[name] = [t for t, _ in mt]

    print("\n== One shared model: pause latency while subtitles keep running ==")
    stop = threading.Event()
    done = [0]

    def subtitles():
        other = Whisper.__new__(Whisper)
        other.url, other.session = turbo.url, requests.Session()
        i = 0
        while not stop.is_set():
            other.run(clips_en[i % len(clips_en)], "en", False, False)
            done[0] += 1
            i += 1

    alone = [turbo.run(x, "uk")[1] for x in cut]
    thread = threading.Thread(target=subtitles)
    thread.start()
    busy = [turbo.run(x, "uk")[1] for x in cut]
    stop.set()
    thread.join()
    print(f"turbo recognition, idle GPU            {stats(alone)}")
    print(f"turbo recognition, subtitles non-stop  {stats(busy)}  ({done[0]} subtitle phrases meanwhile)")
    turbo.close()

    print("\n== Silence and noise: hallucinations without and with Silero VAD ==")
    rng = np.random.default_rng(0)
    quiet = {
        "digital silence 5 s": np.zeros(5 * RATE, np.float32),
        "faint noise 5 s": rng.normal(0, 0.003, 5 * RATE).astype(np.float32),
        "loud noise 5 s": rng.normal(0, 0.05, 5 * RATE).astype(np.float32),
        "50 Hz hum 5 s": (0.05 * np.sin(2 * np.pi * 50 * np.arange(5 * RATE) / RATE)).astype(np.float32),
    }
    for flags in ((), ("--vad", "-vm", str(MODELS / "ggml-silero-v6.2.0.bin"))):
        w = Whisper("ggml-large-v3-turbo.bin", 8773, *flags)
        print("with VAD" if flags else "without VAD")
        for name, x in quiet.items():
            print(f"  {name:22}", {l: w.run(x, l)[0] for l in ("en", "uk")})
        if flags:
            got = [w.run(x, "en") for x in clips_en]
            report("  VAD + turbo on real speech", [d for _, d in got], [t for t, _ in got], ref_en)
        w.close()

    print("\n== Samples ==")
    for k in range(args.samples):
        print(f"UK: {ref_uk[k]}\nEN: {ref_en[k]}")
        for name, hyps in samples.items():
            print(f"   {name:44} {hyps[k]}")


main()
