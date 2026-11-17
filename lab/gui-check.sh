#!/bin/sh
# Drive roomy's real window against real QEMU and a real guest. See gui_check.py.
cd "$(dirname "$0")/.." || exit 1
exec python3 lab/gui_check.py "$@"
