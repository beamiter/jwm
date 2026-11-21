//! Registered native viewer input against installed Firefox and a loopback-only site.
use super::*;
use std::{
    io::{BufRead, BufReader, Read},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::atomic::{AtomicU8, AtomicU32},
};

const RED_PAGE: u8 = 1;
const BLUE_PAGE: u8 = 2;
const CLICKED: u8 = 4;
const WHEEL_EVENT: u8 = 8;
const CTRL: u32 = 29;
const SHIFT: u32 = 42;

struct TestSite {
    address: SocketAddr,
    seen: Arc<AtomicU8>,
    scroll_y: Arc<AtomicU32>,
    stop: Arc<AtomicBool>,
    worker: Option<thread::JoinHandle<()>>,
}

impl TestSite {
    fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicU8::new(0));
        let scroll_y = Arc::new(AtomicU32::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let (seen, scroll_y, stop) =
                (Arc::clone(&seen), Arc::clone(&scroll_y), Arc::clone(&stop));
            thread::spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((stream, _)) => serve(stream, &seen, &scroll_y),
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(4));
                        }
                        Err(error) => panic!("private HTTP test listener: {error}"),
                    }
                }
            })
        };
        Self {
            address,
            seen,
            scroll_y,
            stop,
            worker: Some(worker),
        }
    }

    fn observed(&self, traffic: u8) -> bool {
        self.seen.load(Ordering::Acquire) & traffic == traffic
    }
}

impl Drop for TestSite {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.worker.take().unwrap().join().unwrap();
    }
}

