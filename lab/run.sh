#!/bin/sh
# Run the roomy lab experiments. See probe.py for options.
cd "$(dirname "$0")" || exit 1
exec python3 probe.py "$@"
