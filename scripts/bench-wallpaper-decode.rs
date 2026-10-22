//! Run through bench-wallpaper-decode.py, which extracts the production pixel loop.
#[path = "../src/backend/compositor_common/wallpaper_worker_pool.rs"]
mod worker_pool;
include!(env!("JWM_WALLPAPER_OPAQUE_RS"));
use std::{hint::black_box, sync::mpsc, time::Instant};

// Fixed historical reference: the original unconditional alpha formula.
#[inline(never)]
fn before(pixels: &mut [u8]) {
    for pixel in pixels.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
        pixel[3] = 255;
    }
}

fn median(mut values: [u128; 3]) -> u128 {
    values.sort_unstable();
    values[1]
}

fn dispatch(pooled: bool, jobs: usize) -> u128 {
    let (tx, rx) = mpsc::channel();
    let start = Instant::now();
    for _ in 0..jobs {
        let tx = tx.clone();
        let job = Box::new(move || tx.send(()).unwrap());
        if pooled {
            worker_pool::get_or_start().unwrap().submit(job).unwrap();
        } else {
            std::thread::Builder::new()
                .name("jwm-wallpaper".into())
                .spawn(job)
                .unwrap();
        }
        rx.recv().unwrap();
    }
    start.elapsed().as_nanos()
}

fn pixels(source: &[u8], output: &mut [u8], optimized: bool, repetitions: usize) -> u128 {
    let mut elapsed = 0;
    for _ in 0..repetitions {
        // Restore the identical input outside the timed region. Both alpha
        // algorithms mutate the source alpha to 255 on the first pass.
        output.copy_from_slice(source);
        let now = Instant::now();
        if optimized {
            after(black_box(output));
        } else {
            before(black_box(output));
        }
        elapsed += now.elapsed().as_nanos();
        black_box(&*output);
    }
    elapsed
}

fn print_result(label: &str, old: [u128; 3], new: [u128; 3]) {
    let old = median(old);
    let new = median(new);
    println!(
        "{label}: median_before_us={:.1} median_after_us={:.1} speedup={:.3}x",
        old as f64 / 1000.0,
        new as f64 / 1000.0,
        old as f64 / new as f64
    );
}

fn main() {
    println!("rustc -O; 3 rounds; alternating before/after order; explicit warmup");
    dispatch(false, 100);
    dispatch(true, 100);
    let mut old = [0; 3];
    let mut new = [0; 3];
    for run in 0..3 {
        for optimized in if run % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let measured = dispatch(optimized, 2000);
            if optimized {
                new[run] = measured;
            } else {
                old[run] = measured;
            }
        }
    }
    print_result("dispatch 2000 sequential round trips", old, new);

    for (name, alpha) in [
        ("opaque", 255u8),
        ("transparent", 0),
        ("partial", 128),
        ("mixed", 254),
    ] {
        let mut source = vec![0u8; 3840 * 2160 * 4];
        for (index, pixel) in source.chunks_exact_mut(4).enumerate() {
            pixel.copy_from_slice(&[
                37,
                123,
                241,
                if alpha == 254 {
                    (index % 256) as u8
                } else {
                    alpha
                },
            ]);
        }
        let mut expected = source.clone();
        let mut output = source.clone();
        before(&mut expected);
        after(&mut output);
        assert_eq!(
            expected, output,
            "production and reference pixels must match"
        );
        for _ in 0..3 {
            pixels(&source, &mut output, false, 1);
            pixels(&source, &mut output, true, 1);
        }
        for run in 0..3 {
            // Interleave each pair, reversing its order between rounds.
            old[run] = 0;
            new[run] = 0;
            for _ in 0..12 {
                for optimized in if run % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                } {
                    let measured = pixels(&source, &mut output, optimized, 1);
                    if optimized {
                        new[run] += measured;
                    } else {
                        old[run] += measured;
                    }
                }
            }
        }
        print_result(&format!("RGBA {name} 3840x2160 x12"), old, new);
    }
}
