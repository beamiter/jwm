//! Desktop icon lookup for the focused window.
//!
//! The window manager tells a bar which application owns the focused window —
//! an X11 class or a Wayland app-id — and nothing more, because that is the
//! only identity a compositor actually has. Turning it into a picture is a
//! desktop-integration job, and this is it: the freedesktop desktop-entry and
//! icon-theme lookup, cut down to what a status bar needs.
//!
//! Three things shape the implementation.
//!
//! *It must not stall a frame.* Resolution walks directories, so it happens
//! once per application identity and is cached afterwards — including the
//! failures, which are otherwise the expensive case, since a name with no
//! desktop entry is what triggers the full scan.
//!
//! *It resolves to a file, not to pixels.* Decoding belongs to whichever
//! renderer is drawing, which is also the only layer that knows what size it
//! wants and how to cache textures. [`AppIcon`] therefore carries a path and a
//! stable key.
//!
//! *It only offers raster icons.* Every renderer here can put a PNG on screen;
//! none of them carries an SVG rasterizer, and pretending otherwise would mean
//! a bar that shows an icon for some applications and a hole for others
//! depending on which theme is installed. A scalable-only application falls
//! back to no icon at all, and the title stands on its own as it did before.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// Icon file resolved for one application identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct AppIcon {
    /// Absolute path of a raster image a renderer can decode.
    pub path: PathBuf,
    /// Stable identity of that file, for renderer-side texture caches. Equal
    /// keys mean the same file: it is derived from the path alone, so it stays
    /// put across frames, snapshots and processes.
    pub key: u64,
}

impl AppIcon {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        path.hash(&mut hasher);
        Self {
            key: hasher.finish(),
            path,
        }
    }
}

/// Image extensions a renderer in this workspace can decode.
const RASTER_EXTENSIONS: [&str; 2] = ["png", "webp"];

/// A desktop entry is configuration, not an arbitrary document. This is well
/// above the size of ordinary translated entries while keeping a malformed
/// XDG tree out of the bar's frame path.
const MAX_DESKTOP_ENTRY_BYTES: usize = 256 * 1024;

/// The StartupWMClass fallback is intentionally bounded: XDG data roots are
/// user controlled and resolving one focused application must not walk an
/// unbounded directory or consume an unbounded aggregate amount of input.
const MAX_DESKTOP_SCAN_ENTRIES: usize = 4_096;
const MAX_DESKTOP_SCAN_BYTES: usize = 8 * 1024 * 1024;

/// Search collections ultimately originate in environment variables or host
/// configuration. Keep every nested walk finite even when those inputs are
/// pathological.
const MAX_ICON_SEARCH_ROOTS: usize = 64;
const MAX_PREFERRED_THEMES: usize = 16;
const MAX_THEME_SIZE_DIRECTORIES: usize = 256;

/// Directory holding one theme's icons at one size.
///
/// Themes disagree about which half of the path carries the size —
/// `hicolor/48x48/apps` and `breeze/apps/48` are both in the wild — so both
/// shapes are probed rather than parsed out of `index.theme`. A status bar
/// needs one icon, not a faithful theme engine.
const SIZE_LAYOUTS: [fn(&Path, &str, &str) -> PathBuf; 2] = [
    |theme, size, category| theme.join(size).join(category),
    |theme, category, size| theme.join(category).join(size),
];

/// Icon directories worth searching for an application icon.
const CATEGORIES: [&str; 2] = ["apps", "devices"];

/// Where desktop entries and icon themes live for this session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconSearchPaths {
    /// `applications` directories, in freedesktop precedence order.
    pub applications: Vec<PathBuf>,
    /// Icon theme roots, in precedence order.
    pub themes: Vec<PathBuf>,
    /// Flat directories such as `/usr/share/pixmaps`.
    pub flat: Vec<PathBuf>,
    /// Theme names to try before `hicolor`.
    pub preferred_themes: Vec<String>,
}

impl Default for IconSearchPaths {
    fn default() -> Self {
        Self::from_environment()
    }
}

