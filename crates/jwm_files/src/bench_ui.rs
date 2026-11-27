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
    open_window(
        &app,
        &base,
        &fixture.0,
        &[],
        &copy_ui::CopyBuffer::default(),
    );
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

const FRESH_WINDOW_MARKER: &str = "JWM_FILES_FRESH_WINDOW_PAINTED";
const FRESH_FILE: &str = "00-fresh-window-probe";

#[test]
#[ignore = "internal child for the private fresh-process UI benchmark"]
fn fresh_window_child() {
    let fixture = PathBuf::from(std::env::var_os("JWM_FILE_FRESH_WINDOW_ROOT").unwrap());
    assert!(fixture.is_absolute());
    eprintln!("fresh-window child: initializing GTK");
    gtk::init().unwrap();
    eprintln!("fresh-window child: GTK initialized");
    let app = gtk::Application::builder()
        .application_id("org.jwm.Files.FreshWindowBenchmark")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    eprintln!("fresh-window child: application registered");
    open_window(
        &app,
        &fixture,
        &fixture,
        &[],
        &copy_ui::CopyBuffer::default(),
    );
    eprintln!("fresh-window child: open_window returned");
    let window = app.active_window().unwrap();
    let expected_title = format!("JWM Files · {}", fixture.display());
    until(|| {
        window.is_mapped()
            && window.title().as_deref() == Some(&expected_title)
            && visit(window.upcast_ref(), &mut |widget| {
                widget.is_mapped()
                    && widget.width() > 0
                    && widget.height() > 0
                    && widget
                        .downcast_ref::<gtk::Label>()
                        .is_some_and(|label| label.text().as_str() == FRESH_FILE)
            })
    });
    eprintln!("fresh-window child: known row allocated");
    let clock = window.frame_clock().expect("mapped Files frame clock");
    let painted = Rc::new(Cell::new(false));
    let handler = clock.connect_after_paint({
        let painted = painted.clone();
        move |_| painted.set(true)
    });
    window.queue_draw();
    until(|| painted.get());
    eprintln!("fresh-window child: confirmed after-paint");
    clock.disconnect(handler);
    println!("{FRESH_WINDOW_MARKER}");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    window.close();
}

#[test]
#[ignore = "opt-in fresh-process benchmark requiring a private graphic display"]
fn fresh_window_benchmark() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let samples = std::env::var("JWM_FILE_FRESH_WINDOW_SAMPLES")
        .map_or(20, |value| value.parse::<usize>().unwrap());
    assert!((5..=100).contains(&samples));
    let fixture = Fixture::new();
    fs::File::create_new(fixture.0.join(FRESH_FILE)).unwrap();
    for index in 1..100 {
        fs::File::create_new(fixture.0.join(format!("entry-{index:06}"))).unwrap();
    }
    // A new test process and GTK application per sample. The same 100-row
    // synthetic fixture is reused; this does not flush kernel disk caches.
    let executable = std::env::current_exe().unwrap();
    let mut result = Vec::with_capacity(samples);
    for _ in 0..samples {
        let mut command = Command::new(&executable);
        command
            .args([
                "bench_ui::fresh_window_child",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("JWM_FILE_FRESH_WINDOW_ROOT", &fixture.0)
            .stdout(Stdio::piped());
        let (send, receive) = mpsc::sync_channel(1);
        let start = Instant::now();
        let mut child = command.spawn().unwrap();
        let stdout = child.stdout.take().unwrap();
        let read = thread::spawn(move || {
            let mut sent = false;
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if !sent && line.contains(FRESH_WINDOW_MARKER) {
                    sent = true;
                    let _ = send.send(start.elapsed());
                }
            }
        });
        let measurement = receive.recv_timeout(Duration::from_secs(12));
        let deadline = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if measurement.is_err() || Instant::now() >= deadline {
                let _ = child.kill();
                break child.wait().unwrap();
            }
            thread::sleep(Duration::from_millis(2));
        };
        read.join().unwrap();
        let latency = measurement.expect("fresh Files never confirmed a painted listing");
        assert!(status.success(), "fresh Files child exited with {status}");
        result.push(latency);
    }
    result.sort_unstable();
    let ranked = |rank: usize| display_ms(result[(samples * rank).div_ceil(100) - 1]);
    let backend = std::env::var("GDK_BACKEND").unwrap_or_else(|_| "auto".into());
    println!(
        "measure: spawn fresh GTK test client to confirmed first Files listing paint; \
         GDK_BACKEND={backend}; warm OS cache; fixture has 100 entries"
    );
    println!("samples,p50_ms,p95_ms,max_ms");
    println!(
        "{samples},{:.3},{:.3},{:.3}",
        ranked(50),
        ranked(95),
        display_ms(result[samples - 1])
    );
}

const NAV_READY: &str = "JWM_FILES_KEYBOARD_NAV_READY";
const NAV_PAINT: &str = "JWM_FILES_KEYBOARD_NAV_CONFIRMED_PAINT";
const NAV_BASE_ROW: &str = "00-start-visible";
const NAV_TARGET_ROW: &str = "00-target-visible";

fn confirm_native_listing(window: &gtk::Window, path: &Path, marker: &str) {
    let title = format!("JWM Files · {}", path.display());
    until(|| {
        window.is_mapped()
            && window.title().as_deref() == Some(title.as_str())
            && visit(window.upcast_ref(), &mut |widget| {
                widget.is_mapped()
                    && widget.width() > 0
                    && widget.height() > 0
                    && widget
                        .downcast_ref::<gtk::Label>()
                        .is_some_and(|label| label.text().as_str() == marker)
            })
    });
    let clock = window.frame_clock().unwrap();
    let painted = Rc::new(Cell::new(false));
    let handler = clock.connect_after_paint({
        let painted = painted.clone();
        move |_| painted.set(true)
    });
    window.queue_draw();
    until(|| painted.get());
    clock.disconnect(handler);
}

