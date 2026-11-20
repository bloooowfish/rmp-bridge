use std::f32::consts::{PI, TAU};

/// Normalizes an angle in radians into `[-PI, PI]`.
pub fn wrap_angle_radians(angle: f32) -> f32 {
    if !angle.is_finite() {
        return 0.0;
    }
    let wrapped = (angle + PI).rem_euclid(TAU) - PI;
    if wrapped.is_finite() { wrapped } else { 0.0 }
}

/// Normalizes an angle in degrees into `[-180, 180]`.
pub fn wrap_angle_degrees(deg: f32) -> f32 {
    if !deg.is_finite() {
        return 0.0;
    }
    let wrapped = (deg + 180.0).rem_euclid(360.0) - 180.0;
    if wrapped.is_finite() { wrapped } else { 0.0 }
}

/// Converts UE5 scene actor coordinates (cm) & Yaw (deg) into FFXIV local coordinates (m) & rotation (rad).
///
/// ReMakePlace axis mapping (`remake_place.rs`):
/// - `FFXIV.X = UE.X / 100`
/// - `FFXIV.Y (up) = UE.Z / 100`
/// - `FFXIV.Z (depth) = UE.Y / 100`
/// - `FFXIV.rot = -UE.Yaw.to_radians()`
pub fn ue_to_ffxiv_local(loc_cm: [f32; 3], yaw_deg: f32) -> ([f32; 3], f32) {
    let pos = [loc_cm[0] / 100.0, loc_cm[2] / 100.0, loc_cm[1] / 100.0];
    let rot = wrap_angle_radians(-yaw_deg.to_radians());
    (pos, rot)
}

/// Converts FFXIV local coordinates (m) & rotation (rad) into UE5 scene actor coordinates (cm) & Yaw (deg).
pub fn ffxiv_local_to_ue(pos_m: [f32; 3], rot_rad: f32) -> ([f32; 3], f32) {
    let loc = [pos_m[0] * 100.0, pos_m[2] * 100.0, pos_m[1] * 100.0];
    let yaw = wrap_angle_degrees(-rot_rad.to_degrees());
    (loc, yaw)
}

/// Computes the Z-axis Euler angle in degrees matching `QuaternionExtensions.ComputeZAngle(q)`.
pub fn quat_xyzw_to_yaw_deg(q: [f32; 4]) -> f32 {
    let (x, y, z, w) = (q[0], q[1], q[2], q[3]);
    let siny_cosp = 2.0 * (w * z + x * y);
    let cosy_cosp = 1.0 - 2.0 * (y * y + z * z);
    wrap_angle_degrees(siny_cosp.atan2(cosy_cosp).to_degrees())
}

pub fn angle_diff_deg(a: f32, b: f32) -> f32 {
    ((a - b + 180.0).rem_euclid(360.0) - 180.0).abs()
}

pub fn ue_dist_sq(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    dx * dx + dy * dy + dz * dz
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ue_and_ffxiv_roundtrip_preserves_axes_and_rotation() {
        let ue_in = [350.0, -200.0, 125.0];
        let yaw_in = 42.5;
        let (ff_pos, ff_rot) = ue_to_ffxiv_local(ue_in, yaw_in);
        assert!((ff_pos[0] - 3.5).abs() < 1e-4);
        assert!((ff_pos[1] - 1.25).abs() < 1e-4);
        assert!((ff_pos[2] - (-2.0)).abs() < 1e-4);

        let (ue_out, yaw_out) = ffxiv_local_to_ue(ff_pos, ff_rot);
        assert!((ue_out[0] - ue_in[0]).abs() < 1e-2);
        assert!((ue_out[1] - ue_in[1]).abs() < 1e-2);
        assert!((ue_out[2] - ue_in[2]).abs() < 1e-2);
        assert!(angle_diff_deg(yaw_out, yaw_in) < 1e-3);
    }

    #[test]
    fn quaternion_z_angle_matches_remakeplace_sign_convention() {
        let rot_ffxiv = 0.75f32;
        let half = -rot_ffxiv * 0.5;
        let q = [0.0, 0.0, half.sin(), half.cos()];
        let yaw = quat_xyzw_to_yaw_deg(q);
        assert!(angle_diff_deg(yaw, -rot_ffxiv.to_degrees()) < 1e-2);
    }
}
