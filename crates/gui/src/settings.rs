//! The settings dialog of one VM. Every group says when its changes take effect.

use crate::Context;
use crate::fields::{Amount, Holder, PortForwards, SHARED_FOLDERS_IN_THE_GUEST, SharedFolders};
use crate::machine::SUSPEND_UNAVAILABLE_WITH_GPU;
use adw::prelude::*;
use gtk::{gio, glib};
use roomy_core::control::{self, State};
use roomy_core::host;
use roomy_core::settings::{self, Applied, Applies, Changes};
use roomy_core::vm::{Config, Folder};
use std::rc::Rc;

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
            let free = host::free_space_gb(for_load.dir());
            Ok::<_, String>((config, disk, free, control::status(&env, &for_load)))
        })
        .await;
        match loaded {
            Ok(Ok((config, disk_gb, free_gb, state))) => {
                build(&context, &parent, &folder, config, (disk_gb, free_gb), state, done)
            }
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
    (disk_gb, free_gb): (u64, Option<u64>),
    state: State,
    done: Rc<dyn Fn(String)>,
) {
    let host_cpus = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let host_memory_gb = (host::memory_mb(&context.env).unwrap_or(8192) / 1024).max(1);
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
    // A disk only grows, and by no more than there is room for where it is kept.
    let disk =
        Amount::new("Disk (GB)", disk_gb as f64, free_gb.map_or(16384.0, |free| (disk_gb + free) as f64), 1.0, 0);
    disk.set_value(disk_gb as f64);
    let room = free_gb.map(|free| format!(" {free} GB are free where it is kept.")).unwrap_or_default();
    disk.set_note(&format!(
        "Can only grow.{room} Afterwards the guest has to grow its own partition to use the space."
    ));
    disk.set_sensitive(state != State::Suspended);
    let immediate = adw::PreferencesGroup::builder().title("Takes effect immediately").build();
    for row in [
        name.upcast_ref::<gtk::Widget>(),
        network.upcast_ref(),
        clipboard.upcast_ref(),
        microphone.upcast_ref(),
        disk.row.upcast_ref(),
    ] {
        immediate.add(row);
    }

    let cpus = Amount::new("Processors", 1.0, f64::from(host_cpus.max(config.cpus)), 1.0, 0);
    cpus.set_value(f64::from(config.cpus));
    cpus.set_note(&format!("This computer has {host_cpus}"));
    let memory_gb = config.memory_mb as f64 / 1024.0;
    let memory = Amount::new("Memory (GB)", 0.5, (host_memory_gb as f64).max(memory_gb.ceil()), 0.5, 1);
    memory.set_value(memory_gb);
    // The slider moves in half GBs. A size that is not one is kept as it is unless the slider is moved.
    let memory_as_shown = memory.value();
    let exactly = if memory_as_shown == memory_gb { String::new() } else { format!(" Now {} MB.", config.memory_mb) };
    memory.set_note(&format!("This computer has {host_memory_gb} GB.{exactly}"));
    let gpu = adw::SwitchRow::builder()
        .title("GPU acceleration")
        .subtitle(SUSPEND_UNAVAILABLE_WITH_GPU)
        .active(config.features.gpu_acceleration)
        .build();
    let hardware = adw::PreferencesGroup::builder().title("Hardware").description(later).build();
    hardware.add(&cpus.row);
    hardware.add(&memory.row);
    hardware.add(&gpu);

    let shared = SharedFolders::new(config.shared_folders.clone(), &format!("{later} {SHARED_FOLDERS_IN_THE_GUEST}"));
    let forwarded = adw::PreferencesGroup::builder()
        .title("Forwarded ports")
        .description(format!("Reachable from this computer only. {later}"))
        .build();

    let page = adw::PreferencesPage::new();
    for group in [&immediate, &hardware, &shared.group, &forwarded] {
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
    let forwards = PortForwards::new(Holder::Group(forwarded.clone()), config.port_forwards.clone(), complain.clone());

    apply.connect_clicked({
        let (context, folder, dialog) = (context.clone(), folder.clone(), dialog.clone());
        move |apply| {
            let changes = Changes {
                name: Some(name.text().to_string()),
                cpus: Some(cpus.value() as u32),
                memory_mb: Some(if memory.value() == memory_as_shown {
                    config.memory_mb
                } else {
                    (memory.value() * 1024.0).round() as u64
                }),
                gpu_acceleration: Some(gpu.is_active()),
                network: Some(network.is_active()),
                clipboard: Some(clipboard.is_active()),
                microphone: Some(microphone.is_active()),
                shared_folders: Some(shared.get()),
                port_forwards: Some(forwards.get()),
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