#[test]
#[ignore = "internal native keyboard navigation benchmark child"]
fn keyboard_navigation_child() {
    let root = PathBuf::from(std::env::var_os("JWM_FILE_KEYBOARD_ROOT").unwrap());
    let initial = root.join("start");
    let target = root.join("target with spaces 文件");
    gtk::init().unwrap();
    let display = gtk::gdk::Display::default().unwrap();
    assert_eq!(display.type_().name(), "GdkX11Display");
    let app = gtk::Application::builder()
        .application_id("org.jwm.Files.KeyboardNavigationBenchmark")
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.register(None::<&gio::Cancellable>).unwrap();
    open_window(&app, &initial, &root, &[], &copy_ui::CopyBuffer::default());
    let window = app.active_window().unwrap();
    confirm_native_listing(&window, &initial, NAV_BASE_ROW);
    println!("{NAV_READY}");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    confirm_native_listing(&window, &target, NAV_TARGET_ROW);
    println!("{NAV_PAINT}");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    window.close();
}

#[test]
#[ignore = "opt-in real keyboard benchmark requiring an owned private X11 display and session bus"]
fn keyboard_navigation_benchmark() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let display = std::env::var("DISPLAY").expect("private X11 display");
    let guard = std::env::var("JWM_FILE_KEYBOARD_PRIVATE_DISPLAY")
        .expect("run only inside the private Xvfb validation command");
    assert_eq!(display, guard);
    assert!(!display.is_empty());
    assert!(std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some());

    let samples = std::env::var("JWM_FILE_KEYBOARD_SAMPLES")
        .map_or(5, |raw| raw.parse::<usize>().unwrap());
    assert!((5..=30).contains(&samples));
    let fixture = Fixture::new();
    let initial = fixture.0.join("start");
    let target = fixture.0.join("target with spaces 文件");
    fs::create_dir(&initial).unwrap();
    fs::create_dir(&target).unwrap();
    fs::File::create_new(initial.join(NAV_BASE_ROW)).unwrap();
    fs::File::create_new(target.join(NAV_TARGET_ROW)).unwrap();
    let uri = gio::File::for_path(&target).uri();
    let opening = format!("JWM Files · {}", initial.display());
    let exe = std::env::current_exe().unwrap();
    let mut timings = Vec::with_capacity(samples);

    for _ in 0..samples {
        let mut child = Command::new(&exe)
            .args([
                "bench_ui::keyboard_navigation_child",
                "--exact",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("JWM_FILE_KEYBOARD_ROOT", &fixture.0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, receive) = mpsc::sync_channel(2);
        let pipe = thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if line.contains(NAV_READY) {
                    let _ = send.send((NAV_READY, Instant::now()));
                } else if line.contains(NAV_PAINT) {
                    let _ = send.send((NAV_PAINT, Instant::now()));
                }
            }
        });
        let xdo = |args: &[&str]| -> Result<String, String> {
            let output = Command::new("xdotool")
                .args(args)
                .output()
                .map_err(|e| e.to_string())?;
            if !output.status.success() {
                return Err(format!("xdotool {args:?} exited {:?}", output.status));
            }
            String::from_utf8(output.stdout).map_err(|e| e.to_string())
        };
        let step = (|| {
            let (phase, _) = receive
                .recv_timeout(Duration::from_secs(12))
                .map_err(|_| "initial listing was not confirmed".to_string())?;
            if phase != NAV_READY {
                return Err("unexpected initial readiness message".into());
            }
            let pid = child.id().to_string();
            let windows = xdo(&["search", "--onlyvisible", "--pid", &pid])?;
            let window = windows
                .lines()
                .find(|id| {
                    xdo(&["getwindowname", id])
                        .is_ok_and(|title| title.trim_end() == opening)
                })
                .ok_or_else(|| "no mapped Files window with the expected PID/title".to_owned())?;
            xdo(&["windowfocus", "--sync", window])?;
            xdo(&["key", "--clearmodifiers", "ctrl+l", "ctrl+a", "BackSpace"])?;
            xdo(&["type", "--clearmodifiers", "--delay", "0", uri.as_str()])?;
            let sent = Instant::now();
            xdo(&["key", "--clearmodifiers", "Return"])?;
            let (phase, painted) = receive
                .recv_timeout(Duration::from_secs(10))
                .map_err(|_| "one Ctrl+L and Return did not reveal the target row".to_owned())?;
            if phase != NAV_PAINT {
                return Err("unexpected navigation acknowledgement".into());
            }
            Ok(painted.saturating_duration_since(sent))
        })();
        let end = Instant::now() + Duration::from_secs(3);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if step.is_err() || Instant::now() >= end {
                let _ = child.kill();
                break child.wait().unwrap();
            }
            thread::sleep(Duration::from_millis(2));
        };
        pipe.join().unwrap();
        timings.push(step.expect("single keyboard path workflow failed"));
        assert!(status.success(), "native navigation child exited with {status}");
    }
    timings.sort_unstable();
    let median = display_ms(timings[samples.div_ceil(2) - 1]);
    let max = display_ms(timings[samples - 1]);
    println!("GTK4 X11 keyboard: one Ctrl+L/Return reached exact path and painted target row");
    println!("samples,missed_commits,p50_ms,max_ms");
    println!("{samples},0,{median:.3},{max:.3}");
}
