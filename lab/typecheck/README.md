Stub pkg-config files for type-checking `crates/gui` on a machine without GTK 4 or libadwaita
development files. They let `cargo check` and `cargo clippy` run; nothing can be linked or run with them.

    PKG_CONFIG_PATH=$PWD/lab/typecheck cargo clippy -p roomy-gui --all-targets

On a machine with the real libraries, do not set this; build normally.
