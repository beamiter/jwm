use super::*;
use std::time::Instant;
use std::{
    fs,
    time::{SystemTime, UNIX_EPOCH},
};

const COUNTS: [usize; 4] = [100, 1_000, 10_000, 100_000];

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("jwm-files-ui-bench-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn visit(widget: &gtk::Widget, accept: &mut impl FnMut(&gtk::Widget) -> bool) -> bool {
    if accept(widget) {
        return true;
    }
    let mut child = widget.first_child();
    while let Some(widget) = child {
        child = widget.next_sibling();
        if visit(&widget, accept) {
            return true;
        }
    }
    false
}

fn until(mut ready: impl FnMut() -> bool) {
    let context = glib::MainContext::default();
    let end = Instant::now() + Duration::from_secs(8);
    while !ready() {
        assert!(
            Instant::now() < end,
            "timed out waiting for visible listing"
        );
        if !context.iteration(false) {
            thread::sleep(Duration::from_millis(1));
        }
    }
}

fn display_ms(value: Duration) -> f64 {
    value.as_secs_f64() * 1_000.0
}

#[test]
#[ignore = "opt-in benchmark requiring a private graphic display"]
fn native_listing_benchmark() {
    gtk::init().unwrap();
    let display = gtk::gdk::Display::default().unwrap();
    let samples =
        std::env::var("JWM_FILE_BENCH_SAMPLES").map_or(20, |value| value.parse::<usize>().unwrap());
    assert!((5..=100).contains(&samples));
    let fixture = Fixture::new();
    let base = fixture.0.join("baseline");
    fs::create_dir(&base).unwrap();
    let app = gtk::Application::builder()
        .application_id("org.jwm.Files.ListingBenchmark")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    open_window(&app, &base, &fixture.0, &[]);
    let window = app.active_window().unwrap();
    until(|| window.is_mapped() && window.frame_clock().is_some());
    let clock = window.frame_clock().unwrap();
    let mut address = None;
    assert!(visit(window.upcast_ref(), &mut |widget| {
        address = widget.clone().downcast::<gtk::Entry>().ok();
        address.is_some()
    }));
    let address = address.unwrap();

    let navigate = |target: &Path| {
        gtk::prelude::WidgetExt::activate_action(&window, "win.location", None).unwrap();
        address.set_text(&target.to_string_lossy());
        address.emit_activate();
    };
    let first_row = |name: &str| {
        visit(window.upcast_ref(), &mut |widget| {
            widget.is_mapped()
                && widget
                    .downcast_ref::<gtk::Label>()
                    .is_some_and(|label| label.text().as_str() == name)
        })
    };
    let at = |path: &Path| {
        window
            .title()
            .is_some_and(|value| value.as_str() == format!("JWM Files · {}", path.display()))
    };
    println!(
        "measure: warm directory switch to confirmed client paint; {}; GTK {}.{}.{}; fixture: {}",
        display.type_().name(),
        gtk::major_version(),
        gtk::minor_version(),
        gtk::micro_version(),
        fixture.0.display()
    );
    println!("entries,samples,p50_ms,p95_ms,max_ms");
    for entries in COUNTS {
        let path = fixture.0.join(entries.to_string());
        fs::create_dir(&path).unwrap();
        let marker = format!("00-benchmark-entry-{entries}");
        fs::File::create_new(path.join(&marker)).unwrap();
        for index in 1..entries {
            fs::File::create_new(path.join(format!("entry-{index:06}"))).unwrap();
        }
        navigate(&path);
        until(|| at(&path) && first_row(&marker));

        let mut results = Vec::with_capacity(samples);
        for _ in 0..samples {
            navigate(&base);
            until(|| at(&base) && !first_row(&marker));
            let start = Instant::now();
            navigate(&path);
            until(|| at(&path) && first_row(&marker));
            let painted = Rc::new(Cell::new(false));
            let handler = clock.connect_after_paint({
                let painted = painted.clone();
                move |_| painted.set(true)
            });
            window.queue_draw();
            until(|| painted.get());
            results.push(start.elapsed());
            clock.disconnect(handler);
        }
        results.sort_unstable();
        let ranked = |rank: usize| display_ms(results[(samples * rank).div_ceil(100) - 1]);
        println!(
            "{entries},{samples},{:.3},{:.3},{:.3}",
            ranked(50),
            ranked(95),
            display_ms(results[samples - 1])
        );
        navigate(&base);
        until(|| at(&base) && !first_row(&marker));
        fs::remove_dir_all(path).unwrap();
    }
    window.close();
}
