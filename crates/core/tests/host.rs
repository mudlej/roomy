//! What roomy finds out about the computer it runs on, on fake hosts.

mod common;

use common::FakeHost;
use roomy_core::host;
use std::path::Path;

/// Adds a graphics card the way the kernel lists one under /sys/class/drm.
fn add_card(host: &FakeHost, card: &str, render: &str, boot_display: bool, connectors: &[(&str, &str)]) {
    let drm = "/sys/class/drm";
    host.write(&format!("{drm}/{card}/device/boot_vga"), if boot_display { "1\n" } else { "0\n" });
    host.write(&format!("{drm}/{card}/device/drm/{render}/dev"), "226:128\n");
    host.write(&format!("{drm}/{render}/dev"), "226:128\n");
    for (connector, status) in connectors {
        host.write(&format!("{drm}/{card}-{connector}/status"), &format!("{status}\n"));
    }
}

#[test]
fn vm_graphics_go_to_the_card_the_monitor_is_on() {
    let host = FakeHost::new("gpus");
    host.write("/sys/class/drm/version", "drm 1.1.0\n");
    // The first card listed is the one built into the processor, with nothing plugged in.
    add_card(&host, "card0", "renderD129", false, &[("DP-1", "disconnected")]);
    add_card(&host, "card1", "renderD128", true, &[("DP-2", "disconnected"), ("HDMI-A-1", "connected")]);

    let all = host::gpus(&host.env());
    assert_eq!(all.len(), 2, "{all:?}");
    assert_eq!((all[0].card.as_str(), all[0].render_node.as_path()), ("card0", Path::new("/dev/dri/renderD129")));
    assert_eq!((all[0].has_monitor, all[0].boot_display), (false, false));
    assert_eq!((all[1].has_monitor, all[1].boot_display), (true, true));
    assert_eq!(host::display_gpu(&host.env()).unwrap().render_node, Path::new("/dev/dri/renderD128"));
}

#[test]
fn without_a_monitor_the_boot_card_is_used_and_otherwise_the_first() {
    let host = FakeHost::new("gpus2");
    assert_eq!(host::display_gpu(&host.env()), None, "a computer without graphics cards has none to choose");
    add_card(&host, "card0", "renderD128", false, &[]);
    add_card(&host, "card1", "renderD129", false, &[]);
    assert_eq!(host::display_gpu(&host.env()).unwrap().card, "card0");
    host.write("/sys/class/drm/card1/device/boot_vga", "1\n");
    assert_eq!(host::display_gpu(&host.env()).unwrap().card, "card1");
    // A monitor outweighs having shown the start-up screen.
    host.write("/sys/class/drm/card0-eDP-1/status", "connected\n");
    assert_eq!(host::display_gpu(&host.env()).unwrap().card, "card0");
}
