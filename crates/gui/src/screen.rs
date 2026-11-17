//! The widget that shows a guest's screen.
//!
//! It only draws a picture and reports its own size; what the picture is
//! and what happens with input is decided by the VM window.

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk};
use roomy_display::view::{self, Placement};
use std::cell::{Cell, RefCell};

/// Called with the widget's new width and height.
type ResizeCallback = Box<dyn Fn(i32, i32)>;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct Screen {
        pub texture: RefCell<Option<gdk::Texture>>,
        /// The picture's first row is its bottom (some graphics-memory screens).
        pub upside_down: Cell<bool>,
        pub on_resize: RefCell<Option<ResizeCallback>>,
        pub last_size: Cell<(i32, i32)>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Screen {
        const NAME: &'static str = "RoomyScreen";
        type Type = super::Screen;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Screen {}

    impl WidgetImpl for Screen {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let (width, height) = (widget.width() as f32, widget.height() as f32);
            snapshot.append_color(&gdk::RGBA::BLACK, &graphene::Rect::new(0.0, 0.0, width, height));
            let Some(texture) = self.texture.borrow().clone() else { return };
            let Some(place) = widget.placement() else { return };
            let area = graphene::Rect::new(place.x as f32, place.y as f32, place.width as f32, place.height as f32);
            if self.upside_down.get() {
                snapshot.save();
                snapshot.translate(&graphene::Point::new(0.0, area.y() * 2.0 + area.height()));
                snapshot.scale(1.0, -1.0);
                snapshot.append_texture(&texture, &area);
                snapshot.restore();
            } else {
                snapshot.append_texture(&texture, &area);
            }
        }

        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            // Any size is acceptable, and the preferred size never depends on the picture. The
            // guest is asked to match the window, so a window that grew to match the guest would
            // chase its own tail until it filled the monitor.
            let natural = if orientation == gtk::Orientation::Horizontal { 640 } else { 480 };
            (0, natural, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            if self.last_size.replace((width, height)) != (width, height)
                && let Some(on_resize) = self.on_resize.borrow().as_ref()
            {
                on_resize(width, height);
            }
        }
    }
}

glib::wrapper! {
    pub struct Screen(ObjectSubclass<imp::Screen>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Default for Screen {
    fn default() -> Self {
        Self::new()
    }
}

impl Screen {
    pub fn new() -> Screen {
        let screen: Screen = glib::Object::builder().build();
        screen.set_focusable(true);
        screen.set_hexpand(true);
        screen.set_vexpand(true);
        screen
    }

    /// Shows a new picture, or none.
    pub fn set_picture(&self, texture: Option<gdk::Texture>, upside_down: bool) {
        let imp = self.imp();
        imp.texture.replace(texture);
        imp.upside_down.set(upside_down);
        self.queue_draw();
    }

    pub fn picture(&self) -> Option<gdk::Texture> {
        self.imp().texture.borrow().clone()
    }

    /// A small copy of the picture, the right way up and no larger than the given size.
    /// `None` without a picture, or while the window is not on screen.
    pub fn small_copy(&self, max_width: f32, max_height: f32) -> Option<gdk::Texture> {
        let texture = self.picture()?;
        let (width, height) = (texture.width() as f32, texture.height() as f32);
        if width < 1.0 || height < 1.0 {
            return None;
        }
        let scale = (max_width / width).min(max_height / height).min(1.0);
        let area = graphene::Rect::new(0.0, 0.0, (width * scale).round().max(1.0), (height * scale).round().max(1.0));
        let snapshot = gtk::Snapshot::new();
        snapshot.append_color(&gdk::RGBA::BLACK, &area);
        if self.upside_down() {
            snapshot.translate(&graphene::Point::new(0.0, area.height()));
            snapshot.scale(1.0, -1.0);
        }
        snapshot.append_scaled_texture(&texture, gsk::ScalingFilter::Trilinear, &area);
        let node = snapshot.to_node()?;
        Some(self.native()?.renderer()?.render_texture(&node, Some(&area)))
    }

    /// Whether the picture is stored bottom row first and turned over when drawn.
    pub fn upside_down(&self) -> bool {
        self.imp().upside_down.get()
    }

    /// Size of the picture in guest pixels.
    pub fn picture_size(&self) -> Option<(u32, u32)> {
        self.imp().texture.borrow().as_ref().map(|t| (t.width() as u32, t.height() as u32))
    }

    fn placement(&self) -> Option<Placement> {
        let (width, height) = self.picture_size()?;
        view::fit(width, height, f64::from(self.width()), f64::from(self.height()))
    }

    /// The guest pixel under a point of this widget.
    pub fn guest_position(&self, x: f64, y: f64) -> Option<(u32, u32)> {
        let (width, height) = self.picture_size()?;
        Some(view::guest_position(self.placement()?, width, height, x, y))
    }

    /// Calls `f` with the new size (in window units) whenever this widget's size changes.
    pub fn connect_resized(&self, f: impl Fn(i32, i32) + 'static) {
        self.imp().on_resize.replace(Some(Box::new(f)));
    }
}
