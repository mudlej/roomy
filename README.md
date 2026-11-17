# roomy

**Virtual machines for the Linux desktop that just work.**

[![CI](https://github.com/mudlej/roomy/actions/workflows/ci.yml/badge.svg)](https://github.com/mudlej/roomy/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/mudlej/roomy)](https://github.com/mudlej/roomy/releases/latest)
[![Licence: GPL v3](https://img.shields.io/badge/licence-GPLv3-blue)](LICENSE)

![roomy](DESCRIPTION_IMAGE)

roomy runs other Linux systems in a window on your Linux computer, using KVM and QEMU.

## Getting started

Download `roomy-x86_64.AppImage` from the [latest release](https://github.com/mudlej/roomy/releases/latest), then:

    chmod +x roomy-x86_64.AppImage
    ./roomy-x86_64.AppImage

If something roomy needs is missing, it offers to install it. From a terminal:

    ./roomy-x86_64.AppImage doctor     # check this computer
    ./roomy-x86_64.AppImage setup      # install what is missing

## Building from source

    ./first_time_setup.sh
    just run        # build and open the window
    just check      # formatting, lints and tests
    just appimage   # the single-file release, in work/dist

## Licence

GPL-3.0-or-later. See [LICENSE](LICENSE).
