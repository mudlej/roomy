//! The "Check This Computer" dialog: the doctor's report, ready to copy.

use adw::prelude::*;
use roomy_core::doctor::Report;

/// Shows the report. When something blocks VMs from running, the dialog says so up front.
pub fn show(parent: &impl IsA<gtk::Widget>, report: &Report) {
    let text = report.to_text();
    let view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build();
    view.buffer().set_text(&text);
    let scrolled = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();

    let copy = gtk::Button::with_label("Copy Report");
    copy.add_css_class("suggested-action");
    let header = adw::HeaderBar::new();
    header.pack_start(&copy);

    let banner = adw::Banner::new(
        "This computer cannot run virtual machines yet. Each problem below says how to fix it; 'roomy setup' in a terminal installs missing packages.",
    );
    banner.set_revealed(!report.can_run_vms());

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&scrolled));
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banner);
    toolbar.set_content(Some(&toasts));

    let dialog = adw::Dialog::builder()
        .title("Check This Computer")
        .content_width(760)
        .content_height(560)
        .child(&toolbar)
        .build();
    copy.connect_clicked({
        let toasts = toasts.clone();
        move |button| {
            button.clipboard().set_text(&text);
            toasts.add_toast(adw::Toast::new("Report copied"));
        }
    });
    dialog.present(Some(parent));
}
