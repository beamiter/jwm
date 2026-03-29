// ---------------------------------------------------------------------------
// Matrix math utilities for OpenGL (column-major)
// ---------------------------------------------------------------------------

/// Orthographic projection matrix (column-major for OpenGL).
pub fn ortho(left: f32, right: f32, bottom: f32, top: f32, near: f32, far: f32) -> [f32; 16] {
    let tx = -(right + left) / (right - left);
    let ty = -(top + bottom) / (top - bottom);
    let tz = -(far + near) / (far - near);
    #[rustfmt::skip]
    let m = [
        2.0 / (right - left), 0.0,                  0.0,                 0.0,
        0.0,                  2.0 / (top - bottom),  0.0,                 0.0,
        0.0,                  0.0,                  -2.0 / (far - near),  0.0,
        tx,                   ty,                    tz,                  1.0,
    ];
    m
}

/// Perspective projection matrix.
pub fn perspective_matrix(fov_y: f32, aspect: f32, near: f32, far: f32) -> [f32; 16] {
    let f = 1.0 / (fov_y * 0.5).tan();
    #[rustfmt::skip]
    let m = [
        f / aspect, 0.0, 0.0,                              0.0,
        0.0,        f,   0.0,                              0.0,
        0.0,        0.0, (far + near) / (near - far),     -1.0,
        0.0,        0.0, (2.0 * far * near) / (near - far), 0.0,
    ];
    m
}

/// Translation matrix.
pub fn translate_matrix(x: f32, y: f32, z: f32) -> [f32; 16] {
    #[rustfmt::skip]
    let m = [
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        x,   y,   z,   1.0,
    ];
    m
}

/// Rotation around the Y axis.
pub fn rotate_y_matrix(angle: f32) -> [f32; 16] {
    let c = angle.cos();
    let s = angle.sin();
    #[rustfmt::skip]
    let m = [
         c,  0.0, -s,  0.0,
        0.0, 1.0, 0.0, 0.0,
         s,  0.0,  c,  0.0,
        0.0, 0.0, 0.0, 1.0,
    ];
    m
}

/// Rotation around the X axis.
#[allow(dead_code)]
pub fn rotate_x_matrix(angle: f32) -> [f32; 16] {
    let c = angle.cos();
    let s = angle.sin();
    #[rustfmt::skip]
    let m = [
        1.0, 0.0, 0.0, 0.0,
        0.0,  c,   s,  0.0,
        0.0, -s,   c,  0.0,
        0.0, 0.0, 0.0, 1.0,
    ];
    m
}

/// Uniform scale matrix.
pub fn scale_matrix(sx: f32, sy: f32, sz: f32) -> [f32; 16] {
    #[rustfmt::skip]
    let m = [
        sx,  0.0, 0.0, 0.0,
        0.0, sy,  0.0, 0.0,
        0.0, 0.0, sz,  0.0,
        0.0, 0.0, 0.0, 1.0,
    ];
    m
}

/// 4×4 matrix multiply (column-major).
pub fn mat4_mul(a: &[f32; 16], b: &[f32; 16]) -> [f32; 16] {
    let mut m = [0.0f32; 16];
    for col in 0..4 {
        for row in 0..4 {
            m[col * 4 + row] = (0..4)
                .map(|k| a[k * 4 + row] * b[col * 4 + k])
                .sum();
        }
    }
    m
}
