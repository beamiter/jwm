# Changelog

Notable user-visible changes to JWM are recorded here. Components in this
monorepo use independent Semantic Versions.

## [Unreleased]

### Added

- Status bars show the focused window's desktop icon beside its title. JWM
  publishes the window's application identity in shared-memory protocol v14 and
  `xbar_core` resolves it through the freedesktop desktop-entry and icon-theme
  lookup; `visibility.client_icon` and `ModelConfig::resolve_client_icons` turn
  it off.
- The bar's layout menu offers every layout the running window manager has
  rather than a fixed three. Protocol v14 carries the layout count and the
  layout in use, so the menu also marks the active entry, drops entries a
  compositor cannot enter, and keeps a newer compositor's extra layouts
  reachable.
- Tag-driven release automation with quality gates, an installable bundle, a
  Git source archive, SHA-256 checksums, and artifact provenance.
- Tested versioned install, upgrade, rollback, and uninstall operations.
- Compatibility, upgrade, and release-process documentation.

### Changed

- The shared-memory protocol is v14. JWM and every bar must be rebuilt and
  restarted together, which the existing layout/version validation enforces.
- `display::CANONICAL_LAYOUTS` is now the single source for JWM's layout ids,
  names, symbols, labels and cycle order; `LayoutEnum` derives from it.
- No stable release has been published. The root `0.2.0` manifest version
  remains a development version, not a support commitment.
- CI now treats Clippy correctness, suspicious, and performance diagnostics as
  errors and explicitly tests the Linux action and D-Bus provider adapters.

### Fixed

- Bar tag glyphs no longer depend on which font fontconfig happens to hand a
  private-use code point: an installed Nerd Font is named explicitly in the
  font description (configurable as `presentation.icon_font`). The gear and
  home tags previously resolved to Arial, which draws unrelated shapes there.
- The default Tao/pixels bar now activates a control only after a matching
  press and release on the same node, and follows JWM's authoritative bar
  height instead of leaving a four-pixel layout gap.
- Installed payload ownership and modes are normalized instead of preserving
  untrusted extraction metadata; path traversal, symlink, and special-file
  payloads remain rejected.
- Production X11 session entries no longer force the optional WaterLily layer
  onto shared `/tmp` test endpoints.

## Versioning note

The root `jwm`, `jwm-bridge`, `jwm-portal`, `shared_structures`, `xbar_core`,
provider crates, and each bar are separate SemVer components. A JWM bundle
records the exact set it contains; its tag does not replace component versions.

[Unreleased]: https://github.com/beamiter/jwm/commits/master
