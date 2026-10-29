//! Testable daemon components; the executable owns the single session actor.
pub mod audio;
pub mod config;
pub mod context;
pub mod cost_report;
pub mod daemon;
pub mod errors;
pub mod executor;
pub mod feedback;
pub mod input;
pub mod lifecycle;
pub mod planning_input;
pub mod providers;
pub mod result_projection;
pub mod session;
pub mod store;
pub mod telemetry;
