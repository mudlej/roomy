//! The "Guest Setup" dialog: what to install inside the guest so every
//! feature works, with commands ready to copy.

use adw::prelude::*;
use roomy_core::catalog::{self, Feature};
use roomy_core::vm::Config;

/// A row showing a command with a button that copies it.
fn command_row(title: &str, command: &str) -> adw::ActionRow {
    let row = crate::plain_row(title, command);
    row.set_subtitle_selectable(true);
    row.add_css_class("property");
    let copy =
        gtk::Button::builder().icon_name("edit-copy-symbolic").valign(gtk::Align::Center).tooltip_text("Copy").build();
    copy.add_css_class("flat");
    let command = command.to_owned();
    copy.connect_clicked(move |button| button.clipboard().set_text(&command));
    row.add_suffix(&copy);
    row
}

pub fn show(parent: &impl IsA<gtk::Widget>, config: &Config, guest_agent: Option<bool>) {
    let page = adw::PreferencesPage::new();

    let helpers = adw::PreferencesGroup::builder()
        .title("Helpers inside the guest")
        .description("Run this in a terminal inside the virtual machine. Without the helpers the virtual machine still works; these features do not.")
        .build();
    let system = config.system();
    match system.install_helpers {
        Some(command) => helpers.add(&command_row(&format!("Install on {}", system.label), command)),
        None => helpers.add(&crate::plain_row("Install", &catalog::generic_install_advice())),
    }
    for (package, purpose) in system.helpers() {
        helpers.add(&crate::plain_row(package, purpose));
    }
    let status = match guest_agent {
        Some(true) => "Detected: the guest's agent is answering",
        Some(false) => "Not detected yet (it starts late in the guest's boot)",
        None => "Start the virtual machine to see whether its agent answers",
    };
    helpers.add(&crate::plain_row("Guest agent", status));
    page.add(&helpers);

    if !system.lacks.is_empty() {
        let lacking = adw::PreferencesGroup::builder()
            .title(format!("Not available in a {} guest", system.label))
            .description("Nothing that can be installed in the guest changes these.")
            .build();
        for (feature, why) in system.lacks {
            lacking.add(&crate::plain_row(&feature.title(), why));
        }
        page.add(&lacking);
    }

    let folders = adw::PreferencesGroup::builder().title("Shared folders").build();
    folders.set_visible(system.offers(Feature::SharedFolders));
    if config.shared_folders.is_empty() {
        folders.set_description(Some("This virtual machine has no shared folders. Add one in its settings."));
    } else {
        folders.set_description(Some(
            "A Linux guest with systemd 254 or newer (2023) mounts each folder by itself when it starts, at the \
             place named here. In any other guest, run the command inside the guest.",
        ));
    }
    for share in &config.shared_folders {
        let (shown, place) = (share.path.display(), catalog::guest_path(&share.name));
        folders.add(&crate::plain_row(&shown.to_string(), &format!("In the guest: {place}")));
        folders.add(&command_row("Mount it by hand, until the guest restarts", &catalog::mount_command(&share.name)));
        folders.add(&command_row(
            "Line for the guest's /etc/fstab, to mount it at every boot",
            &catalog::fstab_line(&share.name),
        ));
    }
    page.add(&folders);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    adw::Dialog::builder()
        .title("Guest Setup")
        .content_width(640)
        .content_height(620)
        .child(&toolbar)
        .build()
        .present(Some(parent));
}
