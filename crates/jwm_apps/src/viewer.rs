use crate::neighbors;
use cairo::{Format, ImageSurface};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
};

pub struct DecodedImage {
    pub pixels: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Decoding and directory scanning run off the UI thread. Cap allocations so
/// a malformed image cannot exhaust the desktop's memory.
pub fn decode(path: &Path) -> Result<DecodedImage, String> {
    use image::ImageDecoder;
    let mut reader = image::ImageReader::open(path)
        .map_err(|error| error.to_string())?
        .with_guessed_format()
        .map_err(|error| error.to_string())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(32768);
    limits.max_image_height = Some(32768);
    limits.max_alloc = Some(256 * 1024 * 1024);
    reader.limits(limits);
    let mut decoder = reader.into_decoder().map_err(|error| error.to_string())?;
    let (width, height) = decoder.dimensions();
    if u64::from(width) * u64::from(height) > 64 * 1024 * 1024 {
        return Err("图片超过 6400 万像素，请选择较小的图片。".into());
    }
    let orientation = decoder.orientation().map_err(|error| error.to_string())?;
    let mut image =
        image::DynamicImage::from_decoder(decoder).map_err(|error| error.to_string())?;
    image.apply_orientation(orientation);
    let rgba = image.into_rgba8();
    Ok(DecodedImage {
        width: rgba.width(),
        height: rgba.height(),
        pixels: rgba.into_raw(),
    })
}

type LoadResult = (
    Result<DecodedImage, String>,
    Option<Vec<PathBuf>>,
    PathBuf,
    u64,
);
pub(crate) struct Viewer {
    pub paths: Vec<PathBuf>,
    pub index: usize,
    pub image: Option<ImageSurface>,
    pub fit: bool,
    pub zoom: f64,
    pub rotation: u8,
    pub pan: (f64, f64),
    pub status: String,
    pub message: String,
    generation: u64,
    worker: Option<Receiver<LoadResult>>,
    pending: Option<(PathBuf, bool, u64)>,
}
impl Viewer {
    pub fn new(paths: Vec<PathBuf>) -> Self {
        let mut this = Self {
            paths: vec![],
            index: 0,
            image: None,
            fit: true,
            zoom: 1.,
            rotation: 0,
            pan: (0., 0.),
            status: String::new(),
            message: "打开或拖入图片\nCtrl+O 打开 · ← → 切换 · F 适应窗口 · F11 全屏".into(),
            generation: 0,
            worker: None,
            pending: None,
        };
        this.open(paths);
        this
    }
    pub fn open(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        self.paths = paths;
        self.index = 0;
        self.load(true);
    }
    pub fn title(&self) -> String {
        self.paths
            .get(self.index)
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "图片查看器".into())
    }
    pub fn navigate(&mut self, direction: isize) {
        if self.paths.len() > 1 {
            self.index =
                (self.index as isize + direction).rem_euclid(self.paths.len() as isize) as usize;
            self.load(false);
        }
    }
    fn load(&mut self, scan: bool) {
        let Some(path) = self.paths.get(self.index).cloned() else {
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        self.pending = Some((path.clone(), scan && self.paths.len() == 1, self.generation));
        self.image = None;
        self.message = "正在载入图片…".into();
        self.status = path.to_string_lossy().into_owned();
        self.start();
    }
    fn start(&mut self) {
        if self.worker.is_some() {
            return;
        }
        let Some((path, scan, generation)) = self.pending.take() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.worker = Some(rx);
        std::thread::spawn(move || {
            let entries = scan.then(|| neighbors(&path));
            let _ = tx.send((decode(&path), entries, path, generation));
        });
    }
    pub fn loading(&self) -> bool {
        self.worker.is_some()
    }
    pub fn poll(&mut self) -> bool {
        let Some(worker) = &self.worker else {
            return false;
        };
        let result = match worker.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return false,
            Err(_) => {
                self.worker = None;
                self.message = "图片解码失败".into();
                self.start();
                return true;
            }
        };
        self.worker = None;
        let (decoded, entries, path, generation) = result;
        if generation != self.generation {
            self.start();
            return true;
        }
        if let Some(paths) = entries {
            self.index = paths.iter().position(|p| p == &path).unwrap_or(0);
            self.paths = paths;
        }
        match decoded.and_then(surface) {
            Ok(image) => {
                self.image = Some(image);
                self.rotation = 0;
                self.fit = true;
                self.pan = (0., 0.);
            }
            Err(error) => {
                self.message = format!("无法打开图片\n{error}\n← → 切换或 Ctrl+O 打开其他图片");
                self.status = "载入失败".into();
            }
        }
        true
    }
    pub fn dimensions(&self) -> (f64, f64) {
        let Some(image) = &self.image else {
            return (1., 1.);
        };
        if self.rotation.is_multiple_of(2) {
            (image.width() as f64, image.height() as f64)
        } else {
            (image.height() as f64, image.width() as f64)
        }
    }
    pub fn scale(&self, width: f64, height: f64) -> f64 {
        let (w, h) = self.dimensions();
        if self.fit {
            (width / w).min(height / h)
        } else {
            self.zoom
        }
    }
    pub fn zoom_by(&mut self, factor: f64, width: f64, height: f64) {
        self.zoom = (self.scale(width, height) * factor).clamp(0.05, 16.);
        self.fit = false;
    }
}
fn surface(decoded: DecodedImage) -> Result<ImageSurface, String> {
    let mut pixels = decoded.pixels;
    for p in pixels.as_chunks_mut::<4>().0 {
        let a = u32::from(p[3]);
        let value = (a << 24)
            | ((u32::from(p[0]) * a / 255) << 16)
            | ((u32::from(p[1]) * a / 255) << 8)
            | (u32::from(p[2]) * a / 255);
        p.copy_from_slice(&value.to_ne_bytes());
    }
    ImageSurface::create_for_data(
        pixels,
        Format::ARgb32,
        decoded.width as i32,
        decoded.height as i32,
        decoded.width as i32 * 4,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_png_and_rejects_corruption_without_a_display() {
        let path = std::env::temp_dir().join(format!("jwm-viewer-test-{}.png", std::process::id()));
        image::RgbaImage::from_pixel(3, 2, image::Rgba([20, 40, 60, 255]))
            .save(&path)
            .unwrap();
        let decoded = decode(&path).unwrap();
        assert_eq!((decoded.width, decoded.height), (3, 2));
        assert_eq!(decoded.pixels.len(), 24);
        assert_eq!(&decoded.pixels[..4], &[20, 40, 60, 255]);
        std::fs::write(&path, b"not an image").unwrap();
        assert!(decode(&path).is_err());
        std::fs::remove_file(path).unwrap();
    }
}
