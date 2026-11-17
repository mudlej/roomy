//! The "Check This Computer" dialog: the doctor's report, ready to copy, and
//! the installation of missing packages when that is what the report calls for.

use crate::Context;
use adw::prelude::*;
use gtk::{gio, glib};
use roomy_core::doctor::{self as checks, Report};
use roomy_core::env::{Env, roomy_command};
use roomy_core::{host, setup};
use std::cell::{Cell, RefCell};
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

/// The doctor's checks that matter to a window which is already open.
pub fn examine(env: &Env) -> Report {
    let mut report = checks::run(env);
    report.checks.push(checks::check_sound());
    report
}

struct Doctor {
    context: Rc<Context>,
    dialog: adw::Dialog,
    view: gtk::TextView,
    banner: adw::Banner,
    toasts: adw::ToastOverlay,
    /// Packages were installed from this dialog: offering that again would promise nothing new.
    installed: Cell<bool>,
    /// Something started from the banner's button is still going on.
    busy: Cell<bool>,
    /// How much of the installation's output is on show.
    log_shown: Cell<usize>,
    /// Told about every report this dialog shows.
    examined: Box<dyn Fn(&Report)>,
}

/// Shows the report. When installing packages is the fix, the dialog offers to do it.
pub fn show(
    context: &Rc<Context>,
    parent: &impl IsA<gtk::Widget>,
    report: &Report,
    examined: impl Fn(&Report) + 'static,
) {
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
    let scrolled = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();

    let copy = gtk::Button::with_label("Copy Report");
    copy.add_css_class("suggested-action");
    let header = adw::HeaderBar::new();
    header.pack_start(&copy);

    let banner = adw::Banner::new("");
    banner.set_use_markup(false);

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

    let doctor = Rc::new(Doctor {
        context: context.clone(),
        dialog,
        view,
        banner,
        toasts,
        installed: Cell::new(false),
        busy: Cell::new(false),
        log_shown: Cell::new(0),
        examined: Box::new(examined),
    });
    doctor.show_report(report);

    let weak = Rc::downgrade(&doctor);
    doctor.banner.connect_button_clicked({
        let weak = weak.clone();
        move |_| {
            if let Some(doctor) = weak.upgrade() {
                doctor.propose();
            }
        }
    });
    copy.connect_clicked(move |button| {
        let Some(doctor) = weak.upgrade() else { return };
        let buffer = doctor.view.buffer();
        button.clipboard().set_text(&buffer.text(&buffer.start_iter(), &buffer.end_iter(), false));
        doctor.toasts.add_toast(adw::Toast::new("Copied"));
    });
    // The dialog keeps its own state alive until it is closed.
    let alive = RefCell::new(Some(doctor.clone()));
    doctor.dialog.connect_closed(move |_| drop(alive.take()));
    doctor.dialog.present(Some(parent));
}

impl Doctor {
    fn show_report(&self, report: &Report) {
        self.view.buffer().set_text(&report.to_text());
        let blocked = !report.can_run_vms();
        let (title, button) = match (report.setup_helps() && !self.installed.get(), blocked) {
            (true, true) => (
                "This computer cannot run virtual machines yet: something roomy needs is not installed.",
                Some("Install…"),
            ),
            (true, false) => ("Some features need packages that are not installed.", Some("Install…")),
            (false, true) if self.installed.get() => (
                "The packages are installed, but this computer still cannot run virtual machines. Each problem \
                 below says what to do.",
                None,
            ),
            (false, true) => {
                ("This computer cannot run virtual machines yet. Each problem below says how to fix it.", None)
            }
            (false, false) => ("", None),
        };
        self.banner.set_title(title);
        self.banner.set_button_label(button);
        self.banner.set_revealed(!title.is_empty());
        (self.examined)(report);
    }

    /// Works out what installing would take on this system and puts it to the user.
    fn propose(self: &Rc<Self>) {
        if self.busy.replace(true) {
            return;
        }
        let env = self.context.env.clone();
        let this = self.clone();
        glib::spawn_future_local(async move {
            let proposal = gio::spawn_blocking(move || {
                let os = host::os_release(&env);
                let ask = |program: &str, args: &[&str]| setup::query(&env, program, args);
                let advice = setup::advice(&env, &os, &ask);
                // The window has no terminal anyone is looking at, and the user says yes in a dialog here.
                let session = setup::Session::of_this_process(true, false);
                let commands = match &advice {
                    setup::Advice::Install(plan) => setup::elevated(plan, session, &env, &ask),
                    _ => None,
                };
                (os.pretty_name, advice, commands)
            })
            .await;
            this.busy.set(false);
            let Ok((system, advice, commands)) = proposal else { return };
            match (advice, commands) {
                (setup::Advice::Install(plan), Some(commands)) => this.confirm(plan, commands),
                (setup::Advice::Install(_), None) => this.explain(
                    "The administrator's password cannot be asked for here",
                    &format!(
                        "This desktop has no pkexec to ask with. Run this in a terminal instead:\n\n{} setup",
                        roomy_command()
                    ),
                ),
                (setup::Advice::ByHand(what_to_do), _) => {
                    this.explain(&format!("roomy cannot install this by itself on {system}"), &what_to_do)
                }
                (setup::Advice::Unknown, _) => this.explain(
                    &format!("roomy does not know the packages of {system}"),
                    &format!(
                        "Install these with your package manager, then check this computer again:\n\n{}",
                        setup::REQUIRED_COMPONENTS.join("\n")
                    ),
                ),
            }
        });
    }

