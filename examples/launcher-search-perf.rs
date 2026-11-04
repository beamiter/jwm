//! Repeatable component diagnostic for the production typed-session launcher.
//! No compositor, provider, model, socket or GUI is created.
use jwm::jwm::features::launcher::AgentEntry;
use jwm::jwm::features::system_ui::SystemUiState;
use serde_json::{Value, json};
use std::hint::black_box;
use std::sync::Arc;
use std::time::Instant;

const INDEX_SIZE: usize = 1000;
const WARMUP: usize = 25;
const TARGET_P95_MS: f64 = 50.0;

fn snapshot(case: &str) -> Value {
    let sessions: Vec<_> = (0..INDEX_SIZE)
        .map(|index| {
            let (title, project, id) = match case {
                "max_ascii_metadata" => (
                    format!("{}-{index:07}", "A".repeat(504)),
                    "A".repeat(256),
                    format!("{}-{index:04}", "A".repeat(240)),
                ),
                "max_unicode_metadata" => (
                    "Σ".repeat(256), // 512 bytes: the registry title limit.
                    "Σ".repeat(128),
                    format!("{}-{index:04}", "Σ".repeat(120)),
                ),
                "adverse_subsequence_miss" => (
                    "ab".repeat(256),
                    "ab".repeat(128),
                    format!("{}-{index:04}", "ab".repeat(120)),
                ),
                _ => (
                    format!("Compile cplus task {index:04}"),
                    format!("cplus/project-{}", index % 20),
                    format!("codex-session-{index:04}"),
                ),
            };
            json!({"session_id":id,"title":title,"project_id":project,
                "provider":(["forge","anvil","ember","frost"][index%4]),
                "host_id":format!("host-{}",index%4),"pane_id":format!("pane-{index}"),
                "revision":1,"monitor_token":format!("output-{}",index%2),
                "monitor":index%2,"tag_mask":1_u64<<(index%9),"status":"running","can_reveal":true})
        })
        .collect();
    json!({"sessions":sessions})
}

fn percentile(sorted: &[f64], numerator: usize) -> f64 {
    let index = (sorted.len() * numerator).div_ceil(100).saturating_sub(1);
    sorted[index.min(sorted.len() - 1)]
}

fn measure(case: &str, samples: usize) -> Value {
    let snapshot = snapshot(case);
    let build_started = Instant::now();
    let index = AgentEntry::from_snapshot(&snapshot);
    assert_eq!(index.len(), INDEX_SIZE);
    let index_build_ms = build_started.elapsed().as_secs_f64() * 1000.0;
    let full_query = match case {
        "max_ascii_metadata" | "adverse_subsequence_miss" => {
            format!("sessions:{}", "a".repeat(247))
        }
        "max_unicode_metadata" => format!("sessions:{}", "σ".repeat(247)),
        _ => "sessions:cplus".to_string(),
    };
    // The final typed character, not construction/loading of the provider
    // snapshot, is timed. This is the hot-index50ms target's intended boundary.
    let mut prefix = full_query.clone();
    let last = prefix.pop().unwrap();
    let mut state = SystemUiState::open_launcher(Arc::from([]), Vec::new(), false);
    if let SystemUiState::Launcher { sessions, .. } = &mut state {
        *sessions = index;
    }
    let mut durations = Vec::with_capacity(samples);
    let mut max_rows = 0;
    for iteration in 0..(samples + WARMUP) {
        if let SystemUiState::Launcher { query, .. } = &mut state {
            query.clone_from(&prefix);
        }
        let started = Instant::now();
        state.push_char(black_box(last));
        // Include the actual visible-row conversion handed to the compositor,
        // while excluding any renderer/presentation or provider I/O.
        black_box(state.overlay_parts());
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        if let SystemUiState::Launcher { matches, .. } = &state {
            max_rows = max_rows.max(matches.len());
            assert!(matches.len() <= 128, "production result bound changed");
        }
        if iteration >= WARMUP {
            durations.push(elapsed_ms);
        }
    }
    durations.sort_by(f64::total_cmp);
    let p95 = percentile(&durations, 95);
    json!({"case":case,"index_size":INDEX_SIZE,"index_build_ms":index_build_ms,
        "typed_query_chars":full_query.chars().count(),"samples":samples,"warmup":WARMUP,
        "p50_ms":percentile(&durations,50),"p95_ms":p95,"p99_ms":percentile(&durations,99),
        "max_ms":durations[durations.len()-1],"max_returned_rows":max_rows,
        "candidate_target_p95_ms":TARGET_P95_MS,"candidate_target_met":p95<=TARGET_P95_MS})
}

fn main() {
    let args: Vec<_> = std::env::args().skip(1).collect();
    let samples = match args.as_slice() {
        [] => 200,
        [flag, count] if flag == "--samples" => {
            count.parse::<usize>().expect("samples must be an integer")
        }
        _ => panic!("usage: launcher-search-perf [--samples1..10000]"),
    };
    assert!((1..=10_000).contains(&samples));
    let cases: Vec<_> = [
        "typical_ascii",
        "max_ascii_metadata",
        "max_unicode_metadata",
        "adverse_subsequence_miss",
    ]
    .into_iter()
    .map(|case| measure(case, samples))
    .collect();
    println!("{}",serde_json::to_string_pretty(&json!({"format":1,
        "measurement_kind":"cpu_monotonic_typed_character_to_ranked_visible_rows",
        "source":"production SystemUiState::push_char and overlay_parts",
        "clock_domain":"rust_instant_monotonic","profile":if cfg!(debug_assertions){"debug"}else{"release"},
        "scope":"isolated warm1000-session index; no provider/network/render/presentation time",
        "target_status":"candidate diagnostic, not a portable CI or physical acceptance gate",
        "cases":cases})).unwrap());
}
