#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")/.."

export VLLM_USE_FLASHINFER_SAMPLER=0
export PYTORCH_CUDA_ALLOC_CONF=expandable_segments:True

exec .venv-vllm/bin/vllm serve Qwen/Qwen3-8B-AWQ \
  --host 127.0.0.1 \
  --port 8000 \
  --max-model-len 1024 \
  --gpu-memory-utilization 0.80 \
  --max-num-seqs 1 \
  --max-num-batched-tokens 512 \
  --enforce-eager