    /// Says why nothing can be installed from here, with the text ready to copy.
    fn explain(&self, heading: &str, body: &str) {
        crate::show_copyable(&self.dialog, heading, body, "Copy");
    }

    fn confirm(self: &Rc<Self>, plan: setup::InstallPlan, commands: Vec<Vec<String>>) {
        let mut body = String::from("roomy will run this as administrator:\n\n");
        for line in plan.command_lines() {
            body.push_str(&line);
            body.push('\n');
        }
        for note in &plan.notes {
            body.push_str(&format!("\n{note}\n"));
        }
        body.push_str("\nYou may be asked for your password.");
        let dialog = adw::AlertDialog::new(Some("Install what is missing?"), Some(&body));
        dialog.add_responses(&[("cancel", "Cancel"), ("install", "Install")]);
        dialog.set_response_appearance("install", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("install"));
        dialog.set_close_response("cancel");
        let this = self.clone();
        let work = RefCell::new(Some((plan, commands)));
        dialog.connect_response(Some("install"), move |_, _| {
            if let Some((plan, commands)) = work.take() {
                this.install(plan, commands);
            }
        });
        dialog.present(Some(&self.dialog));
    }

    /// A fresh file for the package manager to write to. A file rather than a pipe to this
    /// window, so that the package manager can finish whatever becomes of the window.
    fn new_log(&self) -> std::io::Result<PathBuf> {
        let dir = self.context.env.runtime_dir.join("roomy");
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let log = dir.join("setup.log");
        std::fs::File::create(&log)?;
        Ok(log)
    }

    fn show_log(&self, log: &Path) {
        let Ok(bytes) = std::fs::read(log) else { return };
        if bytes.len() == self.log_shown.replace(bytes.len()) {
            return;
        }
        let buffer = self.view.buffer();
        buffer.set_text(&String::from_utf8_lossy(&bytes));
        buffer.place_cursor(&buffer.end_iter());
        self.view.scroll_mark_onscreen(&buffer.get_insert());
    }

    fn install(self: &Rc<Self>, plan: setup::InstallPlan, commands: Vec<Vec<String>>) {
        if self.busy.replace(true) {
            return;
        }
        let log = match self.new_log() {
            Ok(log) => log,
            Err(e) => {
                self.busy.set(false);
                crate::toast(&self.toasts, &format!("Cannot keep a log: {e}"), 5);
                return;
            }
        };
        // Neither this dialog nor roomy itself goes away under a package manager at work.
        self.dialog.set_can_close(false);
        let hold = gio::Application::default().map(|app| app.hold());
        self.banner.set_title("Installing… You may be asked for your password.");
        self.banner.set_button_label(None);
        self.banner.set_revealed(true);
        self.view.buffer().set_text("");
        self.log_shown.set(0);

        let watching = glib::timeout_add_local(Duration::from_millis(250), {
            let (weak, log) = (Rc::downgrade(self), log.clone());
            move || match weak.upgrade() {
                Some(this) => {
                    this.show_log(&log);
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            }
        });
        let this = self.clone();
        glib::spawn_future_local(async move {
            let outcome = gio::spawn_blocking({
                let log = log.clone();
                move || setup::run_logged(&commands, &log)
            })
            .await
            .unwrap_or_else(|_| Err("The installation stopped unexpectedly.".to_owned()));
            watching.remove();
            this.show_log(&log);
            drop(hold);
            this.dialog.set_can_close(true);

            if let Err(problem) = outcome {
                let buffer = this.view.buffer();
                let hint = plan.if_it_fails.as_deref().map(|hint| format!("\n{hint}")).unwrap_or_default();
                buffer.insert(&mut buffer.end_iter(), &format!("\n{problem}{hint}\n"));
                buffer.place_cursor(&buffer.end_iter());
                this.view.scroll_mark_onscreen(&buffer.get_insert());
                this.banner.set_title("The installation did not finish. What happened is below.");
                this.banner.set_button_label(Some("Try Again"));
                this.busy.set(false);
                return;
            }
            this.installed.set(true);
            this.banner.set_title("Installed. Checking this computer again…");
            let env = this.context.env.clone();
            let report = gio::spawn_blocking(move || examine(&env)).await;
            this.busy.set(false);
            if let Ok(report) = report {
                this.show_report(&report);
                if report.can_run_vms() {
                    this.toasts.add_toast(adw::Toast::new("This computer is ready for virtual machines"));
                }
            }
        });
    }
}
