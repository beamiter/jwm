# Original standalone lossless preparation evidence

These unchanged JSON files preserve the 2026-11-10 debug CPU-only baseline.
`report.json` contains all 60 raw rows, including warmups. `metadata.json`
records the original revision, dirty status, source hashes, command and machine.

The JPEG arm uses the production tile codec at that revision. The PNG arm is a
separate full-image comparison, **not** the later integrated PNG atlas path.
Different timing boundaries preclude an end-to-end speedup interpretation.
No socket, GL, GPU, physical display or real application was measured.
The glyph/line images are hand-constructed synthetic stimuli.

See [the preparation document](../../remote-lossless-static-preparation.md)
for metrics, interpretation and integration constraints.

The local base revision recorded in the original metadata has the same committed
tree (`8cbab3880aabd38ff394acbb86aa4f2cd5324b67`) as published commit
`65ba003d5f14cb65001118a491b37fe4f788d0d6`. The listed dirty changes were the
independent test module and its test-only include, not the later PNG integration.
The original metadata is preserved rather than rewritten to imply a different run.
