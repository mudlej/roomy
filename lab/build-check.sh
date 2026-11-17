#!/bin/sh
# Build roomy, run its tests and its doctor on this machine, and save the
# output under lab/results/ so it can be reviewed afterwards.
cd "$(dirname "$0")/.." || exit 1
out="lab/results/build-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$out"
ln -sfn "$(basename "$out")" lab/results/latest-build

{ rustc --version; cargo --version; } > "$out/toolchain.txt" 2>&1
cargo test 2>&1 | tee "$out/test.log"
cargo clippy --all-targets > "$out/clippy.log" 2>&1
cargo build --release 2>&1 | tail -n 3 | tee "$out/build.log"

./target/release/roomy doctor --report-file "$out/doctor.txt"
echo "doctor exit code: $?" | tee "$out/doctor-exit.txt"
./target/release/roomy doctor --json > "$out/doctor.json" 2>"$out/doctor-json.err"
# What setup would do here, without changing anything (no terminal on stdin means it only asks).
./target/release/roomy setup < /dev/null > "$out/setup-dry.txt" 2>&1
echo
echo "Saved to $out"
