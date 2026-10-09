#![cfg(test)]

use glib::variant::ToVariant;

#[test]
fn optimized_variant_string_iterator_preserves_all_outputs() {
    // Plain synthetic strings; no window, screen, display, or user data.
    let variant = ["alpha", "beta", "gamma"].to_variant();
    let mut iter = variant.array_iter_str().unwrap();
    assert_eq!(iter.len(), 3);
    assert_eq!(iter.next(), Some("alpha"));
    assert_eq!(iter.next_back(), Some("gamma"));
    assert_eq!(iter.next(), Some("beta"));
    assert_eq!(iter.next(), None);
    assert_eq!(iter.next_back(), None);
    assert_eq!(variant.array_iter_str().unwrap().nth(1), Some("beta"));
    assert_eq!(variant.array_iter_str().unwrap().nth_back(1), Some("beta"));
    assert_eq!(variant.array_iter_str().unwrap().last(), Some("gamma"));
    assert_eq!(
        variant.array_iter_str().unwrap().collect::<Vec<_>>(),
        vec!["alpha", "beta", "gamma"]
    );
    let empty: [&str; 0] = [];
    assert_eq!(empty.to_variant().array_iter_str().unwrap().next(), None);
    assert!(42u32.to_variant().array_iter_str().is_err());
}
