//! The "Download a System" window: the systems roomy can fetch, one family (Linux, BSD) at a
//! time, each with its editions.
//!
//! Downloads belong to [`Shared`], which lives as long as the program, so closing the window does
//! not stop them; opening it again shows how they are going.

use crate::Context;
use adw::prelude::*;
use gtk::{gdk, gio, glib};
use roomy_core::catalog::{Family, GuestSystem};
use roomy_core::env;
use roomy_core::images::download::{self, Download, Progress};
use roomy_core::images::{self, Curl, DISTROS, Distro, Edition, Group, Image, Remembering, size_words};
use serde_json::json;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// What is known about one edition's current image.
#[derive(Clone)]
enum Lookup {
    Looking,
    Found(Image),
    Failed(String),
}

/// An edition: its distribution's id and its place in the distribution's list.
type Key = (&'static str, usize);

/// What every download window shares, and what outlives them.
#[derive(Default)]
pub struct Shared {
    lookups: RefCell<HashMap<Key, Lookup>>,
    downloads: RefCell<HashMap<Key, Download>>,
    fetch: RefCell<Option<Arc<Remembering<Curl>>>>,
    /// Each distribution's screenshot, once it is on disk; a distribution without one never appears.
    screenshots: RefCell<HashMap<&'static str, PathBuf>>,
}

/// Fetches every distribution's screenshot in the background, so that they are ready by the
/// time the download window opens. Cached files younger than a month are used as they are.
pub fn fetch_screenshots(context: &Rc<Context>) {
    let Ok(fetch) = context.downloads.fetch(context) else { return };
    for distro in DISTROS {
        let (context, fetch) = (context.clone(), fetch.clone());
        glib::spawn_future_local(async move {
            if let Ok(Ok(file)) = gio::spawn_blocking(move || images::screenshot(fetch.inner(), distro.id)).await {
                context.downloads.screenshots.borrow_mut().insert(distro.id, file);
            }
        });
    }
}

impl Shared {
    /// What reads the projects' lists; one for the whole program, so each list is read once.
    fn fetch(&self, context: &Context) -> roomy_core::Result<Arc<Remembering<Curl>>> {
        if let Some(fetch) = self.fetch.borrow().as_ref() {
            return Ok(fetch.clone());
        }
        let fetch = Arc::new(Remembering::new(Curl::new(&context.env)?));
        self.fetch.replace(Some(fetch.clone()));
        Ok(fetch)
    }

    fn progress(&self, key: Key) -> Option<Progress> {
        self.downloads.borrow().get(&key).map(Download::progress)
    }
}

const LOGOS: &[(&str, &[u8])] = &[
    ("almalinux", include_bytes!("../data/logos/almalinux.png")),
    ("alpinelinux", include_bytes!("../data/logos/alpinelinux.png")),
    ("archlinux", include_bytes!("../data/logos/archlinux.png")),
    ("bazzite", include_bytes!("../data/logos/bazzite.png")),
    ("cachyos", include_bytes!("../data/logos/cachyos.png")),
    ("debian", include_bytes!("../data/logos/debian.png")),
    ("elementary", include_bytes!("../data/logos/elementary.png")),
    ("endeavouros", include_bytes!("../data/logos/endeavouros.png")),
    ("fedora", include_bytes!("../data/logos/fedora.png")),
    ("freebsd", include_bytes!("../data/logos/freebsd.png")),
    ("garudalinux", include_bytes!("../data/logos/garudalinux.png")),
    ("gentoo", include_bytes!("../data/logos/gentoo.png")),
    ("kalilinux", include_bytes!("../data/logos/kalilinux.png")),
    ("kdeneon", include_bytes!("../data/logos/kdeneon.png")),
    ("linuxmint", include_bytes!("../data/logos/linuxmint.png")),
    ("manjaro", include_bytes!("../data/logos/manjaro.png")),
    ("mxlinux", include_bytes!("../data/logos/mxlinux.png")),
    ("nixos", include_bytes!("../data/logos/nixos.png")),
    ("opensuse", include_bytes!("../data/logos/opensuse.png")),
    ("popos", include_bytes!("../data/logos/popos.png")),
    ("rockylinux", include_bytes!("../data/logos/rockylinux.png")),
    ("solus", include_bytes!("../data/logos/solus.png")),
    ("ubuntu", include_bytes!("../data/logos/ubuntu.png")),
    ("vanillaos", include_bytes!("../data/logos/vanillaos.png")),
    ("voidlinux", include_bytes!("../data/logos/voidlinux.png")),
    ("zorin", include_bytes!("../data/logos/zorin.png")),
];

/// Logos drawn in black, which would vanish on a dark background: their black and white parts swapped.
const DARK_LOGOS: &[(&str, &[u8])] = &[
    ("almalinux", include_bytes!("../data/logos/almalinux-dark.png")),
    ("mxlinux", include_bytes!("../data/logos/mxlinux-dark.png")),
    ("vanillaos", include_bytes!("../data/logos/vanillaos-dark.png")),
];

fn logo_texture(id: &str, dark: bool) -> Option<gdk::Texture> {
    let find = |table: &[(&str, &'static [u8])]| table.iter().find(|(name, _)| *name == id).map(|(_, bytes)| *bytes);
    let bytes = dark.then(|| find(DARK_LOGOS)).flatten().or_else(|| find(LOGOS))?;
    gdk::Texture::from_bytes(&glib::Bytes::from_static(bytes)).ok()
}

/// A distribution's logo at `size` pixels, following the light or dark style; a generic icon for one without.
fn logo(id: &'static str, size: i32) -> gtk::Image {
    let style = adw::StyleManager::default();
    let image = match logo_texture(id, style.is_dark()) {
        Some(texture) => gtk::Image::from_paintable(Some(&texture)),
        None => gtk::Image::from_icon_name("media-optical-symbolic"),
    };
    image.set_pixel_size(size);
    if DARK_LOGOS.iter().any(|(name, _)| *name == id) {
        let weak = image.downgrade();
        let handler = style.connect_dark_notify(move |style| {
            if let Some(image) = weak.upgrade() {
                image.set_paintable(logo_texture(id, style.is_dark()).as_ref());
            }
        });
        let handler = RefCell::new(Some(handler));
        image.connect_destroy(move |_| {
            if let Some(handler) = handler.take() {
                adw::StyleManager::default().disconnect(handler);
            }
        });
    }
    image
}

/// A path for showing: the home folder written as `~`.
fn tilde(path: &Path) -> String {
    match std::env::var_os("HOME").map(PathBuf::from).and_then(|home| path.strip_prefix(home).ok().map(Path::to_owned))
    {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

fn dim(text: &str) -> gtk::Label {
    let label = gtk::Label::builder().label(text).xalign(0.0).wrap(true).build();
    label.add_css_class("dim-label");
    label
}

/// The parts of a row that change as an edition is looked up and downloaded.
struct EditionRow {
    index: usize,
    row: adw::ActionRow,
    size: gtk::Label,
    spinner: gtk::Spinner,
    progress: gtk::ProgressBar,
    download: gtk::Button,
    cancel: gtk::Button,
    show_file: gtk::Button,
    use_it: gtk::Button,
    website: gtk::Button,
}

/// What the right-hand side shows for the chosen distribution.
struct Shown {
    distro: &'static Distro,
    rows: Vec<EditionRow>,
    version: gtk::Label,
    picture: gtk::Picture,
    frame: gtk::AspectFrame,
}

struct SidebarRow {
    row: gtk::ListBoxRow,
    distro: &'static Distro,
    status: gtk::Label,
    /// The edition a search found, when the name alone did not match.
    found: gtk::Label,
}

pub struct Catalog {
    context: Rc<Context>,
    window: adw::Window,
    toasts: adw::ToastOverlay,
    split: adw::NavigationSplitView,
    list: gtk::ListBox,
    family: Cell<Family>,
    /// The button that lists each family; none in a window that is for one family only.
    families: Vec<(Family, gtk::ToggleButton)>,
    sidebar: Vec<SidebarRow>,
    content: adw::NavigationPage,
    scroller: gtk::ScrolledWindow,
    shown: RefCell<Option<Shown>>,
    query: RefCell<String>,
    /// Where images are saved, as the desktop names it.
    downloads_dir: Option<PathBuf>,
    use_label: &'static str,
    on_use: Box<dyn Fn(PathBuf, GuestSystem)>,
}

fn family_switch(shown: Family) -> (gtk::Box, Vec<(Family, gtk::ToggleButton)>) {
    let families: Vec<(Family, gtk::ToggleButton)> = Family::ALL
        .into_iter()
        .map(|each| (each, gtk::ToggleButton::builder().label(each.label()).active(each == shown).build()))
        .collect();
    let switch = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    switch.add_css_class("linked");
    for (_, button) in &families {
        if !std::ptr::eq(button, &families[0].1) {
            button.set_group(Some(&families[0].1));
        }
        switch.append(button);
    }
    (switch, families)
}

/// Opens the window. It lists the systems of the family `only`, or without one those of every
/// family, a switch choosing which. A downloaded image's last button is labelled `use_label`;
/// pressing it closes the window and calls `on_use` with the image and the guest system it holds.
pub fn show(
    context: &Rc<Context>,
    parent: Option<&gtk::Window>,
    only: Option<Family>,
    use_label: &'static str,
    on_use: impl Fn(PathBuf, GuestSystem) + 'static,
) -> Rc<Catalog> {
    let window = adw::Window::builder().title("Download a System").default_width(1040).default_height(760).build();
    window.set_transient_for(parent);

    let search = gtk::SearchEntry::builder().placeholder_text("Search systems and editions").build();
    search.set_margin_start(12);
    search.set_margin_end(12);
    search.set_margin_bottom(6);
    let list = gtk::ListBox::new();
    list.add_css_class("navigation-sidebar");
    let sidebar: Vec<SidebarRow> = DISTROS.iter().map(sidebar_row).collect();
    for entry in &sidebar {
        list.append(&entry.row);
    }
    let sidebar_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_box.append(&search);
    sidebar_box.append(&gtk::ScrolledWindow::builder().vexpand(true).child(&list).build());
    let family = only.unwrap_or(Family::ALL[0]);
    let sidebar_header = adw::HeaderBar::new();
    let families = match only {
        Some(family) => {
            let title = adw::WindowTitle::new(&format!("{} systems", family.label()), "");
            sidebar_header.set_title_widget(Some(&title));
            Vec::new()
        }
        None => {
            let (switch, families) = family_switch(family);
            sidebar_header.set_title_widget(Some(&switch));
            families
        }
    };
    let sidebar_view = adw::ToolbarView::new();
    sidebar_view.add_top_bar(&sidebar_header);
    sidebar_view.set_content(Some(&sidebar_box));

    let scroller = gtk::ScrolledWindow::builder().vexpand(true).build();
    let content_view = adw::ToolbarView::new();
    content_view.add_top_bar(&adw::HeaderBar::new());
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&scroller));
    content_view.set_content(Some(&toasts));
    let content = adw::NavigationPage::new(&content_view, "");

    let split = adw::NavigationSplitView::new();
    split.set_sidebar(Some(&adw::NavigationPage::new(&sidebar_view, "Systems")));
    split.set_content(Some(&content));
    split.set_min_sidebar_width(260.0);
    window.set_content(Some(&split));
    let narrow = adw::Breakpoint::new(adw::BreakpointCondition::parse("max-width: 700sp").expect("a valid condition"));
    narrow.add_setter(&split, "collapsed", Some(&true.to_value()));
    window.add_breakpoint(narrow);

    let catalog = Rc::new(Catalog {
        context: context.clone(),
        window,
        toasts,
        split,
        list,
        family: Cell::new(family),
        families,
        sidebar,
        content,
        scroller,
        shown: RefCell::new(None),
        query: RefCell::new(String::new()),
        downloads_dir: env::downloads_dir().ok(),
        use_label,
        on_use: Box::new(on_use),
    });

    catalog.list.set_header_func(|row, before| {
        let group = |row: &gtk::ListBoxRow| DISTROS.get(row.index() as usize).map(|distro| distro.group);
        let starts_group = before.is_none_or(|before| group(before) != group(row));
        row.set_header(starts_group.then(|| group_header(group(row).unwrap_or(Group::Special))).as_ref());
    });
    catalog.list.set_filter_func({
        let weak = Rc::downgrade(&catalog);
        move |row| weak.upgrade().is_none_or(|this| this.matches(row.index() as usize))
    });
    catalog.list.connect_row_activated(on!(&catalog, |this, _, row| {
        if let Some(entry) = this.sidebar.get(row.index() as usize) {
            this.select(entry.distro.id);
            this.split.set_show_content(true);
        }
    }));
    search.connect_search_changed(on!(&catalog, |this, search| {
        this.query.replace(search.text().trim().to_lowercase());
        this.list.invalidate_filter();
        this.list.invalidate_headers();
        this.refresh_sidebar();
    }));
    for (family, button) in &catalog.families {
        let family = *family;
        button.connect_toggled(on!(&catalog, |this, button| {
            // Also called when `select` presses the button in, with the family already shown.
            if button.is_active() && this.family.get() != family {
                this.select_first_of(family);
            }
        }));
    }

    // Each window keeps itself until it is closed; everything else holds it weakly.
    let keep = RefCell::new(Some(catalog.clone()));
    catalog.window.connect_close_request(move |_| {
        keep.take();
        glib::Propagation::Proceed
    });
    glib::timeout_add_local(Duration::from_millis(500), {
        let weak = Rc::downgrade(&catalog);
        move || match weak.upgrade() {
            Some(this) => {
                this.refresh();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        }
    });

    catalog.select_first_of(family);
    catalog.window.present();
    catalog
}

fn sidebar_row(distro: &'static Distro) -> SidebarRow {
    let name = gtk::Label::builder().label(distro.name).xalign(0.0).build();
    let found = dim("");
    found.add_css_class("caption");
    found.set_visible(false);
    let words = gtk::Box::new(gtk::Orientation::Vertical, 0);
    words.set_hexpand(true);
    words.set_valign(gtk::Align::Center);
    words.append(&name);
    words.append(&found);
    let status = dim("");
    status.add_css_class("caption");
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    line.set_margin_top(4);
    line.set_margin_bottom(4);
    line.append(&logo(distro.id, 28));
    line.append(&words);
    line.append(&status);
    let row = gtk::ListBoxRow::builder().child(&line).build();
    SidebarRow { row, distro, status, found }
}

fn group_header(group: Group) -> gtk::Label {
    let label = gtk::Label::builder().label(group.title()).xalign(0.0).build();
    label.add_css_class("heading");
    label.add_css_class("dim-label");
    label.set_margin_top(14);
    label.set_margin_bottom(4);
    label.set_margin_start(12);
    label
}

impl Catalog {
    pub fn window(&self) -> &adw::Window {
        &self.window
    }

    fn tell(&self, message: &str) {
        crate::toast(&self.toasts, message, 8);
    }

    /// Whether the system at `index` is listed: one of the family shown that fits the search, by
    /// its own words or an edition's.
    fn matches(&self, index: usize) -> bool {
        let query = self.query.borrow();
        let Some(distro) = DISTROS.get(index) else { return false };
        distro.family() == self.family.get()
            && (query.is_empty() || words_fit(distro, &query) || found_edition(distro, &query).is_some())
    }

    fn select_first_of(self: &Rc<Self>, family: Family) {
        if let Some(first) = DISTROS.iter().find(|distro| distro.family() == family) {
            self.select(first.id);
        }
    }

    /// Shows a system on the right, with its family's systems listed on the left, and starts
    /// finding its editions' current images. Not a system of a family this window leaves out.
    pub fn select(self: &Rc<Self>, id: &str) {
        let Some((index, distro)) = DISTROS.iter().enumerate().find(|(_, distro)| distro.id == id) else { return };
        let family = distro.family();
        if family != self.family.get() {
            let Some((_, button)) = self.families.iter().find(|(each, _)| *each == family) else { return };
            self.family.set(family);
            self.list.invalidate_filter();
            self.list.invalidate_headers();
            button.set_active(true);
        }
        if let Some(row) = self.list.row_at_index(index as i32) {
            self.list.select_row(Some(&row));
        }
        let (page, shown) = self.distro_page(distro);
        self.content.set_title(distro.name);
        self.scroller.set_child(Some(&page));
        self.shown.replace(Some(shown));
        self.look_up(distro);
        self.refresh();
    }

    fn distro_page(self: &Rc<Self>, distro: &'static Distro) -> (gtk::Widget, Shown) {
        let column = gtk::Box::new(gtk::Orientation::Vertical, 22);
        column.set_margin_top(12);
        column.set_margin_bottom(32);
        column.set_margin_start(24);
        column.set_margin_end(24);

        let title = gtk::Label::builder().label(distro.name).xalign(0.0).wrap(true).build();
        title.add_css_class("title-1");
        let words = gtk::Box::new(gtk::Orientation::Vertical, 4);
        words.set_hexpand(true);
        words.set_valign(gtk::Align::Center);
        words.append(&title);
        words.append(&dim(distro.tagline));
        let website = gtk::Button::builder().label("Website").valign(gtk::Align::Center).build();
        website.connect_clicked(on!(self, |this, _| this.open_web(distro.site)));
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        head.append(&logo(distro.id, 64));
        head.append(&words);
        head.append(&website);
        column.append(&head);

        // Hidden until the screenshot is there; a distribution without one shows none.
        let picture = gtk::Picture::builder().content_fit(gtk::ContentFit::Cover).build();
        let frame = gtk::AspectFrame::new(0.5, 0.5, 16.0 / 9.0, false);
        frame.set_child(Some(&picture));
        frame.add_css_class("card");
        frame.set_overflow(gtk::Overflow::Hidden);
        frame.set_visible(false);
        column.append(&frame);

        let about = gtk::Label::builder().label(distro.about).xalign(0.0).wrap(true).build();
        column.append(&about);

        let version = gtk::Label::builder().label("…").xalign(0.0).build();
        let downloads_dir = self.downloads_dir.as_deref().map(tilde).unwrap_or_else(|| "Downloads".into());
        let facts = gtk::Grid::builder().column_spacing(24).row_spacing(2).column_homogeneous(true).build();
        for (column_index, (name, value)) in [
            ("Version", version.clone()),
            ("Desktop", gtk::Label::builder().label(distro.desktop).xalign(0.0).wrap(true).build()),
            ("Saved to", gtk::Label::builder().label(&downloads_dir).xalign(0.0).wrap(true).build()),
        ]
        .into_iter()
        .enumerate()
        {
            let name = dim(name);
            name.add_css_class("caption");
            facts.attach(&name, column_index as i32, 0, 1, 1);
            facts.attach(&value, column_index as i32, 1, 1, 1);
        }
        column.append(&facts);

        let mut rows = Vec::new();
        let mut group: Option<adw::PreferencesGroup> = None;
        for (index, edition) in distro.editions.iter().enumerate() {
            if index == 0 || distro.editions[index - 1].section != edition.section {
                let new = adw::PreferencesGroup::builder().title(edition.section).build();
                if let Some(hint) = section_hint(edition.section) {
                    new.set_description(Some(hint));
                }
                column.append(&new);
                group = Some(new);
            }
            let row = self.edition_row(distro, index, edition);
            if let Some(group) = &group {
                group.add(&row.row);
            }
            rows.push(row);
        }

        let clamp = adw::Clamp::builder().maximum_size(820).child(&column).build();
        (clamp.upcast(), Shown { distro, rows, version, picture, frame })
    }

    fn edition_row(self: &Rc<Self>, distro: &'static Distro, index: usize, edition: &'static Edition) -> EditionRow {
        let row = crate::plain_row(edition.name, "");
        row.set_subtitle_lines(2);
        let size = dim("");
        size.set_wrap(false);
        size.set_xalign(1.0);
        let spinner = gtk::Spinner::builder().spinning(true).build();
        let progress = gtk::ProgressBar::builder().valign(gtk::Align::Center).width_request(150).build();
        let button = |label: &str| gtk::Button::builder().label(label).valign(gtk::Align::Center).build();
        let download = button("Download");
        download.add_css_class("suggested-action");
        let cancel = button("Cancel");
        let show_file = button("Show in Files");
        let use_it = button(self.use_label);
        use_it.add_css_class("suggested-action");
        let website = button("Open Download Page");
        for widget in [
            size.upcast_ref::<gtk::Widget>(),
            spinner.upcast_ref(),
            progress.upcast_ref(),
            download.upcast_ref(),
            cancel.upcast_ref(),
            show_file.upcast_ref(),
            use_it.upcast_ref(),
            website.upcast_ref(),
        ] {
            widget.set_visible(false);
            row.add_suffix(widget);
        }
        let key = (distro.id, index);
        download.connect_clicked(on!(self, |this, _| this.download(key)));
        cancel.connect_clicked(on!(self, |this, _| {
            if let Some(download) = this.context.downloads.downloads.borrow().get(&key) {
                download.stop();
            }
        }));
        show_file.connect_clicked(on!(self, |this, _| {
            if let Some(path) = this.saved_file(key) {
                gtk::FileLauncher::new(Some(&gio::File::for_path(path))).open_containing_folder(
                    Some(&this.window),
                    gio::Cancellable::NONE,
                    |_| {},
                );
            }
        }));
        use_it.connect_clicked(on!(self, |this, _| {
            if let Some(path) = this.saved_file(key) {
                (this.on_use)(path, edition.guest);
                this.window.close();
            }
        }));
        website.connect_clicked(on!(self, |this, _| this.open_web(edition.website_only().unwrap_or(distro.site))));
        EditionRow { index, row, size, spinner, progress, download, cancel, show_file, use_it, website }
    }

    fn open_web(&self, uri: &str) {
        gtk::UriLauncher::new(uri).launch(Some(&self.window), gio::Cancellable::NONE, |_| {});
    }

    /// Finds the current image of each of the distribution's editions, unless that is known already.
    fn look_up(self: &Rc<Self>, distro: &'static Distro) {
        let shared = &self.context.downloads;
        let pending: Vec<usize> = (0..distro.editions.len())
            .filter(|&index| distro.editions[index].website_only().is_none())
            .filter(|&index| {
                !matches!(shared.lookups.borrow().get(&(distro.id, index)), Some(Lookup::Found(_) | Lookup::Looking))
            })
            .collect();
        if pending.is_empty() {
            return;
        }
        let fetch = match shared.fetch(&self.context) {
            Ok(fetch) => fetch,
            Err(e) => {
                for &index in &pending {
                    shared.lookups.borrow_mut().insert((distro.id, index), Lookup::Failed(e.to_string()));
                }
                return;
            }
        };
        for &index in &pending {
            shared.lookups.borrow_mut().insert((distro.id, index), Lookup::Looking);
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let found = gio::spawn_blocking(move || {
                images::resolve_all(&*fetch, distro.editions)
                    .into_iter()
                    .map(|(_, found)| found.map_err(|e| e.to_string()))
                    .collect::<Vec<_>>()
            })
            .await
            .unwrap_or_default();
            let Some(this) = weak.upgrade() else { return };
            let mut lookups = this.context.downloads.lookups.borrow_mut();
            for index in pending {
                let lookup = match found.get(index) {
                    Some(Ok(image)) => Lookup::Found(image.clone()),
                    Some(Err(e)) => Lookup::Failed(e.clone()),
                    None => Lookup::Failed("the lookup failed".into()),
                };
                lookups.insert((distro.id, index), lookup);
            }
            drop(lookups);
            this.refresh();
        });
    }

    fn download(self: &Rc<Self>, key: Key) {
        let Some(Lookup::Found(image)) = self.context.downloads.lookups.borrow().get(&key).cloned() else { return };
        let started = env::downloads_dir().and_then(|dir| Download::start(&self.context.env, &image, &dir));
        match started {
            Ok(download) => {
                self.context.downloads.downloads.borrow_mut().insert(key, download);
                self.refresh();
            }
            Err(e) => self.tell(&e.to_string()),
        }
    }

    /// The edition's image in the Downloads folder, if it is there.
    fn saved_file(&self, key: Key) -> Option<PathBuf> {
        if let Some(Progress::Done(path)) = self.context.downloads.progress(key) {
            return Some(path);
        }
        let Some(Lookup::Found(image)) = self.context.downloads.lookups.borrow().get(&key).cloned() else {
            return None;
        };
        Some(download::target(self.downloads_dir.as_deref()?, &image)).filter(|path| path.is_file())
    }

    /// Brings every visible part up to date with the lookups and downloads.
    fn refresh(&self) {
        self.refresh_sidebar();
        let shown = self.shown.borrow();
        let Some(shown) = shown.as_ref() else { return };
        let shared = &self.context.downloads;
        let version = (0..shown.distro.editions.len()).find_map(|index| {
            match shared.lookups.borrow().get(&(shown.distro.id, index)) {
                Some(Lookup::Found(image)) => {
                    Some(if image.version.is_empty() { "Latest".to_owned() } else { image.version.clone() })
                }
                _ => None,
            }
        });
        let all_on_websites = shown.distro.editions.iter().all(|edition| edition.website_only().is_some());
        shown
            .version
            .set_label(&version.unwrap_or_else(|| if all_on_websites { "See the website".into() } else { "…".into() }));
        if shown.picture.paintable().is_none()
            && let Some(file) = shared.screenshots.borrow().get(shown.distro.id)
            && let Ok(texture) = gdk::Texture::from_filename(file)
        {
            shown.picture.set_paintable(Some(&texture));
            shown.frame.set_visible(true);
        }
        for row in &shown.rows {
            self.refresh_row(shown.distro, row);
        }
    }

    fn refresh_row(&self, distro: &'static Distro, row: &EditionRow) {
        let edition = &distro.editions[row.index];
        let key = (distro.id, row.index);
        let shared = &self.context.downloads;
        let mut visible: Vec<&gtk::Widget> = Vec::new();
        let lookup = shared.lookups.borrow().get(&key).cloned();
        let subtitle = if edition.website_only().is_some() {
            visible.push(row.website.upcast_ref());
            joined(edition.note, "Offered on the project’s website")
        } else if let Some(progress) = shared.progress(key).filter(|p| !matches!(p, Progress::Done(_))) {
            match progress {
                Progress::Fetching { done, total } => {
                    visible.extend([row.progress.upcast_ref::<gtk::Widget>(), row.cancel.upcast_ref()]);
                    match total.filter(|&total| total > 0) {
                        Some(total) => {
                            row.progress.set_fraction(done as f64 / total as f64);
                            format!("{} of {}", size_words(done), size_words(total))
                        }
                        None => {
                            row.progress.pulse();
                            format!("{} so far", size_words(done))
                        }
                    }
                }
                Progress::Checking => {
                    visible.push(row.spinner.upcast_ref());
                    "Checking that the download is the genuine image…".to_owned()
                }
                Progress::Stopped => {
                    row.download.set_label("Resume");
                    visible.push(row.download.upcast_ref());
                    "Stopped; it carries on where it left off".to_owned()
                }
                Progress::Failed(why) => {
                    row.download.set_label("Try Again");
                    visible.push(row.download.upcast_ref());
                    why
                }
                Progress::Done(_) => unreachable!("filtered out above"),
            }
        } else {
            match lookup {
                None | Some(Lookup::Looking) => {
                    visible.push(row.spinner.upcast_ref());
                    "Finding the current version…".to_owned()
                }
                Some(Lookup::Failed(why)) => {
                    visible.push(row.website.upcast_ref());
                    format!("Not available right now: {why}")
                }
                Some(Lookup::Found(image)) => {
                    if self.saved_file(key).is_some() {
                        visible.extend([row.show_file.upcast_ref::<gtk::Widget>(), row.use_it.upcast_ref()]);
                        format!("In your Downloads folder: {}", image.file_name)
                    } else {
                        row.size.set_label(&image.size.map(size_words).unwrap_or_default());
                        row.download.set_label("Download");
                        visible.extend([row.size.upcast_ref::<gtk::Widget>(), row.download.upcast_ref()]);
                        joined(edition.note, &image.version)
                    }
                }
            }
        };
        row.row.set_subtitle(&subtitle);
        for widget in [
            row.size.upcast_ref::<gtk::Widget>(),
            row.spinner.upcast_ref(),
            row.progress.upcast_ref(),
            row.download.upcast_ref(),
            row.cancel.upcast_ref(),
            row.show_file.upcast_ref(),
            row.use_it.upcast_ref(),
            row.website.upcast_ref(),
        ] {
            widget.set_visible(visible.contains(&widget));
        }
    }

    fn refresh_sidebar(&self) {
        let query = self.query.borrow().clone();
        let shared = &self.context.downloads;
        for entry in &self.sidebar {
            let distro = entry.distro;
            let progress: Vec<Progress> =
                (0..distro.editions.len()).filter_map(|index| shared.progress((distro.id, index))).collect();
            let status = if progress.iter().any(|p| !p.finished()) {
                "Downloading".to_owned()
            } else if progress.iter().any(|p| matches!(p, Progress::Done(_))) {
                "Downloaded".to_owned()
            } else if distro.editions.len() > 1 {
                format!("{} editions", distro.editions.len())
            } else {
                String::new()
            };
            entry.status.set_label(&status);
            let found =
                if query.is_empty() || words_fit(distro, &query) { None } else { found_edition(distro, &query) };
            entry.found.set_label(found.map(|edition| edition.name).unwrap_or_default());
            entry.found.set_visible(found.is_some());
        }
    }

    /// What roomy's own test runs read about this window.
    pub fn lab_facts(&self) -> serde_json::Value {
        let shown = self.shown.borrow();
        let Some(shown) = shown.as_ref() else { return json!(null) };
        let lookups = self.context.downloads.lookups.borrow();
        let editions: Vec<_> = (0..shown.distro.editions.len())
            .map(|index| match lookups.get(&(shown.distro.id, index)) {
                Some(Lookup::Found(image)) => json!({ "found": image.file_name }),
                Some(Lookup::Failed(why)) => json!({ "failed": why }),
                Some(Lookup::Looking) => json!("looking"),
                None => json!("website"),
            })
            .collect();
        json!({
            "family": self.family.get().label(),
            "selected": shown.distro.id,
            "editions": editions,
            "screenshot": shown.frame.is_visible(),
            "window_size": [self.window.width(), self.window.height()],
        })
    }
}

/// Whether the search matches the distribution's own words.
fn words_fit(distro: &Distro, query: &str) -> bool {
    [distro.id, distro.name, distro.tagline, distro.desktop].iter().any(|words| words.to_lowercase().contains(query))
}

/// The first edition whose name or note matches the search.
fn found_edition(distro: &'static Distro, query: &str) -> Option<&'static Edition> {
    distro
        .editions
        .iter()
        .find(|edition| edition.name.to_lowercase().contains(query) || edition.note.to_lowercase().contains(query))
}

/// A few words on what a heading's editions have in common.
fn section_hint(section: &str) -> Option<&'static str> {
    Some(match section {
        "Official flavours" => "Ubuntu with another desktop",
        "Spins" => "Fedora with another desktop",
        "Atomic desktops" => "The system updates as a whole and can roll back",
        "Labs" => "Fedora with the software for one purpose",
        "Live desktops" => "Try it first, then install it from its desktop",
        "Installers" => "Install it straight away",
        _ => return None,
    })
}

/// Two descriptions side by side, either of which may be empty.
fn joined(first: &str, second: &str) -> String {
    match (first.is_empty(), second.is_empty()) {
        (false, false) => format!("{first} · {second}"),
        (true, _) => second.to_owned(),
        (false, true) => first.to_owned(),
    }
}
