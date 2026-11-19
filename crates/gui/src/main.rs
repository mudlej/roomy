//! roomy's window.
//!
//! Started by the `roomy` command, which stays free of graphical libraries
//! so that `roomy doctor` and `roomy setup` work on a computer where the
//! window cannot start yet.

/// A closure that holds `$owner` (an `Rc`) only weakly, and does nothing once it is gone.
macro_rules! on {
    ($owner:expr, |$this:ident $(, $arg:pat_param)*| $body:expr) => {{
        let weak = std::rc::Rc::downgrade($owner);
        move |$($arg),*| {
            if let Some($this) = weak.upgrade() {
                $body
            }
        }
    }};
}

mod create;
mod doctor;
mod downloads;
mod fields;
mod guest;
mod lab;
mod library;
mod machine;
mod screen;
mod settings;

use adw::prelude::*;
use gtk::{gio, glib};
use roomy_core::control::State;
use roomy_core::env::Env;
use roomy_core::launcher::APP_ID;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

/// What every window needs to know about its surroundings.
pub struct Context {
    pub env: Env,
    /// The `roomy` command, which is also what runs each VM's supervisor.
    pub roomy_program: PathBuf,
    /// The library file listing the user's VMs.
    pub library: PathBuf,
    /// Installation images being looked up and downloaded, for as long as the program runs.
    pub downloads: downloads::Shared,
}

/// The `roomy` command: named by whoever started us, else next to this program, else on the PATH.
fn roomy_program(env: &Env) -> Option<PathBuf> {
    let named = std::env::var_os("ROOMY_PROGRAM").map(PathBuf::from);
    let beside = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.join("roomy")));
    named.into_iter().chain(beside).find(|p| roomy_core::env::is_executable(p)).or_else(|| env.find_program("roomy"))
}

/// A row whose title and subtitle are shown exactly as given.
///
/// Rows read their text as markup unless told otherwise, and text such as a
/// command with `&&` or a folder name with `&` is not markup. The switch has
/// to be thrown on a finished row, before any text is set: while a row is
/// being built its settings only reach its labels afterwards, too late for
/// text set in the same breath.
pub fn plain_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::new();
    row.set_use_markup(false);
    row.set_title(title);
    row.set_subtitle(subtitle);
    row
}

/// Shows a short message over `toasts` for `seconds`, or until it is dismissed when `seconds` is 0.
pub fn toast(toasts: &adw::ToastOverlay, message: &str, seconds: u32) {
    // Messages carry paths and other programs' words: shown as they are, never read as markup.
    toasts.add_toast(adw::Toast::builder().use_markup(false).title(message).timeout(seconds).build());
}

/// Asks before something that cannot be undone. `then` runs if the user goes ahead.
pub fn confirm(parent: &impl IsA<gtk::Widget>, heading: &str, body: &str, go: &str, then: impl Fn() + 'static) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("cancel", "Cancel"), ("go", go)]);
    dialog.set_response_appearance("go", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.connect_response(None, move |_, response| {
        if response == "go" {
            then()
        }
    });
    dialog.present(Some(parent));
}

/// A message with details worth reading and copying, such as another program's own words.
pub fn show_copyable(parent: &impl IsA<gtk::Widget>, heading: &str, body: &str, copy: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("copy", copy), ("close", "Close")]);
    dialog.set_default_response(Some("close"));
    dialog.set_close_response("close");
    let body = body.to_owned();
    dialog.connect_response(Some("copy"), move |dialog, _| dialog.clipboard().set_text(&body));
    dialog.present(Some(parent));
}

/// A VM's state in the words the windows show.
pub fn state_words(state: State) -> &'static str {
    match state {
        State::Off => "Off",
        State::Suspended => "Suspended",
        State::Starting => "Starting…",
        State::Running => "Running",
        State::Paused => "Paused",
        State::Suspending => "Suspending…",
        State::Stopping => "Shutting down…",
    }
}

/// The graphics devices this program has open.
pub fn graphics_devices_open() -> Vec<PathBuf> {
    let mut devices: Vec<PathBuf> = std::fs::read_dir("/proc/self/fd")
        .map(|fds| {
            fds.filter_map(|fd| std::fs::read_link(fd.ok()?.path()).ok())
                .filter(|target| target.starts_with("/dev/dri"))
                .collect()
        })
        .unwrap_or_default();
    devices.sort();
    devices.dedup();
    devices
}

/// The graphics card this window is drawn with, when there is exactly one
/// candidate. A VM's accelerated screen must be drawn on the same card to be
/// shown without garbling, so VMs started from the window are told to use it.
pub fn window_render_node() -> Option<PathBuf> {
    let is_render_node = |p: &PathBuf| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("renderD"));
    let mut nodes = graphics_devices_open().into_iter().filter(is_render_node);
    match (nodes.next(), nodes.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

fn main() -> glib::ExitCode {
    if std::env::args().any(|arg| arg == "--probe") {
        // Run by `roomy doctor`: getting this far means the graphical libraries loaded.
        println!("roomy-gui {} ok", env!("CARGO_PKG_VERSION"));
        return glib::ExitCode::SUCCESS;
    }
    let env = Env::system();
    let context = match (roomy_program(&env), roomy_core::library::default_path()) {
        (Some(roomy_program), Ok(library)) => {
            Rc::new(Context { env, roomy_program, library, downloads: downloads::Shared::default() })
        }
        (None, _) => {
            eprintln!("roomy-gui: cannot find the 'roomy' command it belongs to; start the window with 'roomy'");
            return glib::ExitCode::FAILURE;
        }
        (_, Err(e)) => {
            eprintln!("roomy-gui: {e}");
            return glib::ExitCode::FAILURE;
        }
    };

    let lab_dir = lab::directory();
    // A single-file roomy has no launcher entry unless it keeps one itself, and without the entry
    // the desktop has no icon for this window. Not for a test run, which is nobody's application.
    if lab_dir.is_none()
        && let Some(file) = roomy_core::env::single_file_release()
        && let Err(e) = roomy_core::launcher::keep_for_user(&file)
    {
        eprintln!("roomy-gui: could not add roomy to the application menu: {e}");
    }
    // A test run must not be swallowed by a window the user already has open.
    let flags = if lab_dir.is_some() { gio::ApplicationFlags::NON_UNIQUE } else { gio::ApplicationFlags::empty() };
    let app = adw::Application::builder().application_id(APP_ID).flags(flags).build();
    let main_window: RefCell<Option<Rc<library::Library>>> = RefCell::new(None);
    app.connect_activate(move |app| {
        if let Some(open) = main_window.borrow().as_ref() {
            open.window().present();
            return;
        }
        // The icon filed under the application's name: by the package, or by the lines above.
        gtk::Window::set_default_icon_name(APP_ID);
        let library = library::Library::open(&context, app);
        downloads::fetch_screenshots(&context);
        if let Some(dir) = lab_dir.clone() {
            lab::run(app, &library, dir);
        }
        main_window.replace(Some(library));
    });
    // Our own arguments are handled above; GTK gets none.
    app.run_with_args::<&str>(&[])
}
