# Performance baselines

Machine-readable reference recordings for the Phase 5 performance contract
(see `docs/performance.md`). One JSON file per system label, produced by
`jwm-tool perf record`, evaluated by `jwm-tool perf compare`.

Rules:

- Never edit these files by hand; re-record instead.
- A baseline only means something on the machine whose label it carries.
  `perf compare` refuses cross-system or cross-configuration comparisons.
- Refresh deliberately after a driver, kernel, display-topology, or
  configuration change — each of those is a new label, hence a new file.
