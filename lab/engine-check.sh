#!/bin/sh
# Drive the real roomy program against real QEMU and a real guest. See engine_check.py.
cd "$(dirname "$0")/.." || exit 1
exec python3 lab/engine_check.py "$@"
