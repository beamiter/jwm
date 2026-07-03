//! Backend-independent latency statistics helpers.

pub(crate) fn latency_stats(samples: impl IntoIterator<Item = f32>) -> (f32, f32, f32, f32) {
    let mut sorted: Vec<f32> = samples.into_iter().collect();
    if sorted.is_empty() {
        return (0.0, 0.0, 0.0, 0.0);
    }

    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let len = sorted.len();
    let p50_idx = (len * 50 / 100).min(len - 1);
    let p95_idx = (len * 95 / 100).min(len - 1);
    let p99_idx = (len * 99 / 100).min(len - 1);

    let avg = sorted.iter().sum::<f32>() / len as f32;
    (avg, sorted[p50_idx], sorted[p95_idx], sorted[p99_idx])
}

#[cfg(test)]
mod tests {
    use super::latency_stats;

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
}