fn serve(mut stream: TcpStream, seen: &AtomicU8, scroll_y: &AtomicU32) {
    let timeout = Some(Duration::from_millis(300));
    stream.set_read_timeout(timeout).unwrap();
    stream.set_write_timeout(timeout).unwrap();
    let mut request = String::new();
    let mut reader = BufReader::new(&mut stream).take(8192);
    if reader.read_line(&mut request).is_err() {
        return;
    }
    for line in reader.lines() {
        match line {
            Ok(line) if line.is_empty() => break,
            Ok(_) => {}
            Err(_) => return,
        }
    }
    let mut words = request.split_ascii_whitespace();
    let (status, content, observation) = match (words.next(), words.next()) {
        (Some("GET"), Some("/a")) => (
            "200 OK",
            r#"<!doctype html><title>Red tab</title><style>
            body { margin: 0; background: #16801e; font: 16px sans-serif; }
            header { height: 230px; background: #ee2826; }
            main { height: 1600px; }
            </style><header>Top</header><main>Lower region</main><script>
            addEventListener('click', () => { fetch('/clicked'); }, { once: true });
            addEventListener('wheel', () => { fetch('/wheel'); }, { once: true, passive: true });
            addEventListener('scroll', () => {
              const y = Math.round(window.scrollY);
              if (y > 0) fetch(`/scrolled/${y}`);
            }, { passive: true });
            </script>"#,
            RED_PAGE,
        ),
        (Some("GET"), Some("/b")) => (
            "200 OK",
            "<!doctype html><title>Blue tab</title><body style='background:#1847ee'>",
            BLUE_PAGE,
        ),
        (Some("GET"), Some("/clicked")) => ("204 No Content", "", CLICKED),
        (Some("GET"), Some("/wheel")) => ("204 No Content", "", WHEEL_EVENT),
        (Some("GET"), Some(path)) if path.starts_with("/scrolled/") => {
            if let Ok(y) = path["/scrolled/".len()..].parse() {
                scroll_y.fetch_max(y, Ordering::Release);
            }
            ("204 No Content", "", 0)
        }
        _ => ("404 Not Found", "", 0),
    };
    if write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{content}",
        content.len()
    )
    .is_ok()
    {
        seen.fetch_or(observation, Ordering::Release);
    }
}

fn navigate(app: &mut ApplicationSession, gl: &mut HeadlessSmithayGles, url: &str) {
    app.chord(gl, &[CTRL], 38); // L
    for byte in url.bytes() {
        let (code, modifiers): (u32, &[u32]) = match byte {
            b'a' => (30, &[]),
            b'b' => (48, &[]),
            b'h' => (35, &[]),
            b't' => (20, &[]),
            b'p' => (25, &[]),
            b':' => (39, &[SHIFT]),
            b'/' => (53, &[]),
            b'.' => (52, &[]),
            b'0' => (11, &[]),
            b'1'..=b'9' => (u32::from(byte - b'1') + 2, &[]),
            _ => panic!("unexpected character in private URL"),
        };
        app.chord(gl, modifiers, code);
    }
    app.chord(gl, &[], 28); // Enter
}

fn visible_color(pixels: &[u8], color: [u8; 3]) -> usize {
    if pixels.len() != (SIZE.0 * SIZE.1 * 4) as usize {
        return 0;
    }
    // Keep the sample below the browser chrome and away from either edge.
    (240..260)
        .flat_map(|y| (140..500).map(move |x| (y * SIZE.0 as usize + x) * 4))
        .filter(|&index| {
            let [red, green, blue] = color;
            [blue, green, red]
                .into_iter()
                .zip(&pixels[index..index + 3])
                .all(|(expected, actual)| expected.abs_diff(*actual) <= 28)
        })
        .count()
}

#[test]
#[ignore = "requires installed Firefox, private DBus, standalone CLI and strict software GL"]
fn actual_firefox_remote_address_tabs_and_wheel() {
    let executable = PathBuf::from(std::env::var_os("JWM_REMOTE_FIREFOX_BIN").expect("required"));
    assert!(executable.is_absolute() && executable.is_file());
    assert!(std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some());
    let mut gl = HeadlessSmithayGles::new().expect("strict software GL required");
    let directory = PrivateDir::new();
    let profile = directory.0.join("firefox");
    std::fs::create_dir(&profile).unwrap();
    let site = TestSite::new();
    // Route background HTTP(S) traffic to our loopback server, which refuses
    // proxy requests. The
    // intentionally local test site remains reachable with normal HTTP.
    let mut preferences = File::create(profile.join("user.js")).unwrap();
    preferences
        .write_all(
            r#"
user_pref("network.proxy.type", 1);
user_pref("network.proxy.http", "127.0.0.1");
user_pref("network.proxy.ssl", "127.0.0.1");
user_pref("network.proxy.no_proxies_on", "localhost,127.0.0.1");
user_pref("network.http.speculative-parallel-limit", 0);
user_pref("browser.aboutwelcome.enabled", false);
user_pref("browser.shell.checkDefaultBrowser", false);
user_pref("browser.startup.homepage", "about:blank");
user_pref("browser.startup.page", 0);
user_pref("browser.newtabpage.enabled", false);
user_pref("browser.urlbar.suggest.searches", false);
user_pref("browser.urlbar.quicksuggest.enabled", false);
user_pref("gfx.webrender.software", true);
"#
            .as_bytes(),
        )
        .unwrap();
    let port = site.address.port();
    writeln!(
        preferences,
        "user_pref(\"network.proxy.http_port\", {port});"
    )
    .unwrap();
    writeln!(
        preferences,
        "user_pref(\"network.proxy.ssl_port\", {port});"
    )
    .unwrap();
    drop(preferences);
    let mut app = ApplicationSession::start(&mut gl, directory, &executable, 1, true, |command| {
        Process::spawn(
            command
                .args(["--new-instance", "--profile"])
                .arg(&profile)
                .arg("about:blank")
                .env("MOZ_NO_REMOTE", "1")
                .env("MOZ_ENABLE_WAYLAND", "1")
                .env_remove("MOZ_HEADLESS"),
        )
    });
    eprintln!(
        "JWM_FIREFOX_PAINT_CHILDREN={}",
        smithay::wayland::compositor::get_children(app.source_root.as_ref().unwrap()).len()
    );
    let red = [0xee, 0x28, 0x26];
    let blue = [0x18, 0x47, 0xee];
    let green = [0x16, 0x80, 0x1e];
    navigate(&mut app, &mut gl, &format!("http://{}/a", site.address));
    app.wait(
        &mut gl,
        "typed first address loads and displays red",
        |app| site.observed(RED_PAGE) && visible_color(&app.presented, red) > 6500,
    );
    eprintln!(
        "JWM_FIREFOX_FIRST_ADDRESS_PIXELS={}",
        visible_color(&app.presented, red)
    );
    app.chord(&mut gl, &[CTRL], 20); // T
    navigate(&mut app, &mut gl, &format!("http://{}/b", site.address));
    app.wait(
        &mut gl,
        "typed second address loads and displays blue",
        |app| site.observed(BLUE_PAGE) && visible_color(&app.presented, blue) > 6500,
    );
    eprintln!(
        "JWM_FIREFOX_SECOND_ADDRESS_PIXELS={}",
        visible_color(&app.presented, blue)
    );
    app.chord(&mut gl, &[CTRL], 15); // Tab
    app.wait(
        &mut gl,
        "tab switch presents the existing red page",
        |app| visible_color(&app.presented, red) > 6500,
    );
    eprintln!(
        "JWM_FIREFOX_SWITCH_PIXELS={}",
        visible_color(&app.presented, red)
    );
    let click_proof = app.pointer_proof(&mut gl, None);
    app.left_button(true);
    app.left_button(false);
    app.wait(&mut gl, "the page receives a native mouse click", |_| {
        site.observed(CLICKED)
    });
    let mut previous_ready = Some(click_proof);
    for _ in 0..12 {
        previous_ready = Some(app.pointer_proof(&mut gl, previous_ready));
        app.wheel(1);
        app.pump_input(&mut gl);
        if site.scroll_y.load(Ordering::Acquire) >= 80
            && visible_color(&app.presented, green) > 6500
        {
            break;
        }
    }
    eprintln!(
        "JWM_FIREFOX_PAGE_SCROLL_Y={} GREEN_PIXELS={} WHEEL_RECEIVED={}",
        site.scroll_y.load(Ordering::Acquire),
        visible_color(&app.presented, green),
        site.observed(WHEEL_EVENT)
    );
    app.wait(
        &mut gl,
        "the page reports scroll and presents its lower region",
        |app| {
            site.observed(WHEEL_EVENT)
                && site.scroll_y.load(Ordering::Acquire) >= 80
                && visible_color(&app.presented, green) > 6500
        },
    );
    eprintln!(
        "JWM_FIREFOX_SCROLL_PIXELS={}",
        visible_color(&app.presented, green)
    );
    app.disconnect(&mut gl);
}
