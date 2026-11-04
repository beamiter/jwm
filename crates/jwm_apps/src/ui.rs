//! Shared Cairo/Pango chrome. Like jwm-shell, rendering never needs GTK.
use cairo::{Context, LineCap, LineJoin};

pub type Color = (f64, f64, f64);
thread_local! {
    static THEME: std::cell::RefCell<crate::theme::NativeTheme> = std::cell::RefCell::new(crate::theme::NativeTheme::default());
}
pub(crate) fn apply_theme(theme: crate::theme::NativeTheme) -> bool {
    THEME.with(|current| {
        if *current.borrow() == theme {
            return false;
        }
        *current.borrow_mut() = theme;
        true
    })
}
pub fn canvas() -> Color {
    THEME.with(|theme| theme.borrow().colors[0])
}
pub fn panel() -> Color {
    THEME.with(|theme| theme.borrow().colors[1])
}
pub fn hover() -> Color {
    THEME.with(|theme| theme.borrow().colors[2])
}
pub fn line() -> Color {
    THEME.with(|theme| theme.borrow().colors[3])
}
pub fn text_color() -> Color {
    THEME.with(|theme| theme.borrow().colors[4])
}
pub fn muted() -> Color {
    THEME.with(|theme| theme.borrow().colors[5])
}
pub fn accent() -> Color {
    THEME.with(|theme| theme.borrow().colors[6])
}
pub fn select() -> Color {
    THEME.with(|theme| theme.borrow().colors[7])
}
pub fn density() -> f64 {
    THEME.with(|theme| theme.borrow().density)
}
fn font_scale(font: &pango::FontDescription) -> f64 {
    let points = f64::from(font.size()) / f64::from(pango::SCALE);
    if points.is_finite() && (6.0..=48.0).contains(&points) {
        points / 11.0
    } else {
        1.0
    }
}
fn chrome_scale() -> f64 {
    THEME.with(|theme| font_scale(&pango::FontDescription::from_string(&theme.borrow().font)))
}
pub fn header() -> f64 {
    (64.0 * density()).max(32.0 * chrome_scale() + 16.0)
}
pub fn tools() -> f64 {
    (44.0 * density()).max(28.0 * chrome_scale() + 8.0)
}
pub fn status() -> f64 {
    (32.0 * density()).max(14.0 * chrome_scale() + 10.0)
}
#[derive(Clone, Copy, Default)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}
impl Rect {
    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }
    pub fn contains(self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}
pub fn color(cr: &Context, c: Color) {
    cr.set_source_rgb(c.0, c.1, c.2);
}
pub fn fill(cr: &Context, r: Rect, c: Color) {
    color(cr, c);
    cr.rectangle(r.x, r.y, r.w, r.h);
    let _ = cr.fill();
}
pub fn rounded(cr: &Context, r: Rect, c: Color) {
    color(cr, c);
    cr.new_sub_path();
    let radius = THEME
        .with(|theme| theme.borrow().radius * theme.borrow().density)
        .min(r.w.max(0.0) / 2.)
        .min(r.h.max(0.0) / 2.);
    for (x, y, a) in [
        (r.x + r.w - radius, r.y + radius, -90.),
        (r.x + r.w - radius, r.y + r.h - radius, 0.),
        (r.x + radius, r.y + r.h - radius, 90.),
        (r.x + radius, r.y + radius, 180.),
    ] {
        cr.arc(
            x,
            y,
            radius,
            a * std::f64::consts::PI / 180.,
            (a + 90.) * std::f64::consts::PI / 180.,
        );
    }
    cr.close_path();
    let _ = cr.fill();
}
pub fn layout(cr: &Context, text: &str, size: f64, width: f64, wrap: bool) -> pango::Layout {
    let layout = pangocairo::functions::create_layout(cr);
    let mut font = THEME.with(|theme| pango::FontDescription::from_string(&theme.borrow().font));
    // Component sizes are authored against the shared 11-point default.
    // Retain their hierarchy while inheriting a user's large-text size.
    let scale = font_scale(&font);
    font.set_absolute_size(size * scale * pango::SCALE as f64);
    layout.set_font_description(Some(&font));
    layout.set_text(text);
    layout.set_width((width.max(1.) * pango::SCALE as f64) as i32);
    if wrap {
        layout.set_wrap(pango::WrapMode::WordChar);
        layout.set_spacing(4 * pango::SCALE);
    } else {
        layout.set_single_paragraph_mode(true);
        layout.set_ellipsize(pango::EllipsizeMode::End);
    }
    layout
}
pub fn text(cr: &Context, text: &str, r: Rect, size: f64, c: Color) {
    let layout = layout(cr, text, size, r.w, false);
    color(cr, c);
    cr.move_to(r.x, r.y + (r.h - layout.pixel_size().1 as f64) / 2.);
    pangocairo::functions::show_layout(cr, &layout);
}
#[derive(Clone, Copy)]
pub(crate) enum Icon {
    Notes,
    Image,
    New,
    Folder,
    Save,
    Export,
    Trash,
    Previous,
    Next,
    ZoomIn,
    ZoomOut,
    Actual,
    Fit,
    Rotate,
    Fullscreen,
    Minimize,
    Maximize,
    Restore,
    Close,
}

