Stub pkg-config files for type-checking `crates/gui` on a machine without GTK 4 or libadwaita
development files. They let `cargo check` and `cargo clippy` run; nothing can be linked or run with them.

    just check-headless

There is one file for each library the `-sys` crates probe that such a machine usually lacks: `gtk4`,
`libadwaita-1` and `graphene-gobject-1.0`. The GTK and libadwaita stubs carry the oldest versions roomy
supports (4.14 and 1.5). GLib, Pango, Cairo and gdk-pixbuf still come from the system.

On a machine with the real libraries, do not set this; build normally.
