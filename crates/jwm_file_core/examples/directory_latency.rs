//! Warm-filesystem local-provider benchmark. It measures no GTK rendering.
use jwm_file_core::{Cancellation, FileProvider, ListOptions, LocalProvider, Location};
use std::{
    fs, io,
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const COUNTS: [usize; 4] = [100, 1_000, 10_000, 100_000];

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> io::Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("jwm-files-latency-{}-{nonce}", std::process::id()));
        fs::create_dir(&root)?;
        Ok(Self(root))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn percentile(sorted: &[Duration], percent: usize) -> Duration {
    sorted[(sorted.len() * percent).div_ceil(100) - 1]
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let samples = args.next().map_or(Ok(20), |value| value.parse::<usize>())?;
    if !(5..=100).contains(&samples) || args.next().is_some() {
        return Err("usage: directory_latency [samples between 5 and 100]".into());
    }
    let fixture = Fixture::new()?;
    let provider = LocalProvider;
    let options = ListOptions::default();
    let cancellation = Cancellation::default();
    println!(
        "measure: local-provider snapshot; warm metadata; {}-{}; fixture: {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        fixture.0.display()
    );
    println!("entries,samples,p50_ms,p95_ms,max_ms");
    for entries in COUNTS {
        let path = fixture.0.join(entries.to_string());
        fs::create_dir(&path)?;
        for index in 0..entries {
            fs::File::create_new(path.join(format!("file-{index:06}")))?;
        }
        let location = Location::Local(path.clone());
        let warmup = provider.list(&location, options, &cancellation)?;
        assert_eq!(warmup.entries.len(), entries);
        assert_eq!(warmup.skipped_entries, 0);
        drop(warmup);

        let mut timings = Vec::with_capacity(samples);
        for _ in 0..samples {
            let start = Instant::now();
            let snapshot = provider.list(&location, options, &cancellation)?;
            timings.push(start.elapsed());
            assert_eq!(snapshot.entries.len(), entries);
            assert_eq!(snapshot.skipped_entries, 0);
        }
        timings.sort_unstable();
        let ms = |time: Duration| time.as_secs_f64() * 1_000.0;
        println!(
            "{entries},{samples},{:.3},{:.3},{:.3}",
            ms(percentile(&timings, 50)),
            ms(percentile(&timings, 95)),
            ms(timings[samples - 1])
        );
        fs::remove_dir_all(&path)?;
    }
    Ok(())
}
