//! The "New Virtual Machine" dialog.

use crate::Context;
use crate::fields::{
    Amount, FeatureSwitches, Form, Holder, PortForwards, SHARED_FOLDERS_IN_THE_GUEST, SharedFolders, SystemRow,
    host_size,
};
use adw::prelude::*;
use gtk::{gio, glib};
use roomy_core::catalog::{self, GuestSystem};
use roomy_core::vm::{self, Features, Folder, NewVm};
use roomy_core::{host, library};
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

const DISK_STARTS_EMPTY: &str = "None: the disk starts empty";

/// Offers as much disk as there is room for where the VM will be kept.
fn offer_disk(disk: &Amount, location: &Path) {
    const THIN: &str = "Space is only used as the virtual machine fills it.";
    match host::free_space_gb(location) {
        Some(free) => {
            disk.set_max(free as f64);
            disk.set_note(&format!("{free} GB are free there. {THIN}"));
        }
        None => {
            disk.set_max(4096.0);
            disk.set_note(THIN);
        }
    }
}

/// A file chooser limited to files ending in `suffix`.
fn chooser_for(title: &str, kind: &str, suffix: &str) -> gtk::FileDialog {
    let filter = gtk::FileFilter::new();
    filter.set_name(Some(kind));
    filter.add_suffix(suffix);
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    gtk::FileDialog::builder().title(title).filters(&filters).build()
}

