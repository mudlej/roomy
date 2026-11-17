//! The settings dialog of one VM. Every group says when its changes take effect.

use crate::Context;
use crate::machine::SUSPEND_UNAVAILABLE_WITH_GPU;
use adw::prelude::*;
use gtk::{gio, glib};
use roomy_core::control::{self, State};
use roomy_core::settings::{self, Applied, Applies, Changes};
use roomy_core::vm::{Config, Folder, PortForward, SharedFolder};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

/// A name the guest can mount `folder` by: its last component, reduced to
/// letters, digits, '-' and '_', and different from the names already taken.
fn share_name(folder: &Path, taken: &[SharedFolder]) -> String {
    let raw = folder.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut base: String =
        raw.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).take(30).collect();
    if base.trim_matches('-').is_empty() {
        base = "shared".to_owned();
    }
    let free = |name: &str| !taken.iter().any(|share| share.name == name);
    if free(&base) {
        return base;
    }
    (2..).map(|n| format!("{base}-{n}")).find(|name| free(name)).unwrap_or(base)
}

/// One sentence for the user about what was changed and when it takes effect.
pub fn summary(applied: &[Applied]) -> String {
    let names =
        |when: Applies| -> Vec<&str> { applied.iter().filter(|a| a.applies == when).map(|a| a.setting).collect() };
    let (now, later) = (names(Applies::Now), names(Applies::NextBoot));
    let notes: Vec<&str> = applied.iter().filter_map(|a| a.note.as_deref()).collect();
    let mut parts = Vec::new();
    if applied.is_empty() {
        parts.push("Nothing was changed.".to_owned());
    }
    if !now.is_empty() {
        parts.push(format!("Changed: {}.", now.join(", ")));
    }
    if !later.is_empty() {
        parts.push(format!(
            "Saved for the next boot: {}. Shut the virtual machine down and start it to apply.",
            later.join(", ")
        ));
    }
    parts.extend(notes.iter().map(|note| (*note).to_owned()));
    parts.join(" ")
}

