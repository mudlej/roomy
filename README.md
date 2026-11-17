# roomy

**Virtual machines for the Linux desktop that just work.**

[![CI](https://github.com/mudlej/roomy/actions/workflows/ci.yml/badge.svg)](https://github.com/mudlej/roomy/actions/workflows/ci.yml)
[![Latest release](https://img.shields.io/github/v/release/mudlej/roomy)](https://github.com/mudlej/roomy/releases/latest)
[![Licence: GPL v3](https://img.shields.io/badge/licence-GPLv3-blue)](LICENSE)

roomy runs other Linux systems in a window on your Linux computer, using KVM and QEMU.

<img src="https://github.com/user-attachments/assets/2ebf6fbf-7d4e-4c91-b812-930bae57ab1d" width="100%" alt="roomy's list of virtual machines, next to the window of a running Fedora guest">

<table>
  <tr>
    <td width="50%"><img src="https://github.com/user-attachments/assets/8b20f746-59c5-4a51-8138-3264803b65a9" alt="Creating a virtual machine: name, guest system, installation image, and sliders for processors, memory and disk"></td>
    <td width="50%"><img src="https://github.com/user-attachments/assets/947c8c09-2d38-42cf-acb4-6e9a696906b1" alt="Creating a virtual machine: GPU acceleration, network, clipboard and microphone switches, and a shared folder with its place in the guest"></td>
  </tr>
</table>

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
