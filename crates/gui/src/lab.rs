//! Hooks for roomy's own test runs. Inactive unless `ROOMY_GUI_LAB` names a
//! directory. Then the window keeps a `facts.json` there describing what it
//! shows, and obeys these files a test script may drop into it:
//!
//!   shot       contains a name: save pictures `<name>-library.png` and `<name>-vm.png`
//!   type       contains text: type it into the VM, a line break being the Enter key
//!   action     contains the name of a VM window action to carry out (`settings`, `guest-setup`, ...),
//!              or `close-dialog`
//!   pointer    contains `<across> <down>` as fractions of the screen, then optionally `click`: point there
//!   clipboard  contains text: offer it to the guest as this computer's clipboard
//!   open       contains `downloads` or `downloads:<system>`: open the download window (showing
//!              that system); `shot` then also saves `<name>-downloads.png`. Or `create:<guest
//!              system>`: open the "New Virtual Machine" dialog as an image of that system leads to it
//!   stop       quit
//!
//! Each file is removed once obeyed. Also read at startup:
//!
//!   ROOMY_GUI_LAB_VM=<folder>    open this VM's window, and start it if it is off
//!   ROOMY_GUI_LAB_SECONDS=<n>    quit after this long regardless (default 900)

use crate::library::Library;
use adw::prelude::*;
use gtk::glib;
use roomy_core::control::State;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

pub fn directory() -> Option<PathBuf> {
    std::env::var_os("ROOMY_GUI_LAB").map(PathBuf::from)
}

/// Saves a picture of a window as it is drawn right now.
fn picture(window: &impl IsA<gtk::Widget>, path: &Path) -> Result<(), String> {
    let (width, height) = (window.width(), window.height());
    if width <= 0 || height <= 0 {
        return Err("the window has no size yet".to_owned());
    }
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, f64::from(width), f64::from(height));
    let node = snapshot.to_node().ok_or("the window drew nothing")?;
    let renderer = window.native().and_then(|native| native.renderer()).ok_or("the window is not on screen")?;
    renderer.render_texture(&node, None).save_to_png(path).map_err(|e| e.to_string())
}

/// Where the guest's screen has anything bright on it, as it is shown:
/// `[left, top, right, bottom]` as fractions of the picture, or `None` for an
/// all-dark screen. A text console that was just cleared has its text in the
/// top-left corner, so this tells a correct picture from a scrambled or
/// upside-down one without anyone looking.
fn bright_area(screen: &crate::screen::Screen) -> Option<[f64; 4]> {
    let texture = screen.picture()?;
    let (width, height) = (texture.width() as usize, texture.height() as usize);
    let mut pixels = vec![0u8; width * height * 4];
    texture.download(&mut pixels, width * 4);
    let (mut left, mut top, mut right, mut bottom) = (usize::MAX, usize::MAX, 0, 0);
    for y in (0..height).step_by(2) {
        let row = &pixels[y * width * 4..(y + 1) * width * 4];
        for x in (0..width).step_by(2) {
            // Blue, green, red: bright enough to be text rather than noise.
            if row[x * 4..x * 4 + 3].iter().any(|channel| *channel > 96) {
                let shown_y = if screen.upside_down() { height - 1 - y } else { y };
                (left, right) = (left.min(x), right.max(x));
                (top, bottom) = (top.min(shown_y), bottom.max(shown_y));
            }
        }
    }
    let fraction = |value: usize, of: usize| (value as f64 / of as f64 * 1000.0).round() / 1000.0;
    (left != usize::MAX)
        .then(|| [fraction(left, width), fraction(top, height), fraction(right, width), fraction(bottom, height)])
}

/// Linux input codes for the characters test runs type.
fn key_for(character: char) -> Option<u32> {
    const ROWS: [(&str, u32); 4] = [("1234567890", 2), ("qwertyuiop", 16), ("asdfghjkl", 30), ("zxcvbnm", 44)];
    match character {
        ' ' => Some(57),
        '\n' => Some(28),
        '-' => Some(12),
        _ => ROWS.iter().find_map(|(keys, first)| keys.find(character).map(|index| first + index as u32)),
    }
}