pub fn icon(cr: &Context, kind: Icon, x: f64, y: f64) {
    let _ = cr.save();
    color(cr, text_color());
    cr.translate(x, y);
    cr.scale(0.8, 0.8);
    cr.set_line_width(1.5);
    cr.set_line_cap(LineCap::Round);
    cr.set_line_join(LineJoin::Round);
    let path = |points: &[(f64, f64)]| {
        cr.move_to(points[0].0, points[0].1);
        for &(x, y) in &points[1..] {
            cr.line_to(x, y);
        }
    };
    let rect = |x, y, w, h| cr.rectangle(x, y, w, h);
    match kind {
        Icon::Notes | Icon::New => {
            path(&[
                (6., 3.),
                (15., 3.),
                (19., 7.),
                (19., 21.),
                (6., 21.),
                (6., 3.),
            ]);
            path(&[(15., 3.), (15., 7.), (19., 7.)]);
            path(&[(9., 12.), (16., 12.)]);
            if matches!(kind, Icon::New) {
                path(&[(12.5, 8.5), (12.5, 15.5)]);
            } else {
                path(&[(9., 16.), (16., 16.)]);
            }
        }
        Icon::Image => {
            rect(3., 4., 18., 16.);
            path(&[(3., 17.), (9., 11.), (13., 15.), (16., 12.), (21., 17.)]);
            cr.new_sub_path();
            cr.arc(16., 8., 1., 0., std::f64::consts::TAU);
        }
        // These paths match the player's folder and window-control icons.
        Icon::Folder => path(&[
            (3., 6.),
            (10., 6.),
            (12., 9.),
            (21., 9.),
            (21., 20.),
            (3., 20.),
            (3., 6.),
        ]),
        Icon::Save => {
            path(&[
                (4., 3.),
                (17., 3.),
                (21., 7.),
                (21., 21.),
                (4., 21.),
                (4., 3.),
            ]);
            rect(8., 3., 8., 6.);
            rect(8., 14., 9., 7.);
        }
        Icon::Export => {
            path(&[(4., 14.), (4., 21.), (20., 21.), (20., 14.)]);
            path(&[(12., 16.), (12., 3.)]);
            path(&[(7., 8.), (12., 3.), (17., 8.)]);
        }
        Icon::Trash => {
            path(&[(3., 6.), (21., 6.)]);
            path(&[(5., 6.), (6., 21.), (18., 21.), (19., 6.)]);
            path(&[(9., 6.), (9., 3.), (15., 3.), (15., 6.)]);
            path(&[(10., 10.), (10., 17.)]);
            path(&[(14., 10.), (14., 17.)]);
        }
        Icon::Previous => path(&[(15., 5.), (8., 12.), (15., 19.)]),
        Icon::Next => path(&[(9., 5.), (16., 12.), (9., 19.)]),
        Icon::ZoomIn | Icon::ZoomOut => {
            cr.arc(10., 10., 6.5, 0., std::f64::consts::TAU);
            path(&[(15., 15.), (21., 21.)]);
            path(&[(7., 10.), (13., 10.)]);
            if matches!(kind, Icon::ZoomIn) {
                path(&[(10., 7.), (10., 13.)]);
            }
        }
        Icon::Actual => {
            path(&[(3., 9.), (6., 6.), (6., 18.)]);
            path(&[(15., 9.), (18., 6.), (18., 18.)]);
            path(&[(11., 9.), (12., 9.)]);
            path(&[(11., 15.), (12., 15.)]);
        }
        Icon::Fit | Icon::Fullscreen => {
            path(&[(3., 9.), (3., 3.), (9., 3.)]);
            path(&[(15., 3.), (21., 3.), (21., 9.)]);
            path(&[(21., 15.), (21., 21.), (15., 21.)]);
            path(&[(9., 21.), (3., 21.), (3., 15.)]);
            if matches!(kind, Icon::Fit) {
                rect(8., 8., 8., 8.);
            }
        }
        Icon::Rotate => {
            cr.arc(12., 12., 8., 0., std::f64::consts::PI * 1.5);
            path(&[(8., 1.), (12., 4.), (8., 7.)]);
        }
        Icon::Minimize => path(&[(5., 12.), (19., 12.)]),
        Icon::Maximize => rect(5., 5., 14., 14.),
        Icon::Restore => {
            rect(5., 7., 12., 12.);
            path(&[(9., 3.), (21., 3.), (21., 15.)]);
        }
        Icon::Close => {
            path(&[(6., 6.), (18., 18.)]);
            path(&[(18., 6.), (6., 18.)]);
        }
    }
    let _ = cr.stroke();
    let _ = cr.restore();
}
