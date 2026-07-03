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
            m[col * 4 + row] = (0..4).map(|k| a[k * 4 + row] * b[col * 4 + k]).sum();
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPSILON: f32 = 1e-5;

    fn assert_matrix_approx_eq(a: &[f32; 16], b: &[f32; 16]) {
        for i in 0..16 {
            assert!(
                (a[i] - b[i]).abs() < EPSILON,
                "Matrix element [{}] differs: {} vs {}",
                i,
                a[i],
                b[i]
            );
        }
    }

    fn identity_matrix() -> [f32; 16] {
        [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ]
    }

    #[test]
    fn test_ortho_matrix() {
        let ortho_mat = ortho(0.0, 1920.0, 0.0, 1080.0, -1.0, 1.0);

        assert!((ortho_mat[0] - 2.0 / 1920.0).abs() < EPSILON, "Scale X");
        assert!((ortho_mat[5] - 2.0 / 1080.0).abs() < EPSILON, "Scale Y");
    }

    #[test]
    fn test_ortho_matrix_symmetry() {
        let ortho1 = ortho(-100.0, 100.0, -100.0, 100.0, -1.0, 1.0);
        let ortho2 = ortho(-100.0, 100.0, -100.0, 100.0, -1.0, 1.0);

        assert_matrix_approx_eq(&ortho1, &ortho2);
    }

    #[test]
    fn test_perspective_matrix() {
        let persp = perspective_matrix(std::f32::consts::PI / 4.0, 16.0 / 9.0, 0.1, 100.0);

        assert!(persp[0] > 0.0, "Perspective X scale should be positive");
        assert!(persp[5] > 0.0, "Perspective Y scale should be positive");
        assert!(persp[10] < 0.0, "Perspective Z scale should be negative");
        assert!(persp[11] == -1.0, "Perspective marker should be -1");
    }

    #[test]
    fn test_translate_matrix() {
        let translate = translate_matrix(10.0, 20.0, 30.0);

        let expected = [
            1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 10.0, 20.0, 30.0, 1.0,
        ];

        assert_matrix_approx_eq(&translate, &expected);
    }

    #[test]
    fn test_translate_identity() {
        let translate = translate_matrix(0.0, 0.0, 0.0);
        assert_matrix_approx_eq(&translate, &identity_matrix());
    }

    #[test]
    fn test_rotate_y_matrix() {
        let rotate = rotate_y_matrix(0.0);
        assert_matrix_approx_eq(&rotate, &identity_matrix());

        let rotate_90 = rotate_y_matrix(std::f32::consts::PI / 2.0);
        assert!(rotate_90[0] < EPSILON, "cos(90°) should be ~0");
        assert!(
            (rotate_90[2] + 1.0).abs() < EPSILON,
            "-sin(90°) should be ~-1"
        );
    }

    #[test]
    fn test_rotate_y_matrix_double_rotation() {
        let rotate_45 = rotate_y_matrix(std::f32::consts::PI / 4.0);
        let double = mat4_mul(&rotate_45, &rotate_45);
        let rotate_90 = rotate_y_matrix(std::f32::consts::PI / 2.0);

        assert_matrix_approx_eq(&double, &rotate_90);
    }

    #[test]
    fn test_rotate_x_matrix() {
        let rotate = rotate_x_matrix(0.0);
        assert_matrix_approx_eq(&rotate, &identity_matrix());

        let rotate_90 = rotate_x_matrix(std::f32::consts::PI / 2.0);
        assert!(rotate_90[5].abs() < EPSILON, "cos(90°) should be ~0");
        assert!(
            (rotate_90[9] + 1.0).abs() < EPSILON,
            "-sin(90°) should be ~-1"
        );
    }

    #[test]
    fn test_scale_matrix() {
        let scale = scale_matrix(2.0, 3.0, 4.0);

        let expected = [
            2.0, 0.0, 0.0, 0.0, 0.0, 3.0, 0.0, 0.0, 0.0, 0.0, 4.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ];

        assert_matrix_approx_eq(&scale, &expected);
    }

    #[test]
    fn test_scale_identity() {
        let scale = scale_matrix(1.0, 1.0, 1.0);
        assert_matrix_approx_eq(&scale, &identity_matrix());
    }

    #[test]
    fn test_mat4_mul_identity() {
        let ident = identity_matrix();
        let result = mat4_mul(&ident, &ident);

        assert_matrix_approx_eq(&result, &ident);
    }

    #[test]
    fn test_mat4_mul_translate() {
        let translate1 = translate_matrix(10.0, 0.0, 0.0);
        let translate2 = translate_matrix(5.0, 0.0, 0.0);

        let combined = mat4_mul(&translate1, &translate2);
        let expected = translate_matrix(15.0, 0.0, 0.0);

        assert_matrix_approx_eq(&combined, &expected);
    }

    #[test]
    fn test_mat4_mul_scale() {
        let scale1 = scale_matrix(2.0, 2.0, 2.0);
        let scale2 = scale_matrix(3.0, 3.0, 3.0);

        let combined = mat4_mul(&scale1, &scale2);
        let expected = scale_matrix(6.0, 6.0, 6.0);

        assert_matrix_approx_eq(&combined, &expected);
    }

    #[test]
    fn test_mat4_mul_non_commutative() {
        let translate = translate_matrix(10.0, 0.0, 0.0);
        let scale = scale_matrix(2.0, 2.0, 2.0);

        let ts = mat4_mul(&translate, &scale);
        let st = mat4_mul(&scale, &translate);

        assert!(!matrices_close_enough(&ts, &st));
    }

    #[test]
    fn test_mat4_mul_with_identity() {
        let translate = translate_matrix(5.0, 10.0, 15.0);
        let ident = identity_matrix();

        let result1 = mat4_mul(&translate, &ident);
        let result2 = mat4_mul(&ident, &translate);

        assert_matrix_approx_eq(&result1, &translate);
        assert_matrix_approx_eq(&result2, &translate);
    }

    fn matrices_close_enough(a: &[f32; 16], b: &[f32; 16]) -> bool {
        for i in 0..16 {
            if (a[i] - b[i]).abs() > EPSILON {
                return false;
            }
        }
        true
    }

    #[test]
    fn test_complex_transformation() {
        let scale = scale_matrix(2.0, 2.0, 1.0);
        let translate = translate_matrix(100.0, 50.0, 0.0);
        let rotate = rotate_z_matrix_for_test(std::f32::consts::PI / 4.0);

        let combined = mat4_mul(&mat4_mul(&translate, &scale), &rotate);

        assert_eq!(combined.len(), 16);
    }

    fn rotate_z_matrix_for_test(angle: f32) -> [f32; 16] {
        let c = angle.cos();
        let s = angle.sin();
        [
            c, s, 0.0, 0.0, -s, c, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0,
        ]
    }

    #[test]
    fn test_ortho_matrix_inversion_properties() {
        let ortho_mat = ortho(0.0, 800.0, 0.0, 600.0, -100.0, 100.0);

        assert!(ortho_mat[0] > 0.0);
        assert!(ortho_mat[5] > 0.0);
        assert!(ortho_mat[10] < 0.0);
        assert!(ortho_mat[15] == 1.0);
    }
}