impl IconSearchPaths {
    /// Resolve the freedesktop base directories from the environment.
    #[must_use]
    pub fn from_environment() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let data_home = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| home.as_ref().map(|home| home.join(".local/share")));
        let data_dirs = std::env::var_os("XDG_DATA_DIRS")
            .filter(|value| !value.is_empty())
            .map_or_else(
                || {
                    vec![
                        PathBuf::from("/usr/local/share"),
                        PathBuf::from("/usr/share"),
                    ]
                },
                |value| {
                    std::env::split_paths(&value)
                        .filter(|path| path.is_absolute())
                        .collect()
                },
            );

        let roots: Vec<PathBuf> = data_home.iter().cloned().chain(data_dirs).collect();
        Self {
            applications: roots.iter().map(|root| root.join("applications")).collect(),
            themes: home
                .iter()
                .map(|home| home.join(".icons"))
                .chain(roots.iter().map(|root| root.join("icons")))
                .collect(),
            flat: roots.iter().map(|root| root.join("pixmaps")).collect(),
            preferred_themes: preferred_themes_from_environment(),
        }
    }

    /// Build search paths explicitly, which is what tests and hosts with their
    /// own layout use.
    #[must_use]
    pub fn new(
        applications: Vec<PathBuf>,
        themes: Vec<PathBuf>,
        flat: Vec<PathBuf>,
        preferred_themes: Vec<String>,
    ) -> Self {
        Self {
            applications,
            themes,
            flat,
            preferred_themes,
        }
    }
}

