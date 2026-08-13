# Changelog

Notable user-visible changes to JWM are recorded here. Components in this
monorepo use independent Semantic Versions.

## [Unreleased]

### Added

- Tag-driven release automation with quality gates, an installable bundle, a
  Git source archive, SHA-256 checksums, and artifact provenance.
- Tested versioned install, upgrade, rollback, and uninstall operations.
- Compatibility, upgrade, and release-process documentation.

### Changed

- No stable release has been published. The root `0.2.0` manifest version
  remains a development version, not a support commitment.
- CI now treats Clippy correctness, suspicious, and performance diagnostics as
  errors and explicitly tests the Linux action and D-Bus provider adapters.

### Fixed

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
