/// A point in an annotation stroke.
#[derive(Clone, Copy)]
pub struct AnnotationPoint {
    pub x: f32,
    pub y: f32,
}

/// A single annotation stroke (line segment sequence).
pub struct AnnotationStroke {
    pub points: Vec<AnnotationPoint>,
    pub color: [f32; 4],
    pub width: f32,
}
