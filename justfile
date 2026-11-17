# roomy: the command front door. Run `just` to list recipes.
#
# Everything a recipe produces lands in target/ (cargo) or work/ (the single-file
# release, downloaded tools, lab runs); git ignores both.

set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments

root := justfile_directory()
dist := root / "work" / "dist"
tools := root / "work" / "tools"

default:
    @just --list --unsorted

# ------------------------------------------------------------------- daily loop --

# Build and start roomy: no arguments opens the window, `just run doctor`, `just run vm list`, ...
run *args: build
    @"{{ root }}/target/release/roomy" "$@"

# Check this computer for everything roomy needs.
doctor: build
    @"{{ root }}/target/release/roomy" doctor

# --------------------------------------------------------------------- quality --

# Fetch the dependencies named in Cargo.lock.
setup:
    cargo fetch --locked

# The full local gate: formatting, lints, every test. Needs GTK 4 and libadwaita (the box has them).
check: format-check lint test

# Where GTK 4 is not installed, the window can only be type-checked, against the stand-in
# library descriptions in lab/typecheck, and its crate is left out of the tests.
#
# The same gate for a machine without the window toolkit (type-checks the window, skips its tests).
check-headless: format-check
    PKG_CONFIG_PATH="{{ root }}/lab/typecheck" cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace --exclude roomy-gui

# Apply formatting.
format:
    cargo fmt --all

# Formatting check (CI form).
format-check:
    cargo fmt --all --check

# Lints, warnings fatal.
lint:
    cargo clippy --workspace --all-targets -- -D warnings

# Every crate's tests.
test:
    cargo test --workspace

# ---------------------------------------------------------------------- builds --

# Release build of the `roomy` command and its window.
build:
    cargo build --release --workspace

# The single-file release: work/dist/roomy-x86_64.AppImage (the command and the window, for any recent distribution).
appimage: build
    #!/usr/bin/env bash
    set -euo pipefail
    # Pinned, with checksums: what goes into the file must not change behind our back.
    tool_version=1.9.1
    tool_sha256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0
    runtime_version=20251108
    runtime_sha256=2fca8b443c92510f1483a883f60061ad09b46b978b2631c807cd873a47ec260d
    id=com.mudlej.Roomy
    programs="${ROOMY_APPIMAGE_PROGRAMS:-{{ root }}/target/release}"
    appdir="{{ dist }}/AppDir"
    out="{{ dist }}/roomy-x86_64.AppImage"

    fetch() { # <url> <file> <sha256>
      [[ -f "$2" ]] && echo "$3  $2" | sha256sum -c --quiet 2>/dev/null && return 0
      mkdir -p "$(dirname "$2")"
      curl -fsSL -o "$2.part" "$1"
      echo "$3  $2.part" | sha256sum -c --quiet || { echo "appimage: checksum mismatch for $1" >&2; exit 1; }
      mv "$2.part" "$2"
    }
    fetch "https://github.com/AppImage/appimagetool/releases/download/$tool_version/appimagetool-x86_64.AppImage" \
      "{{ tools }}/appimagetool-$tool_version" "$tool_sha256"
    fetch "https://github.com/AppImage/type2-runtime/releases/download/$runtime_version/runtime-x86_64" \
      "{{ tools }}/runtime-$runtime_version" "$runtime_sha256"
    chmod +x "{{ tools }}/appimagetool-$tool_version"

    rm -rf "$appdir"
    mkdir -p "$appdir/usr/bin" "$appdir/usr/share/applications" "$appdir/usr/share/icons/hicolor/scalable/apps"
    install -m755 "$programs/roomy" "$programs/roomy-gui" "$appdir/usr/bin/"
    install -m644 "{{ root }}/packaging/$id.desktop" "$appdir/usr/share/applications/$id.desktop"
    install -m644 "{{ root }}/packaging/$id.svg" "$appdir/usr/share/icons/hicolor/scalable/apps/$id.svg"
    install -Dm644 "{{ root }}/LICENSE" "$appdir/usr/share/licenses/roomy/LICENSE"
    cp "{{ root }}/packaging/$id.desktop" "{{ root }}/packaging/$id.svg" "$appdir/"
    ln -s "$id.svg" "$appdir/.DirIcon"
    # The file starts the `roomy` command directly: no wrapper script, and nothing added to the
    # environment that QEMU or other programs started from here would inherit.
    ln -s usr/bin/roomy "$appdir/AppRun"

    ARCH=x86_64 APPIMAGE_EXTRACT_AND_RUN=1 "{{ tools }}/appimagetool-$tool_version" --no-appstream \
      --runtime-file "{{ tools }}/runtime-$runtime_version" "$appdir" "$out" >"{{ dist }}/appimagetool.log" 2>&1 \
      || { echo "appimage: appimagetool failed; see {{ dist }}/appimagetool.log" >&2; exit 1; }
    chmod +x "$out"
    echo "$out ($(du -h "$out" | cut -f1))"
    if command -v objdump >/dev/null 2>&1; then
      needs="$(objdump -T "$appdir/usr/bin/roomy" "$appdir/usr/bin/roomy-gui" 2>/dev/null | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -n 1 || true)"
      [[ -z "$needs" ]] || echo "runs on systems with glibc ${needs#GLIBC_} or newer"
    fi

# The Arch package, built from this checkout into work/dist (install it with `pacman -U`).
package-arch:
    mkdir -p "{{ dist }}"
    cd "{{ root }}/packaging/arch" && PKGDEST="{{ dist }}" BUILDDIR="{{ root }}/work/makepkg" makepkg -f

# ------------------------------------------------------------------------- lab --
# Runs against real QEMU and a real guest, inside the box. Each run writes what it saw to
# work/lab/results/<kind>-<time>/ (SUMMARY.txt first) and points work/lab/results/latest-<kind> at it.

# First experiments against QEMU itself; also downloads the guest image the other lab recipes use.
lab-probe *args:
    @python3 "{{ root }}/lab/probe.py" "$@"

# The `roomy` command through a VM's whole life, about 6 minutes.
lab-engine:
    @python3 "{{ root }}/lab/engine_check.py"

# The window, driven for real. mode: (none) everything, about 10 minutes | quick | desktop | appimage.
lab-gui mode="":
    @cd "{{ root }}" && python3 lab/gui_check.py {{ mode }}

# The single-file release on this distribution: roomy installs what it needs, then the quick checks (run inside a roomy-<distro> box).
lab-distro:
    @cd "{{ root }}" && python3 lab/gui_check.py distro

# Which graphics card pairing shows a GPU-accelerated VM correctly.
lab-gpu:
    @cd "{{ root }}" && python3 lab/gpu_check.py
