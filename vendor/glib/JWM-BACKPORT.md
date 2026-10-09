# JWM compatibility backport for glib 0.18.5

Original package: https://static.crates.io/crates/glib/glib-0.18.5.crate
SHA-256: 233daaf6e83ae6a12a52055f568f9d7cf4671dabb78ff9560ab6da230ce00ee5
Original published VCS commit: 42b9caf98e03ded086362d9653ca58fe94dc8658 (glib subdirectory).
Version remains 0.18.5. LICENSE and COPYRIGHT are preserved verbatim.

The only changes to upstream Rust source are the two lines in src/variant_iter.rs that
backport upstream commit b5a4071e439bef2b5eea76c3aa25e5ae84839e34:
https://github.com/gtk-rs/gtk-rs-core/pull/1343
https://github.com/gtk-rs/gtk-rs-core/commit/b5a4071e439bef2b5eea76c3aa25e5ae84839e34
The local pointer is mutable and passed as &mut p to C's variadic out parameter.
Advisory: https://rustsec.org/advisories/RUSTSEC-2024-0429.html

GTK3 dependencies require glib ^0.18. A direct 0.20 override is incompatible.
Standalone bar Cargo roots each need the path patch; root workspace patches
do not propagate into excluded standalone packages. Cargo-audit may continue
to match version 0.18.5. Any exception must identify this source checksum and
backport, never claim that all uses of unmodified 0.18.5 are safe.

The separately locked regression crate is tests/glib_backport. Run its
synthetic-string iterator test with optimizations; debug-only testing is not
sufficient for this aliasing defect:

    scripts/test.sh --manifest-path tests/glib_backport/Cargo.toml --release

This does not start a GUI or prove VariantStrIter is reached by a particular
bar. Standalone GUI package checks remain separate validation gates.
Remove this patch when the relevant GUI framework supports an actually fixed
upstream release without a GTK3 API mismatch.
