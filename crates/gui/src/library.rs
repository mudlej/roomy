//! The main window: the list of virtual machines.

use crate::machine::Machine;
use crate::{Context, create, doctor, downloads};
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use roomy_core::catalog::GuestSystem;
use roomy_core::control::{self, State};
use roomy_core::library;
use roomy_core::vm::{Config, Folder};
use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

/// One line of the library file, with what could be found out about it.
struct Entry {
    dir: PathBuf,
    vm: Result<(Folder, Config, State), String>,
}

struct Row {
    dir: PathBuf,
    row: adw::ActionRow,
    start: gtk::Button,
    /// The VM's screen as its window last showed it, and a stand-in for VMs never seen yet.
    thumbnail: gtk::Picture,
    thumbnail_frame: adw::Clamp,
    no_thumbnail: gtk::Image,
    /// When the picture shown was saved, to load it again only when it changes.
    thumbnail_saved: Cell<Option<std::time::SystemTime>>,
}

impl Row {
    /// Shows the picture saved at `path`, loading it only when it has changed, or the stand-in
    /// when there is none.
    fn show_thumbnail(&self, path: Option<&Path>) {
        let saved = path.and_then(|path| std::fs::metadata(path).and_then(|file| file.modified()).ok());
        if self.thumbnail_saved.replace(saved) != saved || (saved.is_none() && self.thumbnail_frame.is_visible()) {
            let picture = path.filter(|_| saved.is_some()).and_then(|path| gdk::Texture::from_filename(path).ok());
            self.thumbnail.set_paintable(picture.as_ref());
            self.thumbnail_frame.set_visible(picture.is_some());
            self.no_thumbnail.set_visible(picture.is_none());
        }
    }
}

pub struct Library {
    context: Rc<Context>,
    app: adw::Application,
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    list: gtk::ListBox,
    rows: RefCell<Vec<Row>>,
    machines: RefCell<Vec<Rc<Machine>>>,
    refreshing: Cell<bool>,
    /// The doctor's last verdict, for roomy's own test runs.
    pub doctor_verdict: RefCell<Option<String>>,
}

impl Library {
    pub fn open(context: &Rc<Context>, app: &adw::Application) -> Rc<Library> {
        let new_button =
            gtk::Button::builder().icon_name("list-add-symbolic").tooltip_text("New virtual machine").build();
        let menu = gio::Menu::new();
        menu.append(Some("Download a System…"), Some("win.downloads"));
        menu.append(Some("Add Existing…"), Some("win.add-existing"));
        menu.append(Some("Check This Computer…"), Some("win.doctor"));
        let header = adw::HeaderBar::new();
        header.pack_start(&new_button);
        header.pack_end(
            &gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).tooltip_text("Menu").build(),
        );

