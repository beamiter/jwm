# JWM - A Dynamic Window Manager Written in Rust

![JWM Logo](https://via.placeholder.com/150x150?text=JWM)

[![Build Status](https://img.shields.io/github/workflow/status/username/jwm/CI)](https://github.com/username/jwm/actions)
[![Crates.io](https://img.shields.io/crates/v/jwm.svg)](https://crates.io/crates/jwm)
[![License](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

JWM (Just Window Manager) is a dynamic window manager for X11, written in Rust. Inspired by the minimalist philosophy of [dwm](https://dwm.suckless.org/), JWM aims to provide a lightweight, efficient, and customizable window management experience while leveraging Rust's safety and performance benefits.

## 🌟 Features

- **Dynamic tiling window management**
- **Multiple workspaces/tags**
- **Status bar with customizable modules**
- **Keyboard-driven workflow**
- **Memory safe implementation in Rust**
- **Low resource footprint**
- **Multi-monitor support**
- **Customizable layouts:**
  - Tiling
  - Monocle
  - Floating
  - Custom layouts through configuration

## 🚀 Why Rust?

JWM is a complete rewrite of dwm in Rust, offering several advantages:

- **Memory safety** - Eliminates common C pitfalls and security vulnerabilities
- **Thread safety** - Safe concurrent programming model
- **Modern tooling** - Cargo ecosystem for dependencies and building
- **Maintainability** - More expressive type system and error handling
- **Performance** - Comparable speed to C with safer abstractions

## 📦 Installation

### Prerequisites

- Rust toolchain (1.60+)
- X11 development libraries
- Xlib headers

### From Source

```bash
# Clone the repository
git clone https://github.com/username/jwm.git
cd jwm

# Build and install
cargo build --release
sudo cp target/release/jwm /usr/local/bin/
```

### From Cargo

```bash
cargo install jwm
```

## ⚙️ Configuration

JWM is configured by editing `config.rs` and recompiling, similar to dwm's philosophy of simplicity through source code configuration:

```bash
# Edit your configuration
cd jwm
$EDITOR config.rs

# Recompile with your config
cargo build --release
```

### Example Configuration

```rust
// jwm/config.rs

pub static KEY_BINDINGS: &[KeyBinding] = &[
    KeyBinding { 
        modifier: MOD_KEY,
        key: xlib::XK_Return,
        action: Action::Spawn(TERMINAL)
    },
    KeyBinding {
        modifier: MOD_KEY,
        key: xlib::XK_q,
        action: Action::CloseWindow
    },
    // More key bindings...
];
```

## 🖥️ Usage

Add JWM to your `.xinitrc` or display manager configuration:

```bash
# For .xinitrc
exec jwm
```

### Default Keybindings

| Key Combination | Action |
|----------------|--------|
| Mod + Enter | Open terminal |
| Mod + b | Toggle status bar |
| Mod + j/k | Focus next/previous window |
| Mod + h/l | Decrease/increase master area |
| Mod + Tab | Toggle between layouts |
| Mod + [1-9] | Switch to tag [1-9] |
| Mod + Shift + [1-9] | Move window to tag [1-9] |
| Mod + Shift + q | Close window |
| Mod + Shift + f | Truely fullscreen |
| Mod + e | Run dmenu_run |
| Mod + r | Run dmenu_run |
| Mod + ,/. | Move to other monitor |
| Mod + Shift + ,/. | Send to other monitor 
...

## 🔧 Contributing

Contributions are welcome! Please feel free to submit a Pull Request.

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/amazing-feature`)
3. Commit your changes (`git commit -m 'Add some amazing feature'`)
4. Push to the branch (`git push origin feature/amazing-feature`)
5. Open a Pull Request

## 📄 License

This project is licensed under the MIT License - see the [LICENSE](LICENSE) file for details.

## 🙏 Acknowledgments

- [dwm](https://dwm.suckless.org/) - The original inspiration for this project
- [Rust X11 crate](https://github.com/erlepereira/x11-rs) - X11 bindings for Rust
- All contributors and the Rust community

---

*JWM: Minimalist window management, maximum productivity*
