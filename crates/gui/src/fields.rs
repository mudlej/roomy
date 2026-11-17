//! Rows that the "New Virtual Machine" dialog and a VM's settings share.

use adw::prelude::*;
use gtk::gio;
use roomy_core::catalog;
use roomy_core::vm::{PortForward, SharedFolder};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

/// A number picked out of what there is: a slider over the whole range, and
/// beside it a field for typing an exact value.
///
/// The slider gets a line of its own under the title, whatever the dialog's
/// width: next to a title and a note it would be left too short to aim with.
pub struct Amount {
    pub row: adw::PreferencesRow,
    adjustment: gtk::Adjustment,
    note: gtk::Label,
}

impl Amount {
    /// Values run from `min` to `max` in multiples of `step`, shown with `digits` decimals.
    pub fn new(title: &str, min: f64, max: f64, step: f64, digits: u32) -> Amount {
        let max = max.max(min);
        let adjustment = gtk::Adjustment::new(min, min, max, step, step, 0.0);
        // A slider stops anywhere along its length; the values on offer are steps apart.
        adjustment.connect_value_changed(move |adjustment| {
            let value = adjustment.value();
            let stepped = ((value / step).round() * step).clamp(adjustment.lower(), adjustment.upper());
            if (stepped - value).abs() > step / 1000.0 {
                adjustment.set_value(stepped);
            }
        });
        let slider = gtk::Scale::builder()
            .orientation(gtk::Orientation::Horizontal)
            .adjustment(&adjustment)
            .draw_value(false)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let exact = gtk::SpinButton::builder()
            .adjustment(&adjustment)
            .digits(digits)
            .numeric(true)
            .valign(gtk::Align::Center)
            .build();
        let heading = gtk::Label::builder().label(title).xalign(0.0).build();
        let note = gtk::Label::builder().xalign(0.0).wrap(true).visible(false).build();
        note.add_css_class("caption");
        note.add_css_class("dim-label");
        let controls = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        controls.append(&slider);
        controls.append(&exact);
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_top(10)
            .margin_bottom(8)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&heading);
        content.append(&note);
        content.append(&controls);
        let row = adw::PreferencesRow::builder().title(title).activatable(false).child(&content).build();
        Amount { row, adjustment, note }
    }

    pub fn value(&self) -> f64 {
        self.adjustment.value()
    }

    pub fn set_value(&self, value: f64) {
        self.adjustment.set_value(value);
    }

    /// Changes how much there is to pick from. A value beyond it comes down to it.
    pub fn set_max(&self, max: f64) {
        let max = max.max(self.adjustment.lower());
        self.adjustment.set_upper(max);
        if self.adjustment.value() > max {
            self.adjustment.set_value(max);
        }
    }

    /// The line under the title: how much there is, and what else is worth knowing.
    pub fn set_note(&self, note: &str) {
        self.note.set_label(note);
        self.note.set_visible(!note.is_empty());
    }

    pub fn set_sensitive(&self, sensitive: bool) {
        self.row.set_sensitive(sensitive);
    }
}

/// Where the rows of a list are shown: in a group of their own, or among the rows an expander reveals.
#[derive(Clone)]
pub enum Holder {
    Group(adw::PreferencesGroup),
    Expander(adw::ExpanderRow),
}

impl Holder {
    fn add(&self, row: &impl IsA<gtk::Widget>) {
        match self {
            Holder::Group(group) => group.add(row),
            Holder::Expander(expander) => expander.add_row(row),
        }
    }

    fn remove(&self, row: &impl IsA<gtk::Widget>) {
        match self {
            Holder::Group(group) => group.remove(row),
            Holder::Expander(expander) => expander.remove(row),
        }
    }
}

/// Text worth copying for an item of a list: what the button that copies it says about itself,
/// and the text.
type Copyable<T> = (&'static str, fn(&T) -> String);

/// A list the user adds to and removes from, one row per item.
struct List<T: 'static> {
    holder: Holder,
    items: Rc<RefCell<Vec<T>>>,
    rows: Rc<RefCell<Vec<adw::ActionRow>>>,
    describe: fn(&T) -> (String, String),
    to_copy: Option<Copyable<T>>,
}

impl<T: 'static> Clone for List<T> {
    fn clone(&self) -> Self {
        List {
            holder: self.holder.clone(),
            items: self.items.clone(),
            rows: self.rows.clone(),
            describe: self.describe,
            to_copy: self.to_copy,
        }
    }
}