/// Shows the dialog, starting from an installation image and the guest system it holds when
/// `start` names them. `created` is called with the new VM's folder.
pub fn show(
    context: &Rc<Context>,
    parent: &impl IsA<gtk::Widget>,
    start: Option<(PathBuf, GuestSystem)>,
    created: impl Fn(Folder) + 'static,
) {
    let (host_cpus, host_memory_gb) = host_size(&context.env);
    let page = adw::PreferencesPage::new();
    let form = Form::new("New Virtual Machine", "Create", &page);
    let complain = form.complainer();
    let (dialog, create) = (form.dialog, form.confirm);
    create.set_sensitive(false);

    let name = adw::EntryRow::builder().title("Name").build();
    let system = SystemRow::new(catalog::LINUX);

    let image_path: Rc<RefCell<Option<PathBuf>>> = Rc::default();
    let image = crate::plain_row("Installation image", "None: start without one");
    image.set_subtitle_lines(1);
    let download_image = gtk::Button::builder().label("Download…").valign(gtk::Align::Center).build();
    let choose_image = gtk::Button::builder().label("Choose…").valign(gtk::Align::Center).build();
    image.add_suffix(&download_image);
    image.add_suffix(&choose_image);
    // Takes an image, and the guest system it holds when that is known.
    let use_image = {
        let (image, image_path, system) = (image.clone(), image_path.clone(), system.clone());
        move |path: PathBuf, guest: Option<GuestSystem>| {
            image.set_subtitle(&path.display().to_string());
            image_path.replace(Some(path));
            if let Some(guest) = guest {
                system.choose(guest);
            }
        }
    };
    if let Some((path, guest)) = start {
        use_image(path, Some(guest));
    }

    let location_path = Rc::new(RefCell::new(library::default_vm_parent().unwrap_or_else(|_| PathBuf::from("."))));
    let location = crate::plain_row("Location", &location_path.borrow().display().to_string());
    location.set_subtitle_lines(1);
    let choose_location = gtk::Button::builder().label("Change…").valign(gtk::Align::Center).build();
    location.add_suffix(&choose_location);

    let general = adw::PreferencesGroup::new();
    general.add(&name);
    general.add(&system.row);
    general.add(&image);
    general.add(&location);

    let cpus = Amount::new("Processors", 1.0, f64::from(host_cpus), 1.0, 0);
    cpus.set_value(f64::from((host_cpus / 2).clamp(1, 4)));
    cpus.set_note(&format!("This computer has {host_cpus}"));
    let memory = Amount::new("Memory (GB)", 1.0, host_memory_gb as f64, 0.5, 1);
    memory.set_value((host_memory_gb / 2).clamp(1, 4) as f64);
    memory.set_note(&format!("This computer has {host_memory_gb} GB"));
    let disk = Rc::new(Amount::new("Disk (GB)", 8.0, 4096.0, 1.0, 0));
    offer_disk(&disk, &location_path.borrow());
    disk.set_value(64.0);
    let hardware = adw::PreferencesGroup::builder()
        .title("Hardware")
        .description("Processors and memory can be changed later, and the disk can be made larger.")
        .build();
    hardware.add(&cpus.row);
    hardware.add(&memory.row);
    hardware.add(&disk.row);

    let switches = Rc::new(FeatureSwitches::new(&Features::default()));
    let features = adw::PreferencesGroup::builder().title("Features").build();
    features.add(&switches.gpu);
    features.add(&switches.network);
    features.add(&switches.clipboard);
    features.add(&switches.microphone);

    let shared = Rc::new(SharedFolders::new(Vec::new(), SHARED_FOLDERS_IN_THE_GUEST));

    let existing_path: Rc<RefCell<Option<PathBuf>>> = Rc::default();
    let existing = crate::plain_row("Start from an existing disk", DISK_STARTS_EMPTY);
    existing.set_subtitle_lines(2);
    let forget_existing = gtk::Button::builder()
        .icon_name("edit-clear-symbolic")
        .valign(gtk::Align::Center)
        .tooltip_text("Start with an empty disk")
        .visible(false)
        .build();
    forget_existing.add_css_class("flat");
    let choose_existing = gtk::Button::builder().label("Choose…").valign(gtk::Align::Center).build();
    existing.add_suffix(&forget_existing);
    existing.add_suffix(&choose_existing);
    let advanced = adw::ExpanderRow::builder()
        .title("Advanced")
        .subtitle("An existing disk to start from, forwarded ports")
        .build();
    advanced.add_row(&existing);
    let forwards = PortForwards::new(Holder::Expander(advanced.clone()), Vec::new(), complain.clone());
    let advanced_group = adw::PreferencesGroup::new();
    advanced_group.add(&advanced);

    system.on_choice({
        let (switches, shared) = (switches.clone(), shared.clone());
        move |system| {
            switches.fit(&system, |_| false);
            shared.fit(&system, |_| false);
        }
    });

    page.add(&general);
    page.add(&hardware);
    page.add(&features);
    page.add(&shared.group);
    page.add(&advanced_group);

    name.connect_changed({
        let create = create.clone();
        move |name| create.set_sensitive(!name.text().trim().is_empty())
    });

    choose_image.connect_clicked({
        let use_image = use_image.clone();
        move |button| {
            let chooser = chooser_for("Choose an Installation Image", "Installation images (.iso)", "iso");
            let use_image = use_image.clone();
            let window = button.root().and_downcast::<gtk::Window>();
            chooser.open(window.as_ref(), gio::Cancellable::NONE, move |chosen| {
                if let Some(path) = chosen.ok().and_then(|file| file.path()) {
                    use_image(path, None);
                }
            });
        }
    });
    download_image.connect_clicked({
        let (context, system) = (context.clone(), system.clone());
        move |button| {
            let use_image = use_image.clone();
            let window = button.root().and_downcast::<gtk::Window>();
            let family = Some(system.chosen().family);
            crate::downloads::show(&context, window.as_ref(), family, "Use This Image", move |path, guest| {
                use_image(path, Some(guest))
            });
        }
    });
    choose_location.connect_clicked({
        let (location, location_path, disk) = (location.clone(), location_path.clone(), disk.clone());
        move |button| {
            let chooser = gtk::FileDialog::builder().title("Choose Where to Keep the Virtual Machine").build();
            let (location, location_path, disk) = (location.clone(), location_path.clone(), disk.clone());
            let window = button.root().and_downcast::<gtk::Window>();
            chooser.select_folder(window.as_ref(), gio::Cancellable::NONE, move |chosen| {
                if let Some(path) = chosen.ok().and_then(|file| file.path()) {
                    location.set_subtitle(&path.display().to_string());
                    offer_disk(&disk, &path);
                    location_path.replace(path);
                }
            });
        }
    });
    choose_existing.connect_clicked({
        let (existing, existing_path, forget_existing) =
            (existing.clone(), existing_path.clone(), forget_existing.clone());
        move |button| {
            let chooser = chooser_for("Choose a Disk Image to Start From", "Disk images (.qcow2)", "qcow2");
            let (existing, existing_path, forget_existing) =
                (existing.clone(), existing_path.clone(), forget_existing.clone());
            let window = button.root().and_downcast::<gtk::Window>();
            chooser.open(window.as_ref(), gio::Cancellable::NONE, move |chosen| {
                if let Some(path) = chosen.ok().and_then(|file| file.path()) {
                    existing.set_subtitle(&format!(
                        "A copy of {}. The original is not changed; a copy larger than the disk size above keeps its size.",
                        path.display()
                    ));
                    existing_path.replace(Some(path));
                    forget_existing.set_visible(true);
                }
            });
        }
    });
    forget_existing.connect_clicked({
        let (existing, existing_path) = (existing.clone(), existing_path.clone());
        move |button| {
            existing.set_subtitle(DISK_STARTS_EMPTY);
            existing_path.replace(None);
            button.set_visible(false);
        }
    });

    let created = Rc::new(created);
    create.connect_clicked({
        let (context, dialog) = (context.clone(), dialog.clone());
        move |create| {
            let new = NewVm {
                name: name.text().trim().to_owned(),
                os: system.chosen().id.to_owned(),
                cpus: cpus.value() as u32,
                memory_mb: (memory.value() * 1024.0).round() as u64,
                disk_gb: disk.value() as u64,
                install_image: image_path.borrow().clone(),
                existing_disk: existing_path.borrow().clone(),
                features: switches.get(),
                shared_folders: shared.get(),
                port_forwards: forwards.get(),
            };
            let parent = location_path.borrow().clone();
            let (env, library_file) = (context.env.clone(), context.library.clone());
            // Creating the disk can take a moment; keep the window responsive and the button from being pressed twice.
            create.set_sensitive(false);
            let (create, dialog, complain, created) =
                (create.clone(), dialog.clone(), complain.clone(), created.clone());
            glib::spawn_future_local(async move {
                let outcome = gio::spawn_blocking(move || {
                    let folder = vm::create(&env, &parent, new).map_err(|e| e.to_string())?;
                    // The VM exists either way; say so if it could not be listed.
                    let listed = library::add(&library_file, folder.dir()).map_err(|e| e.to_string());
                    Ok::<_, String>((folder, listed))
                })
                .await;
                match outcome {
                    Ok(Ok((folder, listed))) => {
                        if let Err(e) = listed {
                            eprintln!(
                                "roomy: the virtual machine was created but could not be added to the library: {e}"
                            );
                        }
                        dialog.close();
                        created(folder);
                    }
                    Ok(Err(e)) => {
                        create.set_sensitive(true);
                        complain(&e);
                    }
                    Err(_) => create.set_sensitive(true),
                }
            });
        }
    });
    dialog.present(Some(parent));
}
