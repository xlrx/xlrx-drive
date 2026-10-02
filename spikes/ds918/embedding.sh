#!/bin/sh
# Local embedding model without AVX (PLAN 7.5): ONNX Runtime (multilingual-e5-small) vs. llama.cpp
# (EmbeddingGemma 300M). Runs in a container on the NAS; downloads the models from the internet once.
#   sudo sh spikes/ds918/embedding.sh
set -u
cd "$(dirname "$0")"
OUT="results-embedding-$(date +%Y-%m-%d).md"
CACHE=/volume1/docker/xlrx/spike-models
mkdir -p "$CACHE"

onnx() {
	echo "## ONNX Runtime, multilingual-e5-small (int8), 2 Threads"
	docker run --rm -v "$CACHE:/models" python:3.12-slim sh -c '
pip install -q onnxruntime tokenizers huggingface_hub numpy >/dev/null 2>&1 &&
python - <<PY
import time, numpy as np, onnxruntime as ort
from huggingface_hub import hf_hub_download
from tokenizers import Tokenizer
repo = "Xenova/multilingual-e5-small"
model = hf_hub_download(repo, "onnx/model_quantized.onnx", cache_dir="/models")
tok = Tokenizer.from_file(hf_hub_download(repo, "tokenizer.json", cache_dir="/models"))
tok.enable_truncation(512); tok.enable_padding()
o = ort.SessionOptions(); o.intra_op_num_threads = 2
s = ort.InferenceSession(model, o, providers=["CPUExecutionProvider"])
texts = ["passage: Rechnung der Stadtwerke für Heizung und Warmwasser im Januar 2025, Kundennummer 4711."] * 32
enc = tok.encode_batch(texts)
feed = {"input_ids": np.array([e.ids for e in enc], dtype=np.int64),
        "attention_mask": np.array([e.attention_mask for e in enc], dtype=np.int64)}
if "token_type_ids" in [i.name for i in s.get_inputs()]:
    feed["token_type_ids"] = np.zeros_like(feed["input_ids"])
s.run(None, feed)
t = time.time(); n = 0
while time.time() - t < 20:
    s.run(None, feed); n += len(texts)
print(f"{n/(time.time()-t):.1f} Texte/s (je ~25 Tokens)")
PY' || echo "ONNX fehlgeschlagen (Illegal instruction = Build mit AVX)"
}

llama() {
	echo "## llama.cpp, EmbeddingGemma 300M (Q8_0), 2 Threads"
	[ -f "$CACHE/eg.gguf" ] || docker run --rm -v "$CACHE:/models" curlimages/curl -sL -o /models/eg.gguf \
		"https://huggingface.co/ggml-org/embeddinggemma-300M-GGUF/resolve/main/embeddinggemma-300M-Q8_0.gguf"
	docker run --rm -v "$CACHE:/models" --entrypoint sh ghcr.io/ggml-org/llama.cpp:full -c '
B=$(command -v llama-embedding || ls /app/llama-embedding 2>/dev/null)
$B -m /models/eg.gguf -t 2 -p "Rechnung der Stadtwerke für Heizung und Warmwasser im Januar 2025" 2>&1 |
  grep -E "eval time|tokens per second|error|Illegal" | head -5' ||
		echo "llama.cpp fehlgeschlagen (Illegal instruction = Image mit AVX; dann aus den Quellen mit -DGGML_NATIVE=OFF -DGGML_AVX=OFF -DGGML_AVX2=OFF bauen)"
}

{
	echo "# Embedding-Spike $(date -Iseconds)"
	echo
	onnx
	echo
	llama
} 2>&1 | tee "$OUT"
echo
echo "Fertig: $(pwd)/$OUT"
