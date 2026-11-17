//! One VM's window: its screen, keyboard and pointer, and its power controls.
//!
//! The window never talks to QEMU itself. Power actions go to the VM's
//! supervisor (which can take a while, so never on the window's thread);
//! the screen comes over a private display connection the supervisor hands out.

use crate::Context;
use crate::screen::Screen;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use roomy_core::control::{self, Request, Running, State};
use roomy_core::settings;
use roomy_core::vm::{Config, Folder};
use roomy_display::audio::AudioHost;
use roomy_display::pixels::{Canvas, Mapping, Rect};
use roomy_display::pulse::PulseAudio;
use roomy_display::{Button, Display, Dmabuf, Event, Options, clipboard, keymap};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::os::fd::AsRawFd;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const PAUSE_EXPLANATION: &str = "Pause freezes the virtual machine in memory. Resuming is instant, but a paused machine is lost if this computer powers off.";
pub const SUSPEND_EXPLANATION: &str = "Suspend saves the virtual machine to disk and stops it. It survives a restart of this computer; Start continues exactly where it left off.";
pub const SUSPEND_UNAVAILABLE_WITH_GPU: &str = "Suspend is not available while GPU acceleration is on, because an accelerated graphics card cannot be saved to disk. Use Pause instead, or turn GPU acceleration off (applies at next boot).";
const FULLSCREEN_HINT: &str = "Press Ctrl+Alt+Enter to leave fullscreen";

/// What a menu entry does.
type MenuAction = Box<dyn Fn(&Rc<Machine>)>;

/// What one look at the VM's folder and supervisor found.
struct Polled {
    state: State,
    guest_agent: bool,
    config: Option<Config>,
    running: Option<Running>,
    waiting: Vec<&'static str>,
}

/// What the screen widget is currently fed from.
enum Source {
    Nothing,
    /// Pixels kept in `canvas` (sent inline, or copied out of shared memory).
    Pixels(Option<Arc<Mapping>>),
    /// A picture in graphics memory.
    Graphics(Dmabuf),
}

pub struct Machine {
    context: Rc<Context>,
    folder: Folder,
    /// The settings as last read from the VM's folder.
    config: RefCell<Config>,
    /// Whether the guest that is running was booted with GPU acceleration.
    booted_with_gpu: Cell<bool>,
    /// Settings changed since the guest booted that wait for its next boot.
    waiting: RefCell<Vec<&'static str>>,
    /// The running guest draws on another graphics card than this window.
    other_card: Cell<bool>,
    pub guest_agent: Cell<bool>,
    /// Sound through the desktop's sound server; `None` when its library is missing.
    sound: Option<Arc<PulseAudio>>,
    window: adw::ApplicationWindow,
    title: adw::WindowTitle,
    toolbar: adw::ToolbarView,
    banner: adw::Banner,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    status: adw::StatusPage,
    start_button: gtk::Button,
    pause_button: gtk::Button,
    suspend_button: gtk::Button,
    shutdown_button: gtk::Button,
    screen: Screen,

    state: Cell<State>,
    /// What an action in flight is doing ("Starting…"); also blocks further actions.
    activity: Cell<Option<&'static str>>,
    polling: Cell<bool>,

    display: RefCell<Option<Display>>,
    connecting: Cell<bool>,
    /// Attempts in a row that QEMU turned down. One is normal while a VM is shutting down.
    connect_failures: Cell<u32>,
    /// Counts display connections, so events of an earlier one are not mistaken for the current one's.
    display_generation: Cell<u64>,
    last_connect_attempt: Cell<Option<Instant>>,
    source: RefCell<Source>,
    canvas: RefCell<Canvas>,
    dirty: Cell<bool>,
    draw_scheduled: Cell<bool>,
    pointer_absolute: Cell<bool>,
    last_pointer: Cell<Option<(f64, f64)>>,
    /// For roomy's own test runs: the guest pixel last pointed at on their behalf, and what the guest last copied.
    lab_pointed_at: Cell<Option<(u32, u32)>>,
    lab_guest_copied: RefCell<Option<String>>,
    keys_down: RefCell<BTreeSet<u32>>,
    buttons_down: RefCell<Vec<Button>>,
    resize_generation: Cell<u64>,

    /// Counters and notes for roomy's own test runs.
    pub frames_drawn: Cell<u64>,
    /// When the small picture in the VM's folder was last saved, and which frame it shows.
    thumbnail_saved: Cell<Option<(Instant, u64)>>,
    pub screen_kind: Cell<&'static str>,
    /// How the guest described its graphics-memory picture.
    pub screen_detail: RefCell<String>,
    pub problems: RefCell<Vec<String>>,
}