/// Themes to search before `hicolor`, honouring the one environment variable
/// a bar can read without a settings daemon.
fn preferred_themes_from_environment() -> Vec<String> {
    std::env::var("XBAR_ICON_THEME")
        .ok()
        .into_iter()
        .flat_map(|value| {
            value
                .split(':')
                .map(str::trim)
                .filter(|theme| !theme.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .collect()
}

/// Cached application-identity → icon-file lookup.
#[derive(Debug, Clone)]
pub struct AppIconResolver {
    paths: IconSearchPaths,
    /// Size the bar draws icons at. Only a hint: the closest available size
    /// wins, and a larger source is preferred to a smaller one because
    /// downscaling keeps more of the artwork than upscaling invents.
    preferred_size: u32,
    cache: HashMap<String, Option<AppIcon>>,
}

impl Default for AppIconResolver {
    fn default() -> Self {
        Self::new(DEFAULT_ICON_PIXEL_SIZE)
    }
}

/// Icon size a bar of conventional height wants.
pub const DEFAULT_ICON_PIXEL_SIZE: u32 = 24;

impl AppIconResolver {
    #[must_use]
    pub fn new(preferred_size: u32) -> Self {
        Self::with_paths(IconSearchPaths::from_environment(), preferred_size)
    }

    #[must_use]
    pub fn with_paths(paths: IconSearchPaths, preferred_size: u32) -> Self {
        Self {
            paths,
            preferred_size: preferred_size.max(1),
            cache: HashMap::new(),
        }
    }

    /// Forget every cached answer, so a newly installed application is picked
    /// up without restarting the bar.
    pub fn clear(&mut self) {
        self.cache.clear();
    }

    /// Icon for one application identity, resolving it at most once.
    ///
    /// A negative answer is cached too — the scan that produced it is the
    /// expensive one, and repeating it on every focus change would put a
    /// directory walk in the frame path for exactly the windows that have no
    /// icon to show.
    pub fn resolve(&mut self, app_id: &str) -> Option<&AppIcon> {
        let app_id = app_id.trim();
        // A compositor identity names an application, never a filesystem
        // path. Reject separators and `..` before it is joined beneath an
        // applications or icon root.
        if !is_single_path_component(app_id) {
            return None;
        }
        if !self.cache.contains_key(app_id) {
            let resolved = self.lookup(app_id);
            self.cache.insert(app_id.to_owned(), resolved);
        }
        self.cache.get(app_id).and_then(Option::as_ref)
    }

    fn lookup(&self, app_id: &str) -> Option<AppIcon> {
        // An application whose desktop entry names an icon is the common case
        // and the accurate one; the identity doubling as an icon name is the
        // fallback that covers applications shipping no entry at all.
        let from_entry = self
            .desktop_entry_for(app_id)
            .and_then(|entry| desktop_entry_icon(&entry))
            .and_then(|icon| self.icon_file(&icon));
        from_entry.or_else(|| self.icon_file(app_id))
    }

    /// Contents of the desktop entry describing `app_id`.
    fn desktop_entry_for(&self, app_id: &str) -> Option<String> {
        let mut remaining_bytes = MAX_DESKTOP_SCAN_BYTES;
        for directory in self.paths.applications.iter().take(MAX_ICON_SEARCH_ROOTS) {
            if remaining_bytes == 0 {
                break;
            }
            for name in entry_file_candidates(app_id) {
                if let Some(text) =
                    read_desktop_entry_with_budget(&directory.join(&name), &mut remaining_bytes)
                {
                    return Some(text);
                }
            }
        }
        // Nothing is named after the window's class, so fall back to the one
        // field written precisely for this: `StartupWMClass`. This is the
        // expensive bounded scan, which is why the result — including "none"
        // — is cached by the caller.
        let mut remaining_entries = MAX_DESKTOP_SCAN_ENTRIES;
        for directory in self.paths.applications.iter().take(MAX_ICON_SEARCH_ROOTS) {
            if remaining_entries == 0 || remaining_bytes == 0 {
                break;
            }
            let Ok(entries) = std::fs::read_dir(directory) else {
                continue;
            };
            for entry in entries.take(remaining_entries) {
                remaining_entries -= 1;
                let Ok(entry) = entry else {
                    continue;
                };
                let path = entry.path();
                if path.extension() != Some(OsStr::new("desktop")) {
                    continue;
                }
                if remaining_bytes == 0 {
                    break;
                }
                let Some(text) = read_desktop_entry_with_budget(&path, &mut remaining_bytes) else {
                    continue;
                };
                if desktop_entry_startup_wm_class(&text)
                    .is_some_and(|class| class.eq_ignore_ascii_case(app_id))
                {
                    return Some(text);
                }
            }
        }
        None
    }

    /// Resolve an icon name (or absolute path) to a file this workspace's
    /// renderers can decode.
    fn icon_file(&self, icon: &str) -> Option<AppIcon> {
        let icon = icon.trim();
        if icon.is_empty() {
            return None;
        }
        let candidate = Path::new(icon);
        if candidate.is_absolute() {
            return is_supported_raster(candidate).then(|| AppIcon::new(candidate.to_path_buf()));
        }
        if !is_single_path_component(icon) {
            return None;
        }

        let themes = self
            .paths
            .preferred_themes
            .iter()
            .take(MAX_PREFERRED_THEMES)
            .map(String::as_str)
            .filter(|theme| is_single_path_component(theme))
            .chain(["hicolor"]);
        for theme in themes {
            for root in self.paths.themes.iter().take(MAX_ICON_SEARCH_ROOTS) {
                if let Some(found) = self.themed_icon(&root.join(theme), icon) {
                    return Some(found);
                }
            }
        }

        for directory in self.paths.flat.iter().take(MAX_ICON_SEARCH_ROOTS) {
            for extension in RASTER_EXTENSIONS {
                let path = directory.join(format!("{icon}.{extension}"));
                if path.is_file() {
                    return Some(AppIcon::new(path));
                }
            }
        }
        None
    }

    /// Best size of `icon` inside one theme directory.
    fn themed_icon(&self, theme: &Path, icon: &str) -> Option<AppIcon> {
        let mut best: Option<(u32, PathBuf)> = None;
        for size in sizes_in(theme) {
            for category in CATEGORIES {
                for layout in SIZE_LAYOUTS {
                    let directory = layout(theme, &size.directory, category);
                    for extension in RASTER_EXTENSIONS {
                        let path = directory.join(format!("{icon}.{extension}"));
                        if !path.is_file() {
                            continue;
                        }
                        let better = best.as_ref().is_none_or(|(current, _)| {
                            size_rank(size.pixels, self.preferred_size)
                                < size_rank(*current, self.preferred_size)
                        });
                        if better {
                            best = Some((size.pixels, path));
                        }
                    }
                }
            }
        }
        best.map(|(_, path)| AppIcon::new(path))
    }
}

/// Read a regular UTF-8 desktop entry without allowing a device, socket, FIFO
/// or oversized file to turn synchronous icon lookup into unbounded I/O.
fn read_desktop_entry_with_budget(path: &Path, remaining: &mut usize) -> Option<String> {
    let limit = (*remaining).min(MAX_DESKTOP_ENTRY_BYTES);
    if limit == 0 {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > limit as u64 {
        return None;
    }

    let file = std::fs::File::open(path).ok()?;
    // Re-check the opened descriptor so a replacement with another regular
    // file cannot bypass the size bound between the pathname checks.
    let opened = file.metadata().ok()?;
    if !opened.is_file() || opened.len() > limit as u64 {
        return None;
    }
    let mut bytes = Vec::with_capacity(opened.len() as usize);
    let read = file.take(limit as u64 + 1).read_to_end(&mut bytes);
    *remaining = remaining.saturating_sub(bytes.len());
    read.ok()?;
    if bytes.len() > limit {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// One `48x48`-style directory inside a theme.
struct ThemeSize {
    directory: String,
    pixels: u32,
}

/// Size directories present in a theme, whatever it calls them.
fn sizes_in(theme: &Path) -> Vec<ThemeSize> {
    let Ok(entries) = std::fs::read_dir(theme) else {
        return Vec::new();
    };
    let mut sizes: Vec<ThemeSize> = entries
        .take(MAX_THEME_SIZE_DIRECTORIES)
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            parse_size_directory(&name).map(|pixels| ThemeSize {
                directory: name,
                pixels,
            })
        })
        .collect();
    // A stable order keeps the chosen file identical between runs when two
    // directories describe the same pixel size (`48x48` and `48x48@2x`).
    sizes.sort_by(|a, b| a.pixels.cmp(&b.pixels).then(a.directory.cmp(&b.directory)));
    sizes
}

/// Pixel size a theme size-directory name describes, if it describes one.
///
/// `48x48`, `48x48@2x` and the bare `48` KDE uses all resolve; `scalable` and
/// `symbolic` deliberately do not, since neither holds a raster icon.
fn parse_size_directory(name: &str) -> Option<u32> {
    let head = name.split('@').next()?;
    let first = head.split('x').next()?;
    if first.is_empty() || !first.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let pixels: u32 = first.parse().ok()?;
    (pixels > 0).then_some(pixels)
}

/// Ordering key for picking a size: exact wins, then the smallest size larger
/// than wanted, then the largest smaller one.
fn size_rank(size: u32, wanted: u32) -> (u8, u32) {
    if size >= wanted {
        (0, size - wanted)
    } else {
        (1, wanted - size)
    }
}

fn is_supported_raster(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(|extension| {
            RASTER_EXTENSIONS
                .iter()
                .any(|supported| extension.eq_ignore_ascii_case(supported))
        })
        && path.is_file()
}

/// True when `value` can be appended beneath a search root without changing
/// directories. App ids, relative icon names and theme names are identifiers,
/// not relative paths.
fn is_single_path_component(value: &str) -> bool {
    let mut components = Path::new(value).components();
    matches!(
        components.next(),
        Some(Component::Normal(component)) if component == OsStr::new(value)
    ) && components.next().is_none()
}

/// Desktop-entry file names an application identity may be filed under.
///
/// Identities arrive in whatever case the toolkit used (`Alacritty`,
/// `code`, `com.anthropic.Claude`), while entries are conventionally
/// lower-case and reverse-DNS. Both spellings are cheap to try before falling
/// back to reading every entry in the directory.
fn entry_file_candidates(app_id: &str) -> Vec<String> {
    let mut candidates = vec![format!("{app_id}.desktop")];
    let lowered = app_id.to_lowercase();
    if lowered != app_id {
        candidates.push(format!("{lowered}.desktop"));
    }
    // `org.kde.konsole` is also filed as `org.kde.konsole.desktop`, but a
    // Wayland app-id like `firefox-esr` may be `firefox_esr` on disk.
    let dashed = lowered.replace('_', "-");
    if dashed != lowered {
        candidates.push(format!("{dashed}.desktop"));
    }
    candidates
}

/// Value of `Icon=` in the `[Desktop Entry]` group.
#[must_use]
pub fn desktop_entry_icon(text: &str) -> Option<String> {
    desktop_entry_value(text, "Icon")
}

/// Value of `StartupWMClass=` in the `[Desktop Entry]` group.
#[must_use]
pub fn desktop_entry_startup_wm_class(text: &str) -> Option<String> {
    desktop_entry_value(text, "StartupWMClass")
}

/// Read one key out of the entry's main group.
///
/// Only the `[Desktop Entry]` group is considered: an action group further
/// down the file describes a right-click menu item, and taking its `Icon` for
/// the application's own would be wrong rather than merely imprecise.
fn desktop_entry_value(text: &str, key: &str) -> Option<String> {
    let mut in_main_group = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_main_group = line == "[Desktop Entry]";
            continue;
        }
        if !in_main_group || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        // Localized keys (`Icon[de]`) never carry a different icon, and taking
        // one would depend on the reader's locale; only the plain key counts.
        if name.trim() == key {
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_owned());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch tree, removed when the test ends.
    struct Tree(PathBuf);

    impl Tree {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "xbar_app_icon_{}_{}_{name}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&path).expect("scratch tree");
            Self(path)
        }

        fn file(&self, relative: &str, contents: &str) -> PathBuf {
            let path = self.0.join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("scratch directory");
            std::fs::write(&path, contents).expect("scratch file");
            path
        }

        fn paths(&self) -> IconSearchPaths {
            IconSearchPaths::new(
                vec![self.0.join("applications")],
                vec![self.0.join("icons")],
                vec![self.0.join("pixmaps")],
                Vec::new(),
            )
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const ENTRY: &str = "\
[Desktop Entry]
Name=Editor
Icon=vscode
StartupWMClass=Code
Exec=code

[Desktop Action new-window]
Name=New Window
Icon=vscode-new-window
";

    #[test]
    fn only_the_main_group_supplies_the_icon() {
        assert_eq!(desktop_entry_icon(ENTRY).as_deref(), Some("vscode"));
        assert_eq!(
            desktop_entry_startup_wm_class(ENTRY).as_deref(),
            Some("Code")
        );
        assert_eq!(desktop_entry_icon("[Desktop Action x]\nIcon=nope\n"), None);
        assert_eq!(desktop_entry_icon("[Desktop Entry]\nIcon=\n"), None);
        assert_eq!(
            desktop_entry_icon("[Desktop Entry]\nIcon[de]=lokal\nIcon=plain\n").as_deref(),
            Some("plain")
        );
    }

    #[test]
    fn size_directories_are_recognized_in_the_shapes_themes_actually_use() {
        assert_eq!(parse_size_directory("48x48"), Some(48));
        assert_eq!(parse_size_directory("48x48@2x"), Some(48));
        assert_eq!(parse_size_directory("64"), Some(64));
        assert_eq!(parse_size_directory("scalable"), None);
        assert_eq!(parse_size_directory("symbolic"), None);
        assert_eq!(parse_size_directory(""), None);
    }

    #[test]
    fn the_closest_size_at_or_above_the_bar_height_wins() {
        assert!(size_rank(24, 24) < size_rank(32, 24));
        assert!(size_rank(32, 24) < size_rank(16, 24));
        assert!(size_rank(16, 24) < size_rank(8, 24));
    }

    #[test]
    fn an_entry_named_after_the_class_resolves_to_its_themed_icon() {
        let tree = Tree::new("entry");
        tree.file("applications/code.desktop", ENTRY);
        tree.file("icons/hicolor/16x16/apps/vscode.png", "small");
        let wanted = tree.file("icons/hicolor/32x32/apps/vscode.png", "large");
        tree.file("icons/hicolor/scalable/apps/vscode.svg", "vector");

        let mut resolver = AppIconResolver::with_paths(tree.paths(), 24);
        assert_eq!(
            resolver.resolve("code").map(|icon| icon.path.clone()),
            Some(wanted)
        );
    }

    #[test]
    fn a_window_class_is_matched_against_startup_wm_class_when_nothing_is_named_for_it() {
        let tree = Tree::new("wmclass");
        tree.file("applications/code.desktop", ENTRY);
        let wanted = tree.file("icons/hicolor/32x32/apps/vscode.png", "icon");

        let mut resolver = AppIconResolver::with_paths(tree.paths(), 32);
        // "Code" names no file; only StartupWMClass connects it to the entry.
        assert_eq!(
            resolver.resolve("Code").map(|icon| icon.path.clone()),
            Some(wanted)
        );
    }

    #[test]
    fn an_application_without_an_entry_falls_back_to_its_own_name() {
        let tree = Tree::new("noentry");
        let wanted = tree.file("pixmaps/weird-app.png", "icon");

        let mut resolver = AppIconResolver::with_paths(tree.paths(), 24);
        assert_eq!(
            resolver.resolve("weird-app").map(|icon| icon.path.clone()),
            Some(wanted)
        );
    }

    #[test]
    fn a_scalable_only_application_resolves_to_no_icon_rather_than_an_undrawable_file() {
        let tree = Tree::new("scalable");
        tree.file(
            "applications/vector.desktop",
            "[Desktop Entry]\nIcon=vector\n",
        );
        tree.file("icons/hicolor/scalable/apps/vector.svg", "vector");

        let mut resolver = AppIconResolver::with_paths(tree.paths(), 24);
        assert_eq!(resolver.resolve("vector"), None);
    }

    #[test]
    fn resolution_is_cached_including_the_answer_that_costs_a_directory_walk() {
        let tree = Tree::new("cache");
        tree.file("applications/code.desktop", ENTRY);
        let mut resolver = AppIconResolver::with_paths(tree.paths(), 24);
        assert_eq!(resolver.resolve("ghost"), None);

        // The icon appears only after the miss was cached, so a second call
        // still reports nothing — and a cleared cache picks it up.
        let wanted = tree.file("pixmaps/ghost.png", "icon");
        assert_eq!(resolver.resolve("ghost"), None);
        resolver.clear();
        assert_eq!(
            resolver.resolve("ghost").map(|icon| icon.path.clone()),
            Some(wanted)
        );
    }

    #[test]
    fn blank_identities_never_reach_the_filesystem() {
        let mut resolver = AppIconResolver::with_paths(
            IconSearchPaths::new(Vec::new(), Vec::new(), Vec::new(), Vec::new()),
            24,
        );
        assert_eq!(resolver.resolve(""), None);
        assert_eq!(resolver.resolve("   "), None);
    }

    #[test]
    fn application_id_cannot_escape_the_applications_directory() {
        let tree = Tree::new("identity_traversal");
        tree.file("escape.desktop", "[Desktop Entry]\nIcon=escaped\n");
        tree.file("pixmaps/escaped.png", "icon");

        let mut resolver = AppIconResolver::with_paths(tree.paths(), 24);
        assert_eq!(resolver.resolve("../escape"), None);
        assert_eq!(resolver.resolve("subdir/escape"), None);
        assert_eq!(resolver.resolve("escape/"), None);
    }

    #[test]
    fn relative_icon_name_cannot_escape_an_icon_root() {
        let tree = Tree::new("icon_traversal");
        tree.file(
            "applications/unsafe.desktop",
            "[Desktop Entry]\nIcon=../outside\n",
        );
        tree.file("outside.png", "icon");

        let mut resolver = AppIconResolver::with_paths(tree.paths(), 24);
        assert_eq!(resolver.resolve("unsafe"), None);
    }

    #[test]
    fn oversized_desktop_entries_are_not_read_into_the_bar() {
        let tree = Tree::new("oversized_entry");
        let mut entry = String::from("[Desktop Entry]\nIcon=secret\n");
        entry.push_str(&" ".repeat(MAX_DESKTOP_ENTRY_BYTES));
        tree.file("applications/huge.desktop", &entry);
        tree.file("pixmaps/secret.png", "icon");

        let mut resolver = AppIconResolver::with_paths(tree.paths(), 24);
        assert_eq!(resolver.resolve("huge"), None);
    }

    #[cfg(unix)]
    #[test]
    fn non_regular_desktop_entries_are_rejected_before_reading() {
        let tree = Tree::new("special_entry");
        let path = tree.0.join("applications/socket.desktop");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut budget = MAX_DESKTOP_ENTRY_BYTES;

        assert_eq!(read_desktop_entry_with_budget(&path, &mut budget), None);
        drop(listener);
    }

    #[test]
    fn application_root_search_has_one_global_bound() {
        let tree = Tree::new("bounded_roots");
        let applications: Vec<_> = (0..=MAX_ICON_SEARCH_ROOTS)
            .map(|index| tree.0.join(format!("applications-{index}")))
            .collect();
        let ignored = applications.last().unwrap().join("bounded.desktop");
        std::fs::create_dir_all(ignored.parent().unwrap()).unwrap();
        std::fs::write(&ignored, "[Desktop Entry]\nIcon=secret\n").unwrap();
        let wanted = tree.file("pixmaps/secret.png", "icon");
        let paths = IconSearchPaths::new(
            applications,
            Vec::new(),
            vec![tree.0.join("pixmaps")],
            Vec::new(),
        );

        let mut resolver = AppIconResolver::with_paths(paths, 24);
        assert_eq!(resolver.resolve("bounded"), None);
        assert!(wanted.is_file(), "the ignored entry's icon must exist");
    }

    #[test]
    fn the_same_file_always_carries_the_same_renderer_cache_key() {
        let first = AppIcon::new(PathBuf::from("/usr/share/icons/a.png"));
        let second = AppIcon::new(PathBuf::from("/usr/share/icons/a.png"));
        let other = AppIcon::new(PathBuf::from("/usr/share/icons/b.png"));
        assert_eq!(first.key, second.key);
        assert_ne!(first.key, other.key);
    }
}
