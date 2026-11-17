//! roomy's window.
//!
//! Started by the `roomy` command, which stays free of graphical libraries
//! so that `roomy doctor` and `roomy setup` work on a computer where the
//! window cannot start yet.

mod create;
mod doctor;
mod guest;
mod lab;
mod library;
mod machine;
mod screen;
mod settings;

use adw::prelude::*;
use gtk::{gio, glib};
use roomy_core::env::Env;
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

const APP_ID: &str = "com.mudlej.Roomy";

/// What every window needs to know about its surroundings.
pub struct Context {
    pub env: Env,
    /// The `roomy` command, which is also what runs each VM's supervisor.
    pub roomy_program: PathBuf,
    /// The library file listing the user's VMs.
    pub library: PathBuf,
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
        (Some(roomy_program), Ok(library)) => Rc::new(Context { env, roomy_program, library }),
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
    // A test run must not be swallowed by a window the user already has open.
    let flags = if lab_dir.is_some() { gio::ApplicationFlags::NON_UNIQUE } else { gio::ApplicationFlags::empty() };
    let app = adw::Application::builder().application_id(APP_ID).flags(flags).build();
    let main_window: RefCell<Option<Rc<library::Library>>> = RefCell::new(None);
    app.connect_activate(move |app| {
        if let Some(open) = main_window.borrow().as_ref() {
            open.window().present();
            return;
        }
        // The icon installed under the application's name (see packaging/).
        gtk::Window::set_default_icon_name(APP_ID);
        let library = library::Library::open(&context, app);
        if let Some(dir) = lab_dir.clone() {
            lab::run(app, &library, dir);
        }
        main_window.replace(Some(library));
    });
    // Our own arguments are handled above; GTK gets none.
    app.run_with_args::<&str>(&[])
}