/// A list of removable rows inside a group, backed by a vector.
fn rebuild<T: Clone + 'static>(
    group: &adw::PreferencesGroup,
    rows: &Rc<RefCell<Vec<adw::ActionRow>>>,
    items: &Rc<RefCell<Vec<T>>>,
    describe: fn(&T) -> (String, String),
) {
    for row in rows.borrow_mut().drain(..) {
        group.remove(&row);
    }
    for (index, item) in items.borrow().iter().enumerate() {
        let (title, subtitle) = describe(item);
        let row = crate::plain_row(&title, &subtitle);
        row.set_subtitle_lines(1);
        let remove = gtk::Button::builder()
            .icon_name("list-remove-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text("Remove")
            .build();
        remove.add_css_class("flat");
        remove.connect_clicked({
            let (group, rows, items) = (group.clone(), rows.clone(), items.clone());
            move |_| {
                if index < items.borrow().len() {
                    items.borrow_mut().remove(index);
                }
                rebuild(&group, &rows, &items, describe);
            }
        });
        row.add_suffix(&remove);
        group.add(&row);
        rows.borrow_mut().push(row);
    }
}

/// Opens the dialog for the VM in `folder`. `done` is called with the summary once changes were saved.
pub fn show(context: &Rc<Context>, parent: &impl IsA<gtk::Widget>, folder: &Folder, done: impl Fn(String) + 'static) {
    let (env, for_load) = (context.env.clone(), folder.clone());
    let (context, parent, folder) = (context.clone(), parent.clone().upcast::<gtk::Widget>(), folder.clone());
    let done = Rc::new(done);
    glib::spawn_future_local(async move {
        // Reading the disk's size runs a program; keep that off the window's thread.
        let loaded = gio::spawn_blocking(move || {
            let config = for_load.load().map_err(|e| e.to_string())?;
            let disk = settings::disk_gb(&env, &for_load, &config).map_err(|e| e.to_string())?;
            Ok::<_, String>((config, disk, control::status(&env, &for_load)))
        })
        .await;
        match loaded {
            Ok(Ok((config, disk_gb, state))) => build(&context, &parent, &folder, config, disk_gb, state, done),
            Ok(Err(problem)) => {
                let dialog = adw::AlertDialog::new(Some("The settings could not be opened"), Some(&problem));
                dialog.add_responses(&[("close", "Close")]);
                dialog.present(Some(&parent));
            }
            Err(_) => {}
        }
    });
}

fn build(
    context: &Rc<Context>,
    parent: &gtk::Widget,
    folder: &Folder,
    config: Config,
    disk_gb: u64,
    state: State,
    done: Rc<dyn Fn(String)>,
) {
    let host_cpus = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let off = state == State::Off;
    let later = if off {
        "Used the next time the virtual machine starts."
    } else {
        "Takes effect the next time the virtual machine is shut down and started. Restarting from inside the guest is not enough."
    };

    let name = adw::EntryRow::builder().title("Name").text(config.name.as_str()).build();
    let network = adw::SwitchRow::builder()
        .title("Network")
        .subtitle("Internet access through this computer")
        .active(config.features.network)
        .build();
    let clipboard = adw::SwitchRow::builder()
        .title("Clipboard sharing")
        .subtitle("Copy and paste text between the guest and this computer")
        .active(config.features.clipboard)
        .build();
    let microphone = adw::SwitchRow::builder()
        .title("Microphone")
        .subtitle("Let the guest hear this computer's microphone")
        .active(config.features.microphone)
        .build();
    let disk = adw::SpinRow::with_range(disk_gb as f64, 16384.0, 8.0);
    disk.set_title("Disk (GB)");
    disk.set_subtitle("Can only grow. Afterwards the guest has to grow its own partition to use the space.");
    disk.set_value(disk_gb as f64);
    disk.set_sensitive(state != State::Suspended);
    let immediate = adw::PreferencesGroup::builder().title("Takes effect immediately").build();
    for row in [
        name.upcast_ref::<gtk::Widget>(),
        network.upcast_ref(),
        clipboard.upcast_ref(),
        microphone.upcast_ref(),
        disk.upcast_ref(),
    ] {
        immediate.add(row);
    }

    let cpus = adw::SpinRow::with_range(1.0, f64::from(host_cpus.max(config.cpus)), 1.0);
    cpus.set_title("Processors");
    cpus.set_value(f64::from(config.cpus));
    let memory = adw::SpinRow::with_range(256.0, 1_048_576.0, 256.0);
    memory.set_title("Memory (MB)");
    memory.set_value(config.memory_mb as f64);
    let gpu = adw::SwitchRow::builder()
        .title("GPU acceleration")
        .subtitle(SUSPEND_UNAVAILABLE_WITH_GPU)
        .active(config.features.gpu_acceleration)
        .build();
    let hardware = adw::PreferencesGroup::builder().title("Hardware").description(later).build();
    hardware.add(&cpus);
    hardware.add(&memory);
    hardware.add(&gpu);

    let shares = Rc::new(RefCell::new(config.shared_folders.clone()));
    let share_rows = Rc::new(RefCell::new(Vec::new()));
    let describe_share: fn(&SharedFolder) -> (String, String) =
        |share| (share.name.clone(), share.path.display().to_string());
    let add_share = gtk::Button::builder()
        .icon_name("list-add-symbolic")
        .valign(gtk::Align::Center)
        .tooltip_text("Share a folder")
        .build();
    add_share.add_css_class("flat");
    let shared = adw::PreferencesGroup::builder()
        .title("Shared folders")
        .description(format!("{later} See Guest Setup for how to open them inside the guest."))
        .header_suffix(&add_share)
        .build();
    rebuild(&shared, &share_rows, &shares, describe_share);

    let forwards = Rc::new(RefCell::new(config.port_forwards.clone()));
    let forward_rows = Rc::new(RefCell::new(Vec::new()));
    let describe_forward: fn(&PortForward) -> (String, String) = |forward| {
        (format!("Port {} of this computer", forward.host), format!("reaches port {} of the guest", forward.guest))
    };
    let new_forward = adw::EntryRow::builder()
        .title("Add: this computer's port, a colon, the guest's port (2222:22)")
        .show_apply_button(true)
        .build();
    let forwarded = adw::PreferencesGroup::builder()
        .title("Forwarded ports")
        .description(format!("Reachable from this computer only. {later}"))
        .build();
    forwarded.add(&new_forward);
    rebuild(&forwarded, &forward_rows, &forwards, describe_forward);

    let page = adw::PreferencesPage::new();
    for group in [&immediate, &hardware, &shared, &forwarded] {
        page.add(group);
    }
    let cancel = gtk::Button::with_label("Cancel");
    let apply = gtk::Button::with_label("Apply");
    apply.add_css_class("suggested-action");
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    header.pack_start(&cancel);
    header.pack_end(&apply);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&page));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));
    let dialog = adw::Dialog::builder()
        .title(format!("Settings of {}", config.name))
        .content_width(600)
        .content_height(760)
        .child(&toolbar)
        .build();
    let complain = {
        let toasts = toasts.clone();
        move |message: &str| {
            // Messages carry paths and other programs' words: shown as they are, never read as markup.
            let toast = adw::Toast::builder().use_markup(false).title(message).timeout(0).build();
            toasts.add_toast(toast);
        }
    };

    cancel.connect_clicked({
        let dialog = dialog.clone();
        move |_| {
            dialog.close();
        }
    });
    add_share.connect_clicked({
        let (shared, share_rows, shares, dialog) = (shared.clone(), share_rows.clone(), shares.clone(), dialog.clone());
        move |_| {
            let chooser = gtk::FileDialog::builder().title("Choose a Folder to Share").build();
            let (shared, share_rows, shares) = (shared.clone(), share_rows.clone(), shares.clone());
            let window = dialog.root().and_downcast::<gtk::Window>();
            chooser.select_folder(window.as_ref(), gio::Cancellable::NONE, move |chosen| {
                let Some(path) = chosen.ok().and_then(|file| file.path()) else { return };
                if shares.borrow().iter().any(|share| share.path == path) {
                    return;
                }
                let name = share_name(&path, &shares.borrow());
                shares.borrow_mut().push(SharedFolder { name, path });
                rebuild(&shared, &share_rows, &shares, describe_share);
            });
        }
    });
    new_forward.connect_apply({
        let (forwarded, forward_rows, forwards, complain) =
            (forwarded.clone(), forward_rows.clone(), forwards.clone(), complain.clone());
        move |entry| {
            let text = entry.text();
            let ports = text
                .split_once(':')
                .and_then(|(host, guest)| Some((host.trim().parse::<u16>().ok()?, guest.trim().parse::<u16>().ok()?)));
            match ports {
                Some((host, guest)) if host != 0 && guest != 0 => {
                    forwards.borrow_mut().retain(|forward| forward.host != host);
                    forwards.borrow_mut().push(PortForward { host, guest });
                    rebuild(&forwarded, &forward_rows, &forwards, describe_forward);
                    entry.set_text("");
                }
                _ => complain("Write two port numbers with a colon between them, for example 2222:22"),
            }
        }
    });

    apply.connect_clicked({
        let (context, folder, dialog) = (context.clone(), folder.clone(), dialog.clone());
        move |apply| {
            let changes = Changes {
                name: Some(name.text().to_string()),
                cpus: Some(cpus.value() as u32),
                memory_mb: Some(memory.value() as u64),
                gpu_acceleration: Some(gpu.is_active()),
                network: Some(network.is_active()),
                clipboard: Some(clipboard.is_active()),
                microphone: Some(microphone.is_active()),
                shared_folders: Some(shares.borrow().clone()),
                port_forwards: Some(forwards.borrow().clone()),
                disk_gb: Some(disk.value() as u64),
            };
            let (env, folder) = (context.env.clone(), folder.clone());
            apply.set_sensitive(false);
            let (apply, dialog, complain, done) = (apply.clone(), dialog.clone(), complain.clone(), done.clone());
            glib::spawn_future_local(async move {
                let outcome =
                    gio::spawn_blocking(move || settings::apply(&env, &folder, &changes).map_err(|e| e.to_string()))
                        .await;
                match outcome {
                    Ok(Ok(applied)) => {
                        dialog.close();
                        done(summary(&applied));
                    }
                    Ok(Err(problem)) => {
                        apply.set_sensitive(true);
                        complain(&problem);
                    }
                    Err(_) => apply.set_sensitive(true),
                }
            });
        }
    });
    dialog.present(Some(parent));
}