pub fn run(app: &adw::Application, library: &Rc<Library>, dir: PathBuf) {
    let (app, library) = (app.clone(), library.clone());
    glib::spawn_future_local(async move {
        let _ = std::fs::create_dir_all(&dir);
        let seconds: u64 = std::env::var("ROOMY_GUI_LAB_SECONDS").ok().and_then(|s| s.parse().ok()).unwrap_or(900);
        let started = Instant::now();
        let mut notes: Vec<String> = Vec::new();
        let mut first_frame_at = None::<f64>;
        let mut shots = serde_json::Map::new();
        let mut catalog: Option<std::rc::Rc<crate::downloads::Catalog>> = None;
        let take_as_written = |name: &str| -> Option<String> {
            let path = dir.join(name);
            let text = std::fs::read_to_string(&path).ok()?;
            let _ = std::fs::remove_file(&path);
            Some(text)
        };
        let take = |name: &str| take_as_written(name).map(|text| text.trim().to_owned());

        glib::timeout_future(Duration::from_millis(1500)).await;
        let machine = std::env::var_os("ROOMY_GUI_LAB_VM").and_then(|vm| library.open_machine(Path::new(&vm)));
        if let Some(machine) = &machine {
            // The state is asked for in the background; give the answer a moment to arrive.
            glib::timeout_future(Duration::from_millis(1500)).await;
            if matches!(machine.state(), State::Off | State::Suspended) {
                machine.start();
            }
        }

        while started.elapsed() < Duration::from_secs(seconds) {
            glib::timeout_future(Duration::from_millis(500)).await;
            let at = (started.elapsed().as_secs_f64() * 10.0).round() / 10.0;
            if let Some(name) = take("shot") {
                if let Err(e) = picture(library.window(), &dir.join(format!("{name}-library.png"))) {
                    notes.push(format!("{name}: library picture: {e}"));
                }
                if let Some(Err(e)) = machine.as_ref().map(|m| picture(m.window(), &dir.join(format!("{name}-vm.png"))))
                {
                    notes.push(format!("{name}: vm picture: {e}"));
                }
                if let Some(machine) = &machine {
                    shots.insert(name.clone(), json!({ "bright_area": bright_area(machine.screen()) }));
                }
                if let Some(Err(e)) =
                    catalog.as_ref().map(|c| picture(c.window(), &dir.join(format!("{name}-downloads.png"))))
                {
                    notes.push(format!("{name}: downloads picture: {e}"));
                }
            }
            if let Some(what) = take("open") {
                let mut words = what.splitn(2, ':');
                if words.next() == Some("downloads") {
                    let opened = library.open_downloads();
                    if let Some(system) = words.next() {
                        opened.select(system);
                    }
                    catalog = Some(opened);
                }
                if let Some(system) = what.strip_prefix("create:").and_then(roomy_core::catalog::known) {
                    library.new_vm_from(Some((PathBuf::from("/lab/installation-image.iso"), system)));
                }
            }
            if let (Some(action), Some(machine)) = (take("action"), &machine) {
                if action == "close-dialog" {
                    if let Some(dialog) = machine.window().visible_dialog() {
                        dialog.force_close();
                    }
                } else {
                    // Spelled out: a window is also a widget, whose `activate_action` works differently.
                    ActionGroupExt::activate_action(machine.window(), &action, None);
                }
            }
            if let (Some(place), Some(machine)) = (take("pointer"), &machine) {
                let mut words = place.split_whitespace();
                let mut fraction = || words.next().and_then(|word| word.parse::<f64>().ok());
                if let (Some(across), Some(down)) = (fraction(), fraction()) {
                    machine.lab_point(across, down, words.next() == Some("click"));
                }
            }
            if let (Some(text), Some(machine)) = (take("clipboard"), &machine) {
                machine.lab_offer_clipboard(&text);
            }
            if let (Some(text), Some(machine)) = (take_as_written("type"), &machine) {
                for code in text.chars().filter_map(key_for) {
                    machine.tap_key(code);
                    glib::timeout_future(Duration::from_millis(40)).await;
                }
            }
            let vm = machine.as_ref().map(|machine| {
                if machine.frames_drawn.get() > 0 && first_frame_at.is_none() {
                    first_frame_at = Some(at);
                }
                json!({
                    "state": machine.state().word(),
                    "frames_drawn": machine.frames_drawn.get(),
                    "screen_kind": machine.screen_kind.get(),
                    "screen_detail": machine.screen_detail.borrow().clone(),
                    "picture_size": machine.screen().picture_size(),
                    "problems": machine.problems.borrow().clone(),
                    "window_size": [machine.window().width(), machine.window().height()],
                    "window_visible": machine.window().is_visible(),
                    "shows": machine.lab_facts(),
                    // Set by the desktop when the window is covered or minimised; drawing pauses then.
                    "window_suspended": machine.window().is_suspended(),
                })
            });
            let facts = json!({
                "t": at,
                "doctor": library.doctor_verdict.borrow().clone(),
                "toolkit": format!("GTK {}.{}.{}", gtk::major_version(), gtk::minor_version(), gtk::micro_version()),
                "renderer": library.window().renderer().map(|renderer| renderer.type_().name().to_owned()),
                "graphics_devices_open": crate::graphics_devices_open(),
                "window_render_node": crate::window_render_node(),
                "library_window_size": [library.window().width(), library.window().height()],
                "first_frame_at": first_frame_at,
                "notes": notes,
                "shots": shots,
                "vm": vm,
                "downloads": catalog.as_ref().map(|catalog| catalog.lab_facts()),
            });
            // Written whole or not at all, so a reader never sees half a file.
            let partial = dir.join("facts.json.partial");
            if std::fs::write(&partial, format!("{facts:#}")).is_ok() {
                let _ = std::fs::rename(&partial, dir.join("facts.json"));
            }
            if take("stop").is_some() {
                break;
            }
        }
        app.quit();
    });
}
