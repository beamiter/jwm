# Packaging

JWM's first-class install path remains the in-tree helper and the release
bundle from `scripts/package-release.sh`. Distro recipes below are starting
points so external packagers can pick the project up after the first
`jwm-v*` tag.

## Official artifacts

| Artifact | Source |
| --- | --- |
| Binary + session bundle | `scripts/package-release.sh` → GitHub Release on `jwm-v*` |
| Source archive | `git archive` from the same tag |
| Checksums / provenance | `SHA256SUMS` + `actions/attest` |

Prefer installing the Wayland session (`jwm-wayland.desktop`) and the official
bar `tao_glow_bar`. See [hardware-validation](../docs/hardware-validation.md)
before calling a package "stable".

## Arch Linux (AUR sketch)

```pkgbuild
# Maintainer: ...
pkgname=jwm-git
pkgver=0.2.0.r0.gdeadbee
pkgrel=1
pkgdesc="Tag-based WM/compositor with built-in shell (Wayland DRM primary)"
arch=('x86_64')
url="https://github.com/<org>/jwm"
license=('MIT')
depends=('libx11' 'libxkbcommon' 'wayland' 'mesa' 'libinput' 'seatd' 'alsa-lib' 'dbus')
makedepends=('cargo' 'git' 'clang' 'pkgconf')
provides=('jwm')
conflicts=('jwm')
source=("git+$url.git")
sha256sums=('SKIP')

pkgver() {
  cd jwm
  git describe --long --tags | sed 's/^jwm-v//;s/-/.r/;s/-/./'
}

build() {
  cd jwm
  cargo build --locked --release --bins
}

package() {
  cd jwm
  install -Dm755 target/release/jwm target/release/jwm-tool \
    target/release/jwm-support target/release/jwm-remote -t "$pkgdir/usr/bin"
  install -Dm644 jwm-wayland.desktop \
    "$pkgdir/usr/share/wayland-sessions/jwm.desktop"
  install -Dm644 jwm-x11rb.desktop jwm-xcb.desktop -t "$pkgdir/usr/share/xsessions"
  install -Dm644 LICENSE -t "$pkgdir/usr/share/licenses/$pkgname"
}
```

Publish only after a tagged release and hardware sign-off. Adjust the `url`
and dependency list to match the packaging distribution.

## Nix flake sketch

```nix
{
  description = "JWM — Wayland-first tag WM with built-in shell";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
    in {
      packages.${system}.default = pkgs.rustPlatform.buildRustPackage {
        pname = "jwm";
        version = "0.2.0";
        src = self;
        cargoLock.lockFile = ./Cargo.lock;
        buildInputs = with pkgs; [
          xorg.libX11 libxkbcommon wayland mesa libinput seatd alsa-lib dbus
        ];
        nativeBuildInputs = with pkgs; [ pkg-config clang ];
      };
    };
}
```

Wire session desktop files through a NixOS module once the package builds on
hydra-style CI.

## Demo

After hardware validation, replace nested/CI footage in `video-demo/` with a
short **real DRM/KMS** capture that shows: cold start, Shell Hub audio switch,
screenshot to clipboard, and a cube/expose transition — then link it from the
README.