impl<T: 'static> List<T> {
    fn new(
        holder: Holder,
        items: Vec<T>,
        describe: fn(&T) -> (String, String),
        to_copy: Option<Copyable<T>>,
    ) -> List<T> {
        let list = List { holder, items: Rc::new(RefCell::new(items)), rows: Rc::default(), describe, to_copy };
        list.rebuild();
        list
    }

    fn rebuild(&self) {
        for row in self.rows.borrow_mut().drain(..) {
            self.holder.remove(&row);
        }
        for (index, item) in self.items.borrow().iter().enumerate() {
            let (title, subtitle) = (self.describe)(item);
            let row = crate::plain_row(&title, &subtitle);
            row.set_subtitle_lines(1);
            if let Some((about, text)) = self.to_copy {
                let copy = gtk::Button::builder()
                    .icon_name("edit-copy-symbolic")
                    .valign(gtk::Align::Center)
                    .tooltip_text(about)
                    .build();
                copy.add_css_class("flat");
                let text = text(item);
                copy.connect_clicked(move |button| button.clipboard().set_text(&text));
                row.add_suffix(&copy);
            }
            let remove = gtk::Button::builder()
                .icon_name("list-remove-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text("Remove")
                .build();
            remove.add_css_class("flat");
            remove.connect_clicked({
                let list = self.clone();
                move |_| {
                    if index < list.items.borrow().len() {
                        list.items.borrow_mut().remove(index);
                    }
                    list.rebuild();
                }
            });
            row.add_suffix(&remove);
            self.holder.add(&row);
            self.rows.borrow_mut().push(row);
        }
    }
}

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

/// How the folders get into the guest, for the group's description.
pub const SHARED_FOLDERS_IN_THE_GUEST: &str = "Inside a Linux guest each folder is at the place shown from the moment \
    the guest has started. A guest whose systemd is older than version 254 (2023) does not mount them by itself: \
    the copy button gives the command to run in the guest.";

/// The "Shared folders" group: folders of this computer the guest can open.
pub struct SharedFolders {
    pub group: adw::PreferencesGroup,
    list: List<SharedFolder>,
}

impl SharedFolders {
    pub fn new(shared: Vec<SharedFolder>, description: &str) -> SharedFolders {
        let add = gtk::Button::builder()
            .icon_name("list-add-symbolic")
            .valign(gtk::Align::Center)
            .tooltip_text("Share a folder")
            .build();
        add.add_css_class("flat");
        let group = adw::PreferencesGroup::builder()
            .title("Shared folders")
            .description(description)
            .header_suffix(&add)
            .build();
        let list = List::new(
            Holder::Group(group.clone()),
            shared,
            |share: &SharedFolder| {
                (share.path.display().to_string(), format!("In the guest: {}", catalog::guest_path(&share.name)))
            },
            Some((
                "Copy the command that mounts this folder inside a guest which does not do so by itself",
                |share: &SharedFolder| catalog::mount_command(&share.name),
            )),
        );
        add.connect_clicked({
            let list = list.clone();
            move |add| {
                let chooser = gtk::FileDialog::builder().title("Choose a Folder to Share").build();
                let list = list.clone();
                let window = add.root().and_downcast::<gtk::Window>();
                chooser.select_folder(window.as_ref(), gio::Cancellable::NONE, move |chosen| {
                    let Some(path) = chosen.ok().and_then(|file| file.path()) else { return };
                    if list.items.borrow().iter().any(|share| share.path == path) {
                        return;
                    }
                    let name = share_name(&path, &list.items.borrow());
                    list.items.borrow_mut().push(SharedFolder { name, path });
                    list.rebuild();
                });
            }
        });
        SharedFolders { group, list }
    }

    pub fn get(&self) -> Vec<SharedFolder> {
        self.list.items.borrow().clone()
    }
}

/// Forwarded ports: a field to add one, and a row for each.
pub struct PortForwards {
    list: List<PortForward>,
}

impl PortForwards {
    /// Puts the rows into `holder`. `complain` is told when what was typed is not two port numbers.
    pub fn new(holder: Holder, forwards: Vec<PortForward>, complain: impl Fn(&str) + 'static) -> PortForwards {
        let add = adw::EntryRow::builder()
            .title("Forward a port: this computer's port, a colon, the guest's port (2222:22)")
            .show_apply_button(true)
            .build();
        holder.add(&add);
        let list = List::new(
            holder,
            forwards,
            |forward: &PortForward| {
                (
                    format!("Port {} of this computer", forward.host),
                    format!("reaches port {} of the guest", forward.guest),
                )
            },
            None,
        );
        add.connect_apply({
            let list = list.clone();
            move |entry| {
                let text = entry.text();
                let ports = text.split_once(':').and_then(|(host, guest)| {
                    Some((host.trim().parse::<u16>().ok()?, guest.trim().parse::<u16>().ok()?))
                });
                match ports {
                    Some((host, guest)) if host != 0 && guest != 0 => {
                        list.items.borrow_mut().retain(|forward| forward.host != host);
                        list.items.borrow_mut().push(PortForward { host, guest });
                        list.rebuild();
                        entry.set_text("");
                    }
                    _ => complain("Write two port numbers with a colon between them, for example 2222:22"),
                }
            }
        });
        PortForwards { list }
    }

    pub fn get(&self) -> Vec<PortForward> {
        self.list.items.borrow().clone()
    }
}
