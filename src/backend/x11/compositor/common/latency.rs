//! Backend-independent latency statistics helpers.

const INLINE_LATENCY_SAMPLES: usize = 300;

pub(crate) fn latency_stats(samples: impl IntoIterator<Item = f32>) -> (f32, f32, f32, f32) {
    // The compositor retains at most 300 latency samples. Keep that common path
    // entirely on the stack so metrics reads do not allocate. The overflow path
    // preserves the helper's generic behavior for callers with larger inputs.
    let mut inline = [0.0f32; INLINE_LATENCY_SAMPLES];
    let mut inline_len = 0usize;
    let mut overflow: Option<Vec<f32>> = None;

    for sample in samples {
        if let Some(values) = overflow.as_mut() {
            values.push(sample);
        } else if inline_len < inline.len() {
            inline[inline_len] = sample;
            inline_len += 1;
        } else {
            let mut values = Vec::with_capacity(INLINE_LATENCY_SAMPLES * 2);
            values.extend_from_slice(&inline);
            values.push(sample);
            overflow = Some(values);
        }
    }

    if let Some(mut values) = overflow {
        summarize_latency_samples(&mut values)
    } else {
        summarize_latency_samples(&mut inline[..inline_len])
    }
}

fn summarize_latency_samples(samples: &mut [f32]) -> (f32, f32, f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }

    samples.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let len = samples.len();
    let p50_idx = (len * 50 / 100).min(len - 1);
    let p95_idx = (len * 95 / 100).min(len - 1);
    let p99_idx = (len * 99 / 100).min(len - 1);

    let avg = samples.iter().sum::<f32>() / len as f32;
    (avg, samples[p50_idx], samples[p95_idx], samples[p99_idx])
}

#[cfg(test)]
mod tests {
    use super::{INLINE_LATENCY_SAMPLES, latency_stats};

    #[test]
    fn empty_samples_are_zero() {
        assert_eq!(latency_stats([]), (0.0, 0.0, 0.0, 0.0));
    }

    #[test]
    fn uniform_samples_have_identical_percentiles() {
        let samples = vec![20.0; 100];
        let (avg, p50, p95, p99) = latency_stats(samples);
        assert!((avg - 20.0).abs() < 0.001);
        assert!((p50 - 20.0).abs() < 0.001);
        assert!((p95 - 20.0).abs() < 0.001);
        assert!((p99 - 20.0).abs() < 0.001);
    }

    #[test]
    fn ordered_samples_keep_percentile_order() {
        let samples: Vec<f32> = (1..=100).map(|i| i as f32).collect();
        let (_, p50, p95, p99) = latency_stats(samples);
        assert!(p50 <= p95);
        assert!(p95 <= p99);
    }

    #[test]
    fn inline_capacity_boundary_keeps_results() {
        let samples = (1..=INLINE_LATENCY_SAMPLES).map(|i| i as f32);
        let (avg, p50, p95, p99) = latency_stats(samples);

        assert!((avg - 150.5).abs() < 0.001);
        assert_eq!(p50, 151.0);
        assert_eq!(p95, 286.0);
        assert_eq!(p99, 298.0);
    }

    #[test]
    fn overflow_path_keeps_results() {
        let sample_count = INLINE_LATENCY_SAMPLES + 1;
        let samples = (1..=sample_count).map(|i| i as f32);
        let (avg, p50, p95, p99) = latency_stats(samples);

        assert!((avg - 151.0).abs() < 0.001);
        assert_eq!(p50, 151.0);
        assert_eq!(p95, 286.0);
        assert_eq!(p99, 298.0);
    }
}
