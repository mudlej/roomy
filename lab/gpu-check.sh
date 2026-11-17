#!/bin/sh
# Find which graphics card pairing shows a GPU-accelerated VM correctly. See gpu_check.py.
cd "$(dirname "$0")/.." || exit 1
exec python3 lab/gpu_check.py "$@"
