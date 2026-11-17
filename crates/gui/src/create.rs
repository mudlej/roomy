//! The "New Virtual Machine" dialog.

use crate::Context;
use crate::machine::SUSPEND_UNAVAILABLE_WITH_GPU;
use adw::prelude::*;
use gtk::{gio, glib};
use roomy_core::vm::{self, Features, Folder, NewVm};
use roomy_core::{catalog, library};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

/// Memory of this computer in MB, if it can be told.
fn host_memory_mb(context: &Context) -> Option<u32> {
    let meminfo = context.env.read("/proc/meminfo").ok()?;
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    u32::try_from(kb / 1024).ok()
}

/// Shows the dialog. `created` is called with the new VM's folder.
pub fn show(context: &Rc<Context>, parent: &impl IsA<gtk::Widget>, created: impl Fn(Folder) + 'static) {
    let host_cpus = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(1);
    let host_memory_gb = (host_memory_mb(context).unwrap_or(8192) / 1024).max(1);

    let name = adw::EntryRow::builder().title("Name").build();
    let system = adw::ComboRow::builder().title("Guest system").build();
    system.set_model(Some(&gtk::StringList::new(&catalog::SYSTEMS.map(|system| system.label))));

    let image_path: Rc<RefCell<Option<PathBuf>>> = Rc::default();
    let image = crate::plain_row("Installation image", "None: start with an empty disk");
    image.set_subtitle_lines(1);
    let choose_image = gtk::Button::builder().label("Choose…").valign(gtk::Align::Center).build();
    image.add_suffix(&choose_image);

    let location_path = Rc::new(RefCell::new(library::default_vm_parent().unwrap_or_else(|_| PathBuf::from("."))));
    let location = crate::plain_row("Location", &location_path.borrow().display().to_string());
    location.set_subtitle_lines(1);
    let choose_location = gtk::Button::builder().label("Change…").valign(gtk::Align::Center).build();
    location.add_suffix(&choose_location);

    let general = adw::PreferencesGroup::new();
    general.add(&name);
    general.add(&system);
    general.add(&image);
    general.add(&location);

    let cpus = adw::SpinRow::with_range(1.0, f64::from(host_cpus), 1.0);
    cpus.set_title("Processors");
    cpus.set_value(f64::from((host_cpus / 2).clamp(1, 4)));
    let memory = adw::SpinRow::with_range(1.0, f64::from(host_memory_gb), 1.0);
    memory.set_title("Memory (GB)");
    memory.set_value(f64::from((host_memory_gb / 2).clamp(1, 4)));
    let disk = adw::SpinRow::with_range(8.0, 4096.0, 8.0);
    disk.set_title("Disk (GB)");
    disk.set_subtitle("Space is only used as the virtual machine fills it");
    disk.set_value(64.0);
    let hardware = adw::PreferencesGroup::builder()
        .title("Hardware")
        .description("Processors and memory can be changed later; changes apply at the next boot.")
        .build();
    hardware.add(&cpus);
    hardware.add(&memory);
    hardware.add(&disk);

    let gpu = adw::SwitchRow::builder().title("GPU acceleration").subtitle(SUSPEND_UNAVAILABLE_WITH_GPU).build();
    let network = adw::SwitchRow::builder()
        .title("Network")
        .subtitle("Internet access through this computer")
        .active(true)
        .build();
    let features = adw::PreferencesGroup::builder().title("Features").build();
    features.add(&gpu);
    features.add(&network);

    let page = adw::PreferencesPage::new();
    page.add(&general);
    page.add(&hardware);
    page.add(&features);

    let cancel = gtk::Button::with_label("Cancel");
    let create = gtk::Button::with_label("Create");
    create.add_css_class("suggested-action");
    create.set_sensitive(false);
    let header = adw::HeaderBar::builder().show_start_title_buttons(false).show_end_title_buttons(false).build();
    header.pack_start(&cancel);
    header.pack_end(&create);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&page));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&toasts));
    let dialog = adw::Dialog::builder()
        .title("New Virtual Machine")
        .content_width(560)
        .content_height(720)
        .child(&toolbar)
        .build();

    name.connect_changed({
        let create = create.clone();
        move |name| create.set_sensitive(!name.text().trim().is_empty())
    });
    cancel.connect_clicked({
        let dialog = dialog.clone();
        move |_| {
            dialog.close();
        }
    });

    choose_image.connect_clicked({
        let (image, image_path, dialog) = (image.clone(), image_path.clone(), dialog.clone());
        move |_| {
            let filter = gtk::FileFilter::new();
            filter.set_name(Some("Installation images (.iso)"));
            filter.add_suffix("iso");
            let filters = gio::ListStore::new::<gtk::FileFilter>();
            filters.append(&filter);
            let chooser = gtk::FileDialog::builder().title("Choose an Installation Image").filters(&filters).build();
            let (image, image_path) = (image.clone(), image_path.clone());
            let window = dialog.root().and_downcast::<gtk::Window>();
            chooser.open(window.as_ref(), gio::Cancellable::NONE, move |chosen| {
                if let Some(path) = chosen.ok().and_then(|file| file.path()) {
                    image.set_subtitle(&path.display().to_string());
                    image_path.replace(Some(path));
                }
            });
        }
    });
    choose_location.connect_clicked({
        let (location, location_path, dialog) = (location.clone(), location_path.clone(), dialog.clone());
        move |_| {
            let chooser = gtk::FileDialog::builder().title("Choose Where to Keep the Virtual Machine").build();
            let (location, location_path) = (location.clone(), location_path.clone());
            let window = dialog.root().and_downcast::<gtk::Window>();
            chooser.select_folder(window.as_ref(), gio::Cancellable::NONE, move |chosen| {
                if let Some(path) = chosen.ok().and_then(|file| file.path()) {
                    location.set_subtitle(&path.display().to_string());
                    location_path.replace(path);
                }
            });
        }
    });

    let created = Rc::new(created);
    create.connect_clicked({
        let (context, dialog, toasts) = (context.clone(), dialog.clone(), toasts.clone());
        move |create| {
            let new = NewVm {
                name: name.text().trim().to_owned(),
                os: catalog::SYSTEMS.get(system.selected() as usize).map_or("linux", |system| system.id).to_owned(),
                cpus: cpus.value() as u32,
                memory_mb: memory.value() as u64 * 1024,
                disk_gb: disk.value() as u64,
                install_image: image_path.borrow().clone(),
                existing_disk: None,
                features: Features {
                    gpu_acceleration: gpu.is_active(),
                    network: network.is_active(),
                    ..Features::default()
                },
                shared_folders: Vec::new(),
                port_forwards: Vec::new(),
            };
            let parent = location_path.borrow().clone();
            let (env, library_file) = (context.env.clone(), context.library.clone());
            // Creating the disk can take a moment; keep the window responsive and the button from being pressed twice.
            create.set_sensitive(false);
            let (create, dialog, toasts, created) = (create.clone(), dialog.clone(), toasts.clone(), created.clone());
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
                        // Messages carry paths and other programs' words: shown as they are, never read as markup.
                        let toast = adw::Toast::builder().use_markup(false).title(e).timeout(0).build();
                        toasts.add_toast(toast);
                    }
                    Err(_) => create.set_sensitive(true),
                }
            });
        }
    });
    dialog.present(Some(parent));
}
