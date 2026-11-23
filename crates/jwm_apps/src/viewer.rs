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

// Only worker-prepared native-endian pixels may cross into the Cairo path.
struct PreparedImage(DecodedImage);

type LoadResult = (Result<PreparedImage, String>, Option<PathBuf>, u64);
type ScanResult = (Vec<PathBuf>, PathBuf, u64);
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
    scanner: Option<Receiver<ScanResult>>,
    pending_scan: Option<(PathBuf, u64)>,
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
            scanner: None,
            pending_scan: None,
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
        self.pending_scan = None;
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
            let image = decode(&path).map(prepare_surface_pixels);
            let _ = tx.send((image, scan.then_some(path), generation));
        });
    }
    fn start_scan(&mut self) {
        if self.scanner.is_some() {
            return;
        }
        let Some((path, generation)) = self.pending_scan.take() else {
            return;
        };
        let (tx, rx) = mpsc::channel();
        self.scanner = Some(rx);
        std::thread::spawn(move || {
            let entries = neighbors(&path);
            let _ = tx.send((entries, path, generation));
        });
    }
    pub fn loading(&self) -> bool {
        self.worker.is_some() || self.scanner.is_some()
    }
    pub fn poll(&mut self) -> bool {
        let decoded = self.poll_decode();
        self.poll_scan() || decoded
    }
    fn poll_decode(&mut self) -> bool {
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
        let (decoded, scan, generation) = result;
        if generation != self.generation {
            self.start();
            return true;
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
        // Publish the requested image (or its error) before discovering
        // neighbors. An older directory scan cannot block the decoder.
        self.pending_scan = scan.map(|path| (path, generation));
        self.start_scan();
        true
    }
    fn poll_scan(&mut self) -> bool {
        let Some(scanner) = &self.scanner else {
            return false;
        };
        let result = match scanner.try_recv() {
            Err(mpsc::TryRecvError::Empty) => return false,
            result => result,
        };
        self.scanner = None;
        let mut changed = false;
        if let Ok((paths, path, generation)) = result
            && generation == self.generation
            && let Some(index) = paths.iter().position(|candidate| candidate == &path)
        {
            self.paths = paths;
            self.index = index;
            changed = true;
        }
        self.start_scan();
        changed
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
fn prepare_surface_pixels(mut decoded: DecodedImage) -> PreparedImage {
    for p in decoded.pixels.as_chunks_mut::<4>().0 {
        let a = u32::from(p[3]);
        let value = (a << 24)
            | ((u32::from(p[0]) * a / 255) << 16)
            | ((u32::from(p[1]) * a / 255) << 8)
            | (u32::from(p[2]) * a / 255);
        p.copy_from_slice(&value.to_ne_bytes());
    }
    PreparedImage(decoded)
}

fn surface(PreparedImage(decoded): PreparedImage) -> Result<ImageSurface, String> {
    ImageSurface::create_for_data(
        decoded.pixels,
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

    fn deliver_decoded(viewer: &mut Viewer, width: u32, scan: Option<PathBuf>) {
        let (tx, rx) = mpsc::channel();
        assert!(
            tx.send((
                Ok(prepare_surface_pixels(DecodedImage {
                    pixels: vec![255; width as usize * 4],
                    width,
                    height: 1,
                })),
                scan,
                viewer.generation,
            ))
            .is_ok()
        );
        viewer.worker = Some(rx);
    }

    #[test]
    fn viewer_publishes_decoded_image_while_directory_scan_is_pending() {
        let mut viewer = Viewer::new(Vec::new());
        let selected = PathBuf::from("selected.png");
        let sibling = PathBuf::from("earlier.png");
        viewer.paths = vec![selected.clone()];
        let (scan_tx, scan_rx) = mpsc::channel();
        viewer.scanner = Some(scan_rx);
        deliver_decoded(&mut viewer, 3, None);

        assert!(viewer.poll());
        assert_eq!(viewer.dimensions(), (3., 1.));
        assert_eq!(viewer.paths.as_slice(), std::slice::from_ref(&selected));
        assert!(viewer.loading());

        viewer.rotation = 1;
        scan_tx
            .send((
                vec![sibling.clone(), selected.clone()],
                selected.clone(),
                viewer.generation,
            ))
            .unwrap();
        assert!(viewer.poll());
        assert_eq!(viewer.paths, [sibling, selected]);
        assert_eq!(viewer.index, 1);
        assert_eq!(viewer.dimensions(), (1., 3.));
        assert!(!viewer.loading());
    }

    #[test]
    fn viewer_ignores_older_scan_after_opening_an_explicit_file_list() {
        let mut viewer = Viewer::new(Vec::new());
        let old = PathBuf::from("old.png");
        let first = PathBuf::from("new-1.png");
        let second = PathBuf::from("new-2.png");
        let (scan_tx, scan_rx) = mpsc::channel();
        viewer.scanner = Some(scan_rx);

        // Hold the decoder lane so open queues work without touching disk.
        let (old_tx, old_rx) = mpsc::channel();
        viewer.worker = Some(old_rx);
        let old_generation = viewer.generation;
        viewer.open(vec![first.clone(), second.clone()]);
        let (path, scan, generation) = viewer.pending.take().unwrap();
        assert_eq!(path, first);
        assert!(!scan);
        assert_ne!(generation, old_generation);
        drop(old_tx);
        deliver_decoded(&mut viewer, 4, None);
        assert!(viewer.poll());
        assert_eq!(viewer.dimensions(), (4., 1.));

        scan_tx
            .send((vec![old.clone()], old, old_generation))
            .unwrap();
        assert!(!viewer.poll());
        assert_eq!(viewer.paths, [first, second]);
        assert_eq!(viewer.index, 0);
        assert_eq!(viewer.dimensions(), (4., 1.));
        assert!(!viewer.loading());
    }

    #[test]
    fn worker_prepares_native_cairo_pixels_before_surface_creation() {
        let path =
            std::env::temp_dir().join(format!("jwm-viewer-cairo-{}.png", std::process::id()));
        let pixels = vec![10, 20, 30, 0, 100, 200, 255, 128, 20, 40, 60, 255];
        image::RgbaImage::from_raw(3, 1, pixels)
            .unwrap()
            .save(&path)
            .unwrap();
        let mut viewer = Viewer::new(Vec::new());
        viewer.paths.push(path.clone());
        viewer.load(true);
        let (prepared, scan, generation) = viewer
            .worker
            .take()
            .unwrap()
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(generation, viewer.generation);
        assert_eq!(scan, Some(path));
        let mut cairo = surface(prepared.unwrap()).unwrap();
        assert_eq!((cairo.width(), cairo.height(), cairo.stride()), (3, 1, 12));
        let expected = [0x0000_0000u32, 0x8032_6480, 0xff14_283c]
            .into_iter()
            .flat_map(u32::to_ne_bytes)
            .collect::<Vec<_>>();
        assert_eq!(&*cairo.data().unwrap(), expected.as_slice());
    }

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