impl Machine {
    pub fn open(context: &Rc<Context>, app: &adw::Application, folder: Folder, config: Config) -> Rc<Machine> {
        let title = adw::WindowTitle::new(&config.name, "");
        let sound = match PulseAudio::new(&config.name, config.features.microphone) {
            Ok(sound) => Some(Arc::new(sound)),
            Err(problem) => {
                eprintln!("roomy: {}: no sound: {problem}", config.name);
                None
            }
        };
        let header = adw::HeaderBar::new();
        header.set_title_widget(Some(&title));

        let pause_button = gtk::Button::with_label("Pause");
        let suspend_button = gtk::Button::with_label("Suspend");
        let shutdown_button = gtk::Button::with_label("Shut Down");
        shutdown_button.set_tooltip_text(Some("Ask the guest system to shut down, as its power button would"));
        for button in [&pause_button, &suspend_button, &shutdown_button] {
            header.pack_start(button);
        }

        let menu = gio::Menu::new();
        let power = gio::Menu::new();
        power.append(Some("Restart"), Some("win.restart"));
        power.append(Some("Send Ctrl+Alt+Delete"), Some("win.ctrl-alt-delete"));
        power.append(Some("Force Restart…"), Some("win.force-restart"));
        power.append(Some("Force Off…"), Some("win.force-off"));
        menu.append_section(None, &power);
        let view = gio::Menu::new();
        view.append(Some("Fullscreen (Ctrl+Alt+Enter)"), Some("win.fullscreen"));
        view.append(Some("Pause or Suspend?"), Some("win.explain"));
        let setup = gio::Menu::new();
        setup.append(Some("Settings…"), Some("win.settings"));
        setup.append(Some("Guest Setup…"), Some("win.guest-setup"));
        menu.append_section(None, &setup);
        menu.append_section(None, &view);
        let saved = gio::Menu::new();
        saved.append(Some("Discard Saved State…"), Some("win.discard-state"));
        menu.append_section(None, &saved);
        header.pack_end(
            &gtk::MenuButton::builder().icon_name("open-menu-symbolic").menu_model(&menu).tooltip_text("Menu").build(),
        );

        let banner = adw::Banner::new("");
        banner.set_use_markup(false);
        let screen = Screen::new();
        let start_button = gtk::Button::with_label("Start");
        start_button.set_halign(gtk::Align::Center);
        start_button.add_css_class("pill");
        start_button.add_css_class("suggested-action");
        let status = adw::StatusPage::builder().icon_name("computer-symbolic").child(&start_button).build();
        let stack = gtk::Stack::new();
        stack.add_named(&status, Some("status"));
        stack.add_named(&screen, Some("screen"));
        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&stack));

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&header);
        toolbar.add_top_bar(&banner);
        toolbar.set_content(Some(&toasts));
        let window = adw::ApplicationWindow::builder()
            .application(app)
            .title(config.name.as_str())
            .default_width(1024)
            .default_height(768)
            .content(&toolbar)
            .build();

        let machine = Rc::new(Machine {
            context: context.clone(),
            folder,
            booted_with_gpu: Cell::new(config.features.gpu_acceleration),
            waiting: RefCell::new(Vec::new()),
            other_card: Cell::new(false),
            guest_agent: Cell::new(false),
            sound,
            config: RefCell::new(config),
            window,
            title,
            toolbar,
            banner,
            toasts,
            stack,
            status,
            start_button,
            pause_button,
            suspend_button,
            shutdown_button,
            screen,
            state: Cell::new(State::Off),
            activity: Cell::new(None),
            polling: Cell::new(false),
            display: RefCell::new(None),
            connecting: Cell::new(false),
            connect_failures: Cell::new(0),
            display_generation: Cell::new(0),
            last_connect_attempt: Cell::new(None),
            source: RefCell::new(Source::Nothing),
            canvas: RefCell::new(Canvas::default()),
            dirty: Cell::new(false),
            draw_scheduled: Cell::new(false),
            pointer_absolute: Cell::new(true),
            last_pointer: Cell::new(None),
            lab_pointed_at: Cell::new(None),
            lab_guest_copied: RefCell::new(None),
            keys_down: RefCell::new(BTreeSet::new()),
            buttons_down: RefCell::new(Vec::new()),
            resize_generation: Cell::new(0),
            frames_drawn: Cell::new(0),
            thumbnail_saved: Cell::new(None),
            screen_kind: Cell::new("none"),
            screen_detail: RefCell::new(String::new()),
            problems: RefCell::new(Vec::new()),
        });
        machine.pause_button.set_tooltip_text(Some(PAUSE_EXPLANATION));
        machine.wire_controls();
        machine.wire_input();
        machine.refresh();
        machine.poll();

        // Keep the state current for as long as the window exists.
        glib::timeout_add_local(Duration::from_millis(500), {
            let weak = Rc::downgrade(&machine);
            move || match weak.upgrade() {
                Some(machine) => {
                    machine.poll();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            }
        });
        machine.window.present();
        machine
    }

    pub fn window(&self) -> &adw::ApplicationWindow {
        &self.window
    }

    pub fn folder(&self) -> &Folder {
        &self.folder
    }

    pub fn state(&self) -> State {
        self.state.get()
    }

    /// What the window currently tells the user, for roomy's own test runs.
    pub fn lab_facts(&self) -> serde_json::Value {
        let sound = self.sound.as_ref().map(|sound| {
            let (received, played, recorded, requested) = sound.totals();
            serde_json::json!({ "received": received, "played": played, "microphone_recorded": recorded, "microphone_requested": requested })
        });
        serde_json::json!({
            "banner": if self.banner.is_revealed() { Some(self.banner.title().to_string()) } else { None },
            "waiting_for_next_boot": self.waiting.borrow().clone(),
            "other_graphics_card": self.other_card.get(),
            "guest_agent": self.guest_agent.get(),
            "pointer_absolute": self.pointer_absolute.get(),
            "pointed_at": self.lab_pointed_at.get(),
            "guest_copied": self.lab_guest_copied.borrow().clone(),
            "sound": sound,
            "name": self.config.borrow().name.clone(),
        })
    }

    pub fn screen(&self) -> &Screen {
        &self.screen
    }

    fn wire_controls(self: &Rc<Self>) {
        self.start_button.connect_clicked(on!(self, |this, _| this.start()));
        self.banner.connect_button_clicked(on!(self, |this, _| {
            if this.state.get() == State::Paused {
                this.request(Request::Resume, "Resuming…")
            }
        }));
        self.pause_button.connect_clicked(on!(self, |this, _| {
            if this.state.get() == State::Paused {
                this.request(Request::Resume, "Resuming…")
            } else {
                this.request(Request::Pause, "Pausing…")
            }
        }));
        self.suspend_button.connect_clicked(on!(self, |this, _| this.request(Request::Suspend, "Suspending…")));
        self.shutdown_button.connect_clicked(on!(self, |this, _| this.request(Request::Shutdown, "Shutting down…")));

        let add = |name: &str, run: MenuAction| {
            let action = gio::SimpleAction::new(name, None);
            action.connect_activate(on!(self, |this, _, _| run(&this)));
            self.window.add_action(&action);
        };
        add("restart", Box::new(|this| this.request(Request::Restart, "Restarting…")));
        add(
            "settings",
            Box::new(|this| {
                crate::settings::show(
                    &this.context,
                    &this.window,
                    &this.folder,
                    on!(this, |this, summary| {
                        this.tell(&summary);
                        this.poll();
                    }),
                )
            }),
        );
        add(
            "guest-setup",
            Box::new(|this| {
                let alive = matches!(this.state.get(), State::Running | State::Paused);
                crate::guest::show(&this.window, &this.config.borrow(), alive.then(|| this.guest_agent.get()))
            }),
        );
        add("ctrl-alt-delete", Box::new(|this| this.send_ctrl_alt_delete()));
        add("fullscreen", Box::new(|this| this.toggle_fullscreen()));
        add(
            "explain",
            Box::new(|this| {
                let body = format!("{PAUSE_EXPLANATION}\n\n{SUSPEND_EXPLANATION}\n\n{SUSPEND_UNAVAILABLE_WITH_GPU}");
                let dialog = adw::AlertDialog::new(Some("Pause or Suspend?"), Some(&body));
                dialog.add_responses(&[("close", "Close")]);
                dialog.present(Some(&this.window));
            }),
        );
        add(
            "force-off",
            Box::new(|this| {
                crate::confirm(
                    &this.window,
                    "Force this virtual machine off?",
                    "This is like pulling the plug: anything not saved inside the virtual machine is lost.",
                    "Force Off",
                    on!(this, |this| this.request(Request::ForceOff, "Stopping…")),
                )
            }),
        );
        add(
            "force-restart",
            Box::new(|this| {
                crate::confirm(
                    &this.window,
                    "Force this virtual machine to restart?",
                    "This is like pressing a reset button: anything not saved inside the virtual machine is lost.",
                    "Force Restart",
                    on!(this, |this| this.request(Request::ForceRestart, "Restarting…")),
                )
            }),
        );
        add(
            "discard-state",
            Box::new(|this| {
                crate::confirm(
                    &this.window,
                    "Discard the saved state?",
                    "The virtual machine will start fresh instead of continuing where it was suspended. Anything that was not saved inside it is lost.",
                    "Discard",
                    on!(this, |this| {
                        if let Err(e) = this.folder.discard_saved_state() {
                            this.tell(&e.to_string());
                        }
                        this.poll();
                    }),
                )
            }),
        );
    }

    /// Shows a short message at the bottom of the window.
    pub fn tell(&self, message: &str) {
        crate::toast(&self.toasts, message, 8);
    }

    /// Sends a power request to the VM's supervisor without blocking the window.
    fn request(self: &Rc<Self>, request: Request, doing: &'static str) {
        if self.activity.get().is_some() {
            return;
        }
        self.activity.set(Some(doing));
        self.refresh();
        let (env, folder) = (self.context.env.clone(), self.folder.clone());
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome =
                gio::spawn_blocking(move || control::request(&env, &folder, request).map_err(|e| e.to_string())).await;
            let Some(this) = weak.upgrade() else { return };
            this.activity.set(None);
            match outcome {
                Ok(Ok(reply)) => {
                    if let Some(note) = reply.note {
                        this.tell(&note);
                    }
                    this.apply(reply.state);
                }
                Ok(Err(e)) => this.tell(&e),
                Err(_) => this.tell("roomy ran into an internal error carrying that out"),
            }
            this.poll();
        });
    }

    /// Boots the VM, or continues it if it is suspended.
    pub fn start(self: &Rc<Self>) {
        if self.activity.get().is_some() || !matches!(self.state.get(), State::Off | State::Suspended) {
            return;
        }
        self.activity.set(Some(if self.state.get() == State::Suspended { "Continuing…" } else { "Starting…" }));
        self.refresh();
        let (mut env, folder, roomy) =
            (self.context.env.clone(), self.folder.clone(), self.context.roomy_program.clone());
        // Have the VM draw on the graphics card this window is drawn with.
        env.render_node = env.render_node.or_else(crate::window_render_node);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome =
                gio::spawn_blocking(move || control::start(&env, &folder, &roomy).map_err(|e| e.to_string())).await;
            let Some(this) = weak.upgrade() else { return };
            this.activity.set(None);
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    crate::show_copyable(&this.window, "The virtual machine could not be started", &e, "Copy Message")
                }
                Err(_) => this.tell("roomy ran into an internal error starting the virtual machine"),
            }
            this.poll();
        });
    }

    /// Asks the supervisor for the current state, off the window's thread.
    pub fn poll(self: &Rc<Self>) {
        if self.polling.replace(true) {
            return;
        }
        let (env, folder) = (self.context.env.clone(), self.folder.clone());
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let polled = gio::spawn_blocking(move || {
                let (state, guest_agent) = match control::send(&env, &folder, Request::Status) {
                    Ok(reply) => (reply.state, reply.guest_agent),
                    Err(_) if folder.is_suspended() => (State::Suspended, false),
                    Err(_) => (State::Off, false),
                };
                Polled {
                    state,
                    guest_agent,
                    config: folder.load().ok(),
                    running: control::running(&env, &folder),
                    waiting: settings::waiting_for_next_boot(&env, &folder),
                }
            })
            .await;
            let Some(this) = weak.upgrade() else { return };
            this.polling.set(false);
            if let Ok(polled) = polled {
                this.take_in(polled);
            }
        });
    }

    /// Brings what the window knows in line with the VM's folder and supervisor.
    fn take_in(self: &Rc<Self>, polled: Polled) {
        if let Some(config) = polled.config.filter(|config| *config != *self.config.borrow()) {
            self.title.set_title(&config.name);
            self.window.set_title(Some(&config.name));
            if let Some(sound) = &self.sound {
                sound.set_microphone(config.features.microphone);
            }
            if !config.features.clipboard
                && let Some(display) = &*self.display.borrow()
            {
                display.clipboard_offer(None);
            }
            self.config.replace(config);
        }
        let booted = polled.running.as_ref().map(|running| running.config.features.gpu_acceleration);
        self.booted_with_gpu.set(booted.unwrap_or(self.config.borrow().features.gpu_acceleration));
        let (theirs, ours) = (polled.running.and_then(|running| running.render_node), crate::window_render_node());
        self.other_card.set(matches!((&theirs, &ours), (Some(theirs), Some(ours)) if theirs != ours));
        self.waiting.replace(polled.waiting);
        self.guest_agent.set(polled.guest_agent);
        self.apply(polled.state);
    }

    fn apply(self: &Rc<Self>, state: State) {
        self.state.set(state);
        let alive = matches!(state, State::Running | State::Paused | State::Stopping);
        if state == State::Running {
            self.save_thumbnail(Some(Duration::from_secs(20)));
        }
        if alive && self.display.borrow().is_none() {
            self.connect_display();
        }
        if !alive && state != State::Suspending {
            self.disconnect_display();
        }
        self.refresh();
    }

    /// Brings every control in line with the current state.
    fn refresh(&self) {
        let state = self.state.get();
        let busy = self.activity.get().is_some();
        self.title.set_subtitle(self.activity.get().unwrap_or(crate::state_words(state)));

        let alive = matches!(state, State::Running | State::Paused | State::Stopping);
        for button in [&self.pause_button, &self.suspend_button, &self.shutdown_button] {
            button.set_visible(alive);
        }
        let steady = !busy && matches!(state, State::Running | State::Paused);
        self.pause_button.set_sensitive(steady);
        self.pause_button.set_label(if state == State::Paused { "Resume" } else { "Pause" });
        let gpu = if alive { self.booted_with_gpu.get() } else { self.config.borrow().features.gpu_acceleration };
        let tip = if gpu { SUSPEND_UNAVAILABLE_WITH_GPU } else { SUSPEND_EXPLANATION };
        // Only when it changes: setting a tooltip again would close one that is showing.
        if self.suspend_button.tooltip_text().as_deref() != Some(tip) {
            self.suspend_button.set_tooltip_text(Some(tip));
        }
        self.suspend_button.set_sensitive(steady && !gpu);
        self.shutdown_button.set_sensitive(!busy && state == State::Running);
        for (name, enabled) in [
            ("restart", !busy && state == State::Running),
            ("ctrl-alt-delete", state == State::Running && self.display.borrow().is_some()),
            ("force-restart", !busy && alive),
            ("force-off", !busy && alive),
            ("discard-state", !busy && state == State::Suspended),
        ] {
            if let Some(action) = self.window.lookup_action(name).and_downcast::<gio::SimpleAction>() {
                action.set_enabled(enabled);
            }
        }

        // One banner, for the most pressing thing to know.
        let waiting = self.waiting.borrow();
        let (message, button) = if state == State::Paused && !busy {
            (
                "Paused: frozen in memory. Resuming is instant; a paused machine is lost if this computer powers off."
                    .to_owned(),
                Some("Resume"),
            )
        } else if alive && self.other_card.get() {
            ("This virtual machine draws its screen on a different graphics card than this window, so the picture may look scrambled. Shut it down and start it from this window to fix that.".to_owned(), None)
        } else if !waiting.is_empty() {
            (
                format!(
                    "Changes to {} take effect the next time this virtual machine is shut down and started.",
                    waiting.join(", ")
                ),
                None,
            )
        } else {
            (String::new(), None)
        };
        if self.banner.title() != message {
            self.banner.set_title(&message);
        }
        self.banner.set_button_label(button);
        self.banner.set_revealed(!message.is_empty());

        let show_screen = alive && self.screen.picture().is_some();
        self.stack.set_visible_child_name(if show_screen { "screen" } else { "status" });
        let (heading, text, can_start) = match state {
            State::Off => ("This virtual machine is off", String::new(), true),
            State::Suspended => {
                ("Suspended", "Its state is saved on disk. Start continues exactly where it left off.".to_owned(), true)
            }
            State::Suspending => ("Suspending…", "Saving the virtual machine to disk.".to_owned(), false),
            State::Starting => ("Starting…", String::new(), false),
            _ => ("Waiting for the screen…", String::new(), false),
        };
        self.status.set_title(self.activity.get().unwrap_or(heading));
        self.status.set_description(if text.is_empty() || busy { None } else { Some(text.as_str()) });
        self.start_button.set_label(if state == State::Suspended { "Continue" } else { "Start" });
        self.start_button.set_visible(can_start && !busy);
    }

    fn connect_display(self: &Rc<Self>) {
        // One attempt at a time, and not in a tight loop if QEMU keeps refusing.
        let recently = self.last_connect_attempt.get().is_some_and(|at| at.elapsed() < Duration::from_secs(2));
        if self.connecting.get() || recently {
            return;
        }
        self.connecting.set(true);
        self.last_connect_attempt.set(Some(Instant::now()));
        let (env, folder) = (self.context.env.clone(), self.folder.clone());
        let (events_in, events) = async_channel::unbounded::<Event>();
        // The clipboard is always connected; whether text actually crosses is decided per copy, so the
        // setting takes effect at once.
        let options = Options { audio: self.sound.clone().map(|sound| sound as Arc<dyn AudioHost>), clipboard: true };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = gio::spawn_blocking(move || {
                let socket = control::open_display(&env, &folder).map_err(|e| e.to_string())?;
                // Called on the display thread: only hand the event over.
                Display::connect(socket, options, move |event| {
                    let _ = events_in.try_send(event);
                })
            })
            .await;
            let Some(this) = weak.upgrade() else { return };
            this.connecting.set(false);
            match outcome {
                Ok(Ok(display)) => {
                    this.connect_failures.set(0);
                    this.pointer_absolute.set(display.info().pointer_absolute);
                    let generation = this.display_generation.get() + 1;
                    this.display_generation.set(generation);
                    this.display.replace(Some(display));
                    this.send_window_size();
                    this.refresh();
                    drop(this);
                    while let Ok(event) = events.recv().await {
                        let Some(this) = weak.upgrade() else { break };
                        if this.display_generation.get() != generation {
                            break;
                        }
                        this.on_event(event);
                    }
                }
                Ok(Err(e)) => {
                    // A VM that is just shutting down turns the window away once; that is not a problem.
                    // The next look at its state either stops the attempts or leads to a second one.
                    this.connect_failures.set(this.connect_failures.get() + 1);
                    if this.connect_failures.get() >= 2 {
                        this.note_problem(format!("display: {e}"));
                    }
                }
                Err(_) => this.note_problem("display: internal error while connecting".to_owned()),
            }
        });
    }

    /// Keeps a small picture of the screen in the VM's folder, which the library's list shows.
    /// `at_least_every` spaces the saves out while the VM runs; `None` saves now.
    fn save_thumbnail(&self, at_least_every: Option<Duration>) {
        let frame = self.frames_drawn.get();
        let due = match (self.thumbnail_saved.get(), at_least_every) {
            (Some((_, saved_frame)), _) if saved_frame == frame => false,
            (Some((at, _)), Some(interval)) => at.elapsed() >= interval,
            _ => true,
        };
        if !due {
            return;
        }
        let Some(small) = self.screen.small_copy(480.0, 300.0) else { return };
        let path = self.folder.thumbnail();
        // Written whole or not at all, so the library never shows half a picture.
        let partial = path.with_extension("png.partial");
        if small.save_to_png(&partial).is_ok() && std::fs::rename(&partial, &path).is_ok() {
            self.thumbnail_saved.set(Some((Instant::now(), frame)));
        } else {
            let _ = std::fs::remove_file(&partial);
        }
    }

    fn disconnect_display(&self) {
        // The last thing the screen showed: for a suspended VM, where it will continue.
        self.save_thumbnail(None);
        self.connect_failures.set(0);
        self.display_generation.set(self.display_generation.get() + 1);
        self.display.replace(None);
        self.source.replace(Source::Nothing);
        self.canvas.replace(Canvas::default());
        self.keys_down.borrow_mut().clear();
        self.buttons_down.borrow_mut().clear();
        self.screen.set_picture(None, false);
        self.screen.set_cursor(None);
        self.screen_kind.set("none");
    }

    fn note_problem(&self, problem: String) {
        let mut problems = self.problems.borrow_mut();
        // Keep the first few: a problem that repeats every frame must not grow without bound.
        if problems.len() < 20 && !problems.contains(&problem) {
            eprintln!("roomy: {}: {problem}", self.config.borrow().name);
            problems.push(problem);
        }
    }

    fn on_event(self: &Rc<Self>, event: Event) {
        let area = |x: i32, y: i32, width: i32, height: i32| -> Option<Rect> {
            let positive = |v: i32| usize::try_from(v).ok();
            Some(Rect { x: positive(x)?, y: positive(y)?, width: positive(width)?, height: positive(height)? })
        };
        let drawn = match event {
            Event::Scanout { width, height, stride, format, data } => {
                self.source.replace(Source::Pixels(None));
                self.screen_kind.set("pixels");
                self.canvas
                    .borrow_mut()
                    .set(width as usize, height as usize, stride as usize, format, &data)
                    .map_err(|e| e.to_string())
            }
            Event::Update { x, y, width, height, stride, format, data } => match area(x, y, width, height) {
                Some(area) => {
                    self.canvas.borrow_mut().draw(area, stride as usize, format, &data).map_err(|e| e.to_string())
                }
                None => Err("an update with a negative position".to_owned()),
            },
            Event::ScanoutMap(map) => {
                let drawn = self.canvas.borrow_mut().draw_mapped(&map, None).map_err(|e| e.to_string());
                self.source.replace(Source::Pixels(Some(map)));
                self.screen_kind.set("shared memory");
                drawn
            }
            Event::UpdateMap { x, y, width, height } => match &*self.source.borrow() {
                Source::Pixels(Some(map)) => {
                    self.canvas.borrow_mut().draw_mapped(map, area(x, y, width, height)).map_err(|e| e.to_string())
                }
                _ => Ok(()),
            },
            Event::ScanoutDmabuf(dmabuf) => {
                let code: String = dmabuf.fourcc.to_le_bytes().iter().map(|b| char::from(*b)).collect();
                self.screen_detail.replace(format!(
                    "{}x{} stride {} format {code} modifier {:#x} first row is the {}",
                    dmabuf.width,
                    dmabuf.height,
                    dmabuf.stride,
                    dmabuf.modifier,
                    if dmabuf.stored_bottom_up { "bottom" } else { "top" }
                ));
                self.canvas.replace(Canvas::default());
                self.source.replace(Source::Graphics(dmabuf));
                self.screen_kind.set("graphics memory");
                Ok(())
            }
            Event::UpdateDmabuf { .. } => Ok(()),
            Event::Disable => Ok(()),
            Event::MouseSet { .. } => return,
            Event::CursorDefine { width, height, hot_x, hot_y, data } => {
                self.set_guest_cursor(width, height, hot_x, hot_y, &data);
                return;
            }
            Event::PointerMode { absolute } => {
                self.pointer_absolute.set(absolute);
                self.last_pointer.set(None);
                return;
            }
            Event::ClipboardOffered { mimes } => {
                let wanted =
                    self.config.borrow().features.clipboard && mimes.iter().any(|mime| mime == clipboard::TEXT);
                if let (true, Some(display)) = (wanted, &*self.display.borrow()) {
                    display.clipboard_request(on!(self, |this, text| {
                        // Our own copy of the guest's text: `offer_host_clipboard` will not send it back.
                        if let Ok(text) = text {
                            this.window.clipboard().set_text(&text);
                            if crate::lab::directory().is_some() {
                                this.lab_guest_copied.replace(Some(text));
                            }
                        }
                    }));
                }
                return;
            }
            Event::ClipboardReleased => return,
            Event::Problem(problem) => Err(problem),
            Event::Closed => {
                self.disconnect_display();
                self.refresh();
                self.poll();
                return;
            }
        };
        match drawn {
            Ok(()) => self.schedule_draw(),
            Err(problem) => self.note_problem(problem),
        }
    }

    /// Asks for the picture to be brought up to date on the window's next
    /// frame. Many updates between two frames cost one redraw, and nothing
    /// runs at all while the guest's screen is still.
    fn schedule_draw(self: &Rc<Self>) {
        self.dirty.set(true);
        if self.draw_scheduled.replace(true) {
            return;
        }
        // The window's clock, not the screen widget's: the widget is hidden until the first picture exists.
        let weak = Rc::downgrade(self);
        self.window.add_tick_callback(move |_, _| {
            if let Some(this) = weak.upgrade() {
                this.draw_scheduled.set(false);
                this.draw_frame();
            }
            glib::ControlFlow::Break
        });
    }

    /// Turns what has arrived since the last frame into the picture on screen.
    fn draw_frame(&self) {
        if !self.dirty.replace(false) {
            return;
        }
        let picture: Result<(gdk::Texture, bool), String> = match &*self.source.borrow() {
            Source::Nothing => return,
            Source::Pixels(_) => {
                let canvas = self.canvas.borrow();
                if canvas.is_empty() {
                    return;
                }
                let texture = gdk::MemoryTexture::new(
                    canvas.width() as i32,
                    canvas.height() as i32,
                    gdk::MemoryFormat::B8g8r8x8,
                    &glib::Bytes::from(canvas.pixels()),
                    canvas.stride(),
                );
                Ok((texture.upcast(), false))
            }
            Source::Graphics(dmabuf) => {
                graphics_texture(&self.screen, dmabuf).map(|texture| (texture, dmabuf.stored_bottom_up))
            }
        };
        match picture {
            Ok((texture, upside_down)) => {
                let first = self.screen.picture().is_none();
                self.screen.set_picture(Some(texture), upside_down);
                self.frames_drawn.set(self.frames_drawn.get() + 1);
                if first {
                    self.refresh();
                    self.screen.grab_focus();
                }
            }
            Err(problem) => self.note_problem(problem),
        }
    }

    fn set_guest_cursor(&self, width: i32, height: i32, hot_x: i32, hot_y: i32, data: &[u8]) {
        let fits =
            width > 0 && height > 0 && width <= 512 && height <= 512 && data.len() >= (width * height * 4) as usize;
        if !fits {
            return;
        }
        let texture = gdk::MemoryTexture::new(
            width,
            height,
            gdk::MemoryFormat::B8g8r8a8Premultiplied,
            &glib::Bytes::from(&data[..(width * height * 4) as usize]),
            (width * 4) as usize,
        );
        let fallback = gdk::Cursor::from_name("default", None);
        let cursor = gdk::Cursor::from_texture(
            &texture,
            hot_x.clamp(0, width - 1),
            hot_y.clamp(0, height - 1),
            fallback.as_ref(),
        );
        self.screen.set_cursor(Some(&cursor));
    }

    /// Tells the guest how big the window's screen area is, once the size has settled.
    fn send_window_size(self: &Rc<Self>) {
        let generation = self.resize_generation.get() + 1;
        self.resize_generation.set(generation);
        glib::timeout_add_local_once(
            Duration::from_millis(300),
            on!(self, |this| {
                if this.resize_generation.get() != generation {
                    return;
                }
                let scale = this.screen.scale_factor().max(1);
                let (width, height) = (this.screen.width() * scale, this.screen.height() * scale);
                if let (Some(display), true) = (this.display.borrow().as_ref(), width >= 320 && height >= 200) {
                    display.set_window_size(width as u32, height as u32, 0, 0);
                }
            }),
        );
    }

    fn wire_input(self: &Rc<Self>) {
        // What is copied on this computer is offered to the guest while this window is in front.
        self.window.clipboard().connect_changed(on!(self, |this, _| this.offer_host_clipboard()));
        self.window.connect_is_active_notify(on!(self, |this, _| this.offer_host_clipboard()));
        self.screen.connect_resized(on!(self, |this, _, _| this.send_window_size()));

        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed({
            let weak = Rc::downgrade(self);
            move |_, key, keycode, modifiers| match weak.upgrade() {
                Some(this) => this.key_pressed(key, keycode, modifiers),
                None => glib::Propagation::Proceed,
            }
        });
        keys.connect_key_released(on!(self, |this, _, _, keycode, _| this.key_released(keycode)));
        self.screen.add_controller(keys);

        let focus = gtk::EventControllerFocus::new();
        focus.connect_enter(on!(self, |this, _| this.capture_shortcuts(true)));
        focus.connect_leave(on!(self, |this, _| {
            this.release_everything();
            this.capture_shortcuts(false);
        }));
        self.screen.add_controller(focus);

        let motion = gtk::EventControllerMotion::new();
        motion.connect_motion(on!(self, |this, _, x, y| this.pointer_moved(x, y)));
        motion.connect_leave(on!(self, |this, _| this.last_pointer.set(None)));
        self.screen.add_controller(motion);

        let click = gtk::GestureClick::new();
        click.set_button(0);
        click.connect_pressed(on!(self, |this, gesture, _, x, y| {
            this.screen.grab_focus();
            this.pointer_moved(x, y);
            if let Some(button) = guest_button(gesture.current_button()) {
                this.button(button, true);
            }
        }));
        click.connect_released(on!(self, |this, gesture, _, _, _| {
            if let Some(button) = guest_button(gesture.current_button()) {
                this.button(button, false);
            }
        }));
        self.screen.add_controller(click);

        let scroll = gtk::EventControllerScroll::new(
            gtk::EventControllerScrollFlags::BOTH_AXES | gtk::EventControllerScrollFlags::DISCRETE,
        );
        scroll.connect_scroll({
            let weak = Rc::downgrade(self);
            move |_, dx, dy| {
                let Some(this) = weak.upgrade() else { return glib::Propagation::Proceed };
                let wheel = if dy < 0.0 {
                    Some(Button::WheelUp)
                } else if dy > 0.0 {
                    Some(Button::WheelDown)
                } else if dx < 0.0 {
                    Some(Button::WheelLeft)
                } else if dx > 0.0 {
                    Some(Button::WheelRight)
                } else {
                    None
                };
                if let (Some(wheel), Some(display)) = (wheel, this.display.borrow().as_ref()) {
                    display.button(wheel, true);
                    display.button(wheel, false);
                }
                glib::Propagation::Stop
            }
        });
        self.screen.add_controller(scroll);
    }

    /// Offers this computer's clipboard text to the guest.
    fn offer_host_clipboard(self: &Rc<Self>) {
        let clipboard = self.window.clipboard();
        // Text this window put there came from the guest; it is not sent back.
        let wanted = self.config.borrow().features.clipboard
            && self.window.is_active()
            && !clipboard.is_local()
            && self.display.borrow().is_some();
        if !wanted {
            return;
        }
        clipboard.read_text_async(
            gio::Cancellable::NONE,
            on!(self, |this, text| {
                if let (Ok(Some(text)), Some(display)) = (text, &*this.display.borrow()) {
                    display.clipboard_offer(Some(text.as_str()));
                }
            }),
        );
    }

    /// While the guest's screen has the keyboard, desktop shortcuts such as
    /// Alt+Tab go to the guest. Clicking outside the screen gives them back.
    fn capture_shortcuts(&self, capture: bool) {
        let Some(toplevel) = self.window.surface().and_downcast::<gdk::Toplevel>() else { return };
        if capture {
            toplevel.inhibit_system_shortcuts(None::<&gdk::Event>);
        } else {
            toplevel.restore_system_shortcuts();
        }
    }

    fn key_pressed(self: &Rc<Self>, key: gdk::Key, keycode: u32, modifiers: gdk::ModifierType) -> glib::Propagation {
        let ctrl_alt = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK;
        if modifiers.contains(ctrl_alt) && matches!(key, gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter) {
            self.toggle_fullscreen();
            return glib::Propagation::Stop;
        }
        let Some(display) = &*self.display.borrow() else { return glib::Propagation::Proceed };
        // The window system numbers keys 8 higher than the kernel does.
        if let Some(qnum) = keycode.checked_sub(8).and_then(keymap::qnum_from_linux) {
            self.keys_down.borrow_mut().insert(qnum);
            display.key(qnum, true);
        }
        // Everything typed here belongs to the guest, including Tab and the arrow keys.
        glib::Propagation::Stop
    }

    fn key_released(&self, keycode: u32) {
        let Some(qnum) = keycode.checked_sub(8).and_then(keymap::qnum_from_linux) else { return };
        if self.keys_down.borrow_mut().remove(&qnum)
            && let Some(display) = &*self.display.borrow()
        {
            display.key(qnum, false);
        }
    }

    /// Lets go of every key and button the guest still believes is held, so
    /// nothing stays stuck when the window loses the keyboard.
    fn release_everything(&self) {
        let keys = std::mem::take(&mut *self.keys_down.borrow_mut());
        let buttons = std::mem::take(&mut *self.buttons_down.borrow_mut());
        if let Some(display) = &*self.display.borrow() {
            for qnum in keys {
                display.key(qnum, false);
            }
            for button in buttons {
                display.button(button, false);
            }
        }
    }

    fn pointer_moved(&self, x: f64, y: f64) {
        let Some(display) = &*self.display.borrow() else { return };
        if self.pointer_absolute.get() {
            if let Some((guest_x, guest_y)) = self.screen.guest_position(x, y) {
                display.pointer_position(guest_x, guest_y);
            }
        } else if let Some((last_x, last_y)) = self.last_pointer.get() {
            let (dx, dy) = ((x - last_x).round() as i32, (y - last_y).round() as i32);
            if (dx, dy) != (0, 0) {
                display.pointer_motion(dx, dy);
            }
        }
        self.last_pointer.set(Some((x, y)));
    }

    fn button(&self, button: Button, pressed: bool) {
        let Some(display) = &*self.display.borrow() else { return };
        let mut down = self.buttons_down.borrow_mut();
        down.retain(|held| *held != button);
        if pressed {
            down.push(button);
        }
        display.button(button, pressed);
    }

    fn send_ctrl_alt_delete(&self) {
        if let Some(display) = &*self.display.borrow() {
            let combination = [keymap::keys::LEFT_CTRL, keymap::keys::LEFT_ALT, keymap::keys::DELETE];
            for qnum in combination {
                display.key(qnum, true);
            }
            for qnum in combination.into_iter().rev() {
                display.key(qnum, false);
            }
        }
    }

    /// Points at a place on the screen given as fractions of its area in the window, the way the real
    /// pointer would, and optionally clicks there (used by roomy's own test runs).
    pub fn lab_point(&self, across: f64, down: f64, click: bool) {
        let (x, y) = (across * f64::from(self.screen.width()), down * f64::from(self.screen.height()));
        self.lab_pointed_at.set(self.screen.guest_position(x, y));
        self.pointer_moved(x, y);
        if click {
            self.button(Button::Left, true);
            self.button(Button::Left, false);
        }
    }

    /// Offers `text` to the guest as if it had just been copied on this computer (used by roomy's own
    /// test runs, which cannot count on this window being the one in front).
    pub fn lab_offer_clipboard(&self, text: &str) {
        if let Some(display) = &*self.display.borrow() {
            display.clipboard_offer(Some(text));
        }
    }

    /// Types a key by its Linux input code (used by roomy's own test runs).
    pub fn tap_key(&self, linux_code: u32) {
        if let (Some(display), Some(qnum)) = (&*self.display.borrow(), keymap::qnum_from_linux(linux_code)) {
            display.key(qnum, true);
            display.key(qnum, false);
        }
    }

    fn toggle_fullscreen(&self) {
        self.release_everything();
        let entering = !self.window.is_fullscreen();
        if entering {
            self.window.fullscreen();
            self.tell(FULLSCREEN_HINT);
        } else {
            self.window.unfullscreen();
        }
        self.toolbar.set_reveal_top_bars(!entering);
        self.screen.grab_focus();
    }
}