        let create_button = gtk::Button::with_label("Create a Virtual Machine");
        create_button.set_halign(gtk::Align::Center);
        create_button.add_css_class("pill");
        create_button.add_css_class("suggested-action");
        let empty = adw::StatusPage::builder()
            .icon_name("computer-symbolic")
            .title("No Virtual Machines Yet")
            .description("Each virtual machine lives in a folder of its own that you can move, copy or back up.")
            .child(&create_button)
            .build();

        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).valign(gtk::Align::Start).build();
        list.add_css_class("boxed-list");
        let clamp =
            adw::Clamp::builder().child(&list).margin_top(24).margin_bottom(24).margin_start(12).margin_end(12).build();
        let scrolled = gtk::ScrolledWindow::builder().child(&clamp).hscrollbar_policy(gtk::PolicyType::Never).build();

        let stack = gtk::Stack::new();
        stack.add_named(&empty, Some("empty"));
        stack.add_named(&scrolled, Some("list"));
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&stack));
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.set_content(Some(&toasts));
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title("roomy")
            .default_width(960)
            .default_height(720)
            .content(&toolbar)
            .build();

        let library = Rc::new(Library {
            context: context.clone(),
            app: app.clone(),
            window,
            toasts,
            stack,
            list,
            rows: RefCell::new(Vec::new()),
            machines: RefCell::new(Vec::new()),
            refreshing: Cell::new(false),
            doctor_verdict: RefCell::new(None),
        });

        new_button.connect_clicked(on!(&library, |this, _| this.new_vm()));
        create_button.connect_clicked(on!(&library, |this, _| this.new_vm()));
        let add_existing = gio::SimpleAction::new("add-existing", None);
        add_existing.connect_activate(on!(&library, |this, _, _| this.add_existing()));
        library.window.add_action(&add_existing);
        let downloads_action = gio::SimpleAction::new("downloads", None);
        downloads_action.connect_activate(on!(&library, |this, _, _| {
            this.open_downloads();
        }));
        library.window.add_action(&downloads_action);
        let doctor_action = gio::SimpleAction::new("doctor", None);
        doctor_action.connect_activate(on!(&library, |this, _, _| this.check_computer(true)));
        library.window.add_action(&doctor_action);

        library.refresh();
        glib::timeout_add_local(Duration::from_secs(1), {
            let weak = Rc::downgrade(&library);
            move || match weak.upgrade() {
                Some(library) => {
                    library.refresh();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            }
        });
        library.window.present();
        // Say so right away when this computer cannot run VMs, instead of failing at the first Start.
        library.check_computer(false);
        library
    }

    pub fn window(&self) -> &adw::ApplicationWindow {
        &self.window
    }

    fn tell(&self, message: &str) {
        crate::toast(&self.toasts, message, 8);
    }

    /// Runs the doctor. Its report is shown when asked for, or when something blocks VMs.
    fn check_computer(self: &Rc<Self>, always_show: bool) {
        let env = self.context.env.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let report = gio::spawn_blocking(move || doctor::examine(&env)).await;
            let (Some(this), Ok(report)) = (weak.upgrade(), report) else { return };
            this.doctor_verdict.replace(Some(report.verdict()));
            if always_show || !report.can_run_vms() {
                // The dialog checks again after installing packages.
                let weak = Rc::downgrade(&this);
                doctor::show(&this.context, &this.window, &report, move |report| {
                    if let Some(this) = weak.upgrade() {
                        this.doctor_verdict.replace(Some(report.verdict()));
                    }
                });
            }
        });
    }

    fn new_vm(self: &Rc<Self>) {
        self.new_vm_from(None);
    }

    /// Opens the "New Virtual Machine" dialog, filled in for an installation image when one is given.
    pub fn new_vm_from(self: &Rc<Self>, start: Option<(PathBuf, GuestSystem)>) {
        create::show(
            &self.context,
            &self.window,
            start,
            on!(self, |this, folder| {
                this.refresh();
                this.open_machine(folder.dir());
            }),
        );
    }

    /// Opens the window that downloads installation images; a downloaded one leads to a new VM.
    pub fn open_downloads(self: &Rc<Self>) -> Rc<downloads::Catalog> {
        let window = self.window.clone().upcast::<gtk::Window>();
        downloads::show(
            &self.context,
            Some(&window),
            None,
            "Create a Virtual Machine",
            on!(self, |this, path, guest| this.new_vm_from(Some((path, guest)))),
        )
    }

    fn add_existing(self: &Rc<Self>) {
        let chooser = gtk::FileDialog::builder().title("Choose a Virtual Machine's Folder").build();
        let weak = Rc::downgrade(self);
        chooser.select_folder(Some(&self.window), gio::Cancellable::NONE, move |chosen| {
            let (Some(this), Some(path)) = (weak.upgrade(), chosen.ok().and_then(|file| file.path())) else { return };
            match library::add(&this.context.library, &path) {
                Ok(true) => this.refresh(),
                Ok(false) => this.tell("That virtual machine is already in the list"),
                Err(e) => this.tell(&e.to_string()),
            }
        });
    }

    /// Opens the window of the VM in `dir`, or brings it forward if it is open already.
    pub fn open_machine(self: &Rc<Self>, dir: &Path) -> Option<Rc<Machine>> {
        let folder = match Folder::open(dir) {
            Ok(folder) => folder,
            Err(e) => {
                self.tell(&e.to_string());
                return None;
            }
        };
        if let Some(open) = self.machines.borrow().iter().find(|m| *m.folder() == folder) {
            open.window().present();
            return Some(open.clone());
        }
        let config = match folder.load() {
            Ok(config) => config,
            Err(e) => {
                self.tell(&e.to_string());
                return None;
            }
        };
        let machine = Machine::open(&self.context, &self.app, folder, config);
        // Closing the window only closes the view: the virtual machine keeps running.
        machine.window().connect_close_request({
            let (weak, closing) = (Rc::downgrade(self), Rc::downgrade(&machine));
            move |_| {
                if let (Some(this), Some(closing)) = (weak.upgrade(), closing.upgrade()) {
                    this.machines.borrow_mut().retain(|m| !Rc::ptr_eq(m, &closing));
                    if matches!(closing.state(), State::Running | State::Paused) {
                        this.tell("The virtual machine keeps running in the background");
                    }
                }
                glib::Propagation::Proceed
            }
        });
        self.machines.borrow_mut().push(machine.clone());
        Some(machine)
    }

    /// Re-reads the library and every VM's state, off the window's thread.
    pub fn refresh(self: &Rc<Self>) {
        if self.refreshing.replace(true) {
            return;
        }
        let (env, library_file) = (self.context.env.clone(), self.context.library.clone());
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let entries = gio::spawn_blocking(move || -> Result<Vec<Entry>, String> {
                let folders = library::folders(&library_file).map_err(|e| e.to_string())?;
                Ok(folders
                    .into_iter()
                    .map(|dir| {
                        let vm = Folder::open(&dir)
                            .and_then(|folder| {
                                let config = folder.load()?;
                                let state = control::status(&env, &folder);
                                Ok((folder, config, state))
                            })
                            .map_err(|e| e.to_string());
                        Entry { dir, vm }
                    })
                    .collect())
            })
            .await;
            let Some(this) = weak.upgrade() else { return };
            this.refreshing.set(false);
            match entries {
                Ok(Ok(entries)) => this.show(entries),
                Ok(Err(e)) => this.tell(&e),
                Err(_) => {}
            }
        });
    }

    fn show(self: &Rc<Self>, entries: Vec<Entry>) {
        self.stack.set_visible_child_name(if entries.is_empty() { "empty" } else { "list" });
        // Rows are only rebuilt when the list itself changes, so hovering and focus survive the refresh.
        let same_list = {
            let rows = self.rows.borrow();
            rows.len() == entries.len() && rows.iter().zip(&entries).all(|(row, entry)| row.dir == entry.dir)
        };
        if !same_list {
            for row in self.rows.borrow_mut().drain(..) {
                self.list.remove(&row.row);
            }
            let rows: Vec<Row> = entries.iter().map(|entry| self.build_row(&entry.dir)).collect();
            for row in &rows {
                self.list.append(&row.row);
            }
            self.rows.replace(rows);
        }
        for (row, entry) in self.rows.borrow().iter().zip(&entries) {
            match &entry.vm {
                Ok((folder, config, state)) => {
                    row.show_thumbnail(Some(&folder.thumbnail()));
                    row.row.set_title(&config.name);
                    row.row.set_subtitle(&format!("{} · {}", crate::state_words(*state), entry.dir.display()));
                    row.row.set_activatable(true);
                    row.start.set_visible(matches!(state, State::Off | State::Suspended));
                    row.start.set_label(if *state == State::Suspended { "Continue" } else { "Start" });
                }
                Err(problem) => {
                    row.show_thumbnail(None);
                    row.row.set_title(&entry.dir.display().to_string());
                    row.row.set_subtitle(&format!("Not available: {problem}"));
                    row.row.set_activatable(false);
                    row.start.set_visible(false);
                }
            }
        }
    }

    fn build_row(self: &Rc<Self>, dir: &Path) -> Row {
        let row = adw::ActionRow::builder().use_markup(false).subtitle_lines(1).build();
        let thumbnail = gtk::Picture::builder().can_shrink(true).content_fit(gtk::ContentFit::Contain).build();
        // A picture asks for its full size; the two frames hold it to a thumbnail's, whatever its shape.
        let narrow = adw::Clamp::builder().maximum_size(96).tightening_threshold(96).child(&thumbnail).build();
        let thumbnail_frame = adw::Clamp::builder()
            .orientation(gtk::Orientation::Vertical)
            .maximum_size(72)
            .tightening_threshold(72)
            .valign(gtk::Align::Center)
            .visible(false)
            .child(&narrow)
            .build();
        let no_thumbnail =
            gtk::Image::builder().icon_name("computer-symbolic").pixel_size(32).width_request(96).build();
        row.add_prefix(&thumbnail_frame);
        row.add_prefix(&no_thumbnail);
        let start = gtk::Button::builder().label("Start").valign(gtk::Align::Center).build();
        start.connect_clicked({
            let (weak, dir) = (Rc::downgrade(self), dir.to_path_buf());
            move |_| {
                if let Some(machine) = weak.upgrade().and_then(|this| this.open_machine(&dir)) {
                    machine.start();
                }
            }
        });
        row.connect_activated({
            let (weak, dir) = (Rc::downgrade(self), dir.to_path_buf());
            move |_| {
                if let Some(this) = weak.upgrade() {
                    this.open_machine(&dir);
                }
            }
        });

        let actions = gio::SimpleActionGroup::new();
        let add = |name: &str, run: fn(&Rc<Library>, &Path)| {
            let action = gio::SimpleAction::new(name, None);
            let (weak, dir) = (Rc::downgrade(self), dir.to_path_buf());
            action.connect_activate(move |_, _| {
                if let Some(this) = weak.upgrade() {
                    run(&this, &dir)
                }
            });
            actions.add_action(&action);
        };
        add("show-folder", |this, dir| {
            gtk::FileLauncher::new(Some(&gio::File::for_path(dir))).open_containing_folder(
                Some(&this.window),
                gio::Cancellable::NONE,
                |_| {},
            );
        });
        add("forget", |this, dir| match library::forget(&this.context.library, dir) {
            Ok(_) => {
                this.tell(&format!("Removed from the list. Its files are still in {}", dir.display()));
                this.refresh();
            }
            Err(e) => this.tell(&e.to_string()),
        });
        add("delete", |this, dir| this.confirm_delete(dir));
        add("settings", |this, dir| match Folder::open(dir) {
            Ok(folder) => crate::settings::show(
                &this.context,
                &this.window,
                &folder,
                on!(this, |this, summary| {
                    this.tell(&summary);
                    this.refresh();
                }),
            ),
            Err(e) => this.tell(&e.to_string()),
        });
        row.insert_action_group("vm", Some(&actions));
        let menu = gio::Menu::new();
        menu.append(Some("Settings…"), Some("vm.settings"));
        menu.append(Some("Show Folder"), Some("vm.show-folder"));
        menu.append(Some("Remove from List"), Some("vm.forget"));
        menu.append(Some("Delete…"), Some("vm.delete"));
        let more = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .menu_model(&menu)
            .valign(gtk::Align::Center)
            .tooltip_text("More")
            .build();
        more.add_css_class("flat");
        row.add_suffix(&start);
        row.add_suffix(&more);
        Row {
            dir: dir.to_path_buf(),
            row,
            start,
            thumbnail,
            thumbnail_frame,
            no_thumbnail,
            thumbnail_saved: Cell::new(None),
        }
    }

    fn confirm_delete(self: &Rc<Self>, dir: &Path) {
        let Ok(folder) = Folder::open(dir) else {
            // Nothing left to delete; just take the stale entry off the list.
            let _ = library::forget(&self.context.library, dir);
            self.refresh();
            return;
        };
        let name = folder.load().map(|config| config.name).unwrap_or_else(|_| dir.display().to_string());
        let body = format!(
            "This permanently deletes its disk, saved state and settings in {}. Files of your own in that folder are kept.",
            dir.display()
        );
        crate::confirm(
            &self.window,
            &format!("Delete “{name}”?"),
            &body,
            "Delete",
            on!(self, |this| {
                if let Some(open) = this.machines.borrow().iter().find(|m| *m.folder() == folder) {
                    open.window().close();
                }
                match library::delete(&this.context.env, &this.context.library, &folder) {
                    Ok(left) if left.is_empty() => this.tell("Deleted"),
                    Ok(left) => this.tell(&format!("Deleted. {} other item(s) in the folder were kept", left.len())),
                    Err(e) => this.tell(&e.to_string()),
                }
                this.refresh();
            }),
        );
    }
}
