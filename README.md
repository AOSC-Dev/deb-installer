deb-installer
===

A `.deb` package installation wizard.

Dependencies
---

- Rust (`rustc`) that is reasonably new
- Qt 5
- OpenSSL
- libapt-pkg

Building
---

Simply build with `cargo`:

```bash
cargo build --release
```

To build a copy for local debug/testing (such as to test Gettext
localisation):

```bash
cargo build --release --features debug
```

Installation
---

```debug
# Install the main executable.
install -Dvm755 ./target/release/deb-installer \
    /usr/local/bin/deb-installer

# Install the D-Bus service file.
install -Dvm644 ./io.aosc.deb_installer.conf \
    /usr/share/dbus-1/system.d/io.aosc.deb_installer.conf

# Install the icon and .desktop entry.
install -Dvm644 ./data/io.aosc.deb_installer.{png,svg} \
    -t /usr/share/pixmaps/
install -Dvm644 ./data/io.aosc.deb_installer.desktop \
    /usr/share/applications/io.aosc.deb_installer.desktop
```