fn guest_button(window_button: u32) -> Option<Button> {
    match window_button {
        1 => Some(Button::Left),
        2 => Some(Button::Middle),
        3 => Some(Button::Right),
        8 => Some(Button::Side),
        9 => Some(Button::Extra),
        _ => None,
    }
}

/// Wraps a picture that lives in graphics memory so it can be drawn without copying.
fn graphics_texture(screen: &Screen, dmabuf: &Dmabuf) -> Result<gdk::Texture, String> {
    // The texture keeps its own descriptor, closed when the texture is released.
    let fd = dmabuf.fd.try_clone().map_err(|e| format!("cannot share the guest's picture with the window: {e}"))?;
    let builder = gdk::DmabufTextureBuilder::new()
        .set_display(&screen.display())
        .set_width(dmabuf.width)
        .set_height(dmabuf.height)
        .set_fourcc(dmabuf.fourcc)
        .set_modifier(dmabuf.modifier)
        .set_n_planes(1)
        .set_offset(0, 0)
        .set_stride(0, dmabuf.stride);
    // SAFETY: `fd` is a valid dmabuf descriptor that stays open until the release function runs.
    unsafe { builder.set_fd(0, fd.as_raw_fd()).build_with_release_func(move || drop(fd)) }
        .map_err(|e| format!("this computer's graphics driver could not show the guest's picture: {}", e.message()))
}
