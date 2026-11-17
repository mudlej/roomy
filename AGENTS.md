# Agent notes

- Use only `just` recipes (`just` lists them). No loose shell scripts: new commands become recipes in `justfile`;
  `first_time_setup.sh` is the one script, for bootstrapping.
- Everything produced locally goes under `work/` (single-file release in `work/dist`, lab runs in `work/lab`) or
  `target/`; both are ignored by git.
- `just check` must pass before handing work back. Where GTK 4 is not installed (most sandboxes), run
  `just check-headless` instead and say that the window was only type-checked.
- A sandbox cannot run VMs or the window. The `lab-*` recipes do that on a real machine, inside the distrobox
  (`distrobox.ini`); each run writes `work/lab/results/<kind>-<time>/SUMMARY.txt` and `report.json`. Read those
  rather than guessing.
- `README.md` describes what roomy does and its user-facing commands. Keep it in step with behaviour.
- Comments only where they explain why. Tests exercise behaviour, with stand-ins for QEMU under
  `crates/*/tests/fixtures`.
