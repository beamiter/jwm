//! The default desktop's cool glass family, shared by permanent edge controls
//! and compositor-owned menus. These are sRGB colors with straight alpha;
//! renderers remain responsible for their own backdrop and blend domain.

pub type Color = [f32; 4];

#[derive(Clone, Copy, Debug)]
pub struct ShellPalette {
    pub ink: Color,
    pub muted_ink: Color,
    pub accent: Color,
    pub selection: Color,
    /// Text-heavy modal surfaces need a denser veil than the narrow edges.
    pub panel: Color,
    pub sheet_top: Color,
    pub sheet_bottom: Color,
    pub control_top: Color,
    pub control_bottom: Color,
    pub field: Color,
    pub hover: Color,
    pub pressed: Color,
    pub rim: Color,
    pub divider: Color,
    pub track: Color,
}

impl ShellPalette {
    pub const LIGHT: Self = Self {
        ink: [0.06, 0.15, 0.26, 1.0],
        muted_ink: [0.13, 0.18, 0.25, 1.0],
        accent: [0.08, 0.43, 0.94, 1.0],
        selection: [0.03, 0.39, 0.95, 1.0],
        panel: [0.94, 0.98, 1.0, 0.64],
        sheet_top: [0.80, 0.92, 1.0, 0.64],
        sheet_bottom: [0.72, 0.86, 1.0, 0.56],
        control_top: [1.0, 1.0, 1.0, 0.57],
        control_bottom: [0.80, 0.91, 1.0, 0.34],
        field: [0.96, 0.99, 1.0, 0.50],
        hover: [0.30, 0.62, 1.0, 0.16],
        pressed: [0.08, 0.43, 0.94, 0.24],
        rim: [1.0, 1.0, 1.0, 0.48],
        divider: [0.14, 0.38, 0.64, 0.22],
        track: [0.06, 0.16, 0.29, 0.14],
    };

    pub const DARK: Self = Self {
        ink: [0.93, 0.95, 0.99, 1.0],
        muted_ink: [0.85, 0.88, 0.94, 1.0],
        accent: [0.08, 0.43, 0.94, 1.0],
        selection: [0.03, 0.39, 0.95, 1.0],
        panel: [0.06, 0.13, 0.23, 0.79],
        sheet_top: [0.09, 0.18, 0.30, 0.84],
        sheet_bottom: [0.05, 0.12, 0.22, 0.77],
        control_top: [0.14, 0.21, 0.31, 0.92],
        control_bottom: [0.11, 0.18, 0.28, 0.90],
        field: [0.11, 0.18, 0.28, 0.90],
        hover: [0.48, 0.70, 1.0, 0.14],
        pressed: [0.02, 0.08, 0.18, 0.38],
        rim: [0.78, 0.89, 1.0, 0.22],
        divider: [0.65, 0.80, 1.0, 0.22],
        track: [0.01, 0.04, 0.10, 0.46],
    };

    pub const fn for_dark(dark: bool) -> Self {
        if dark { Self::DARK } else { Self::LIGHT }
    }

    pub const fn ink8(color: Color) -> [u8; 4] {
        [
            (color[0] * 255.0 + 0.5) as u8,
            (color[1] * 255.0 + 0.5) as u8,
            (color[2] * 255.0 + 0.5) as u8,
            (color[3] * 255.0 + 0.5) as u8,
        ]
    }
}
