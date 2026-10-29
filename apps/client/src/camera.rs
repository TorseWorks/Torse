//! Camera and matrix math for the client.
//!
//! Matrices are column-major `[f32; 16]`, the layout WGSL expects. The
//! projection targets WebGPU's `0..1` depth range.

/// Deterministic paths for finite performance measurements.
#[derive(Clone, Copy, Debug)]
pub enum BenchmarkMotion {
    Stationary,
    Local,
    Travel,
}
impl BenchmarkMotion {
    pub fn eye(self, frame: u32) -> [f32; 3] {
        let frame = frame as f32;
        match self {
            Self::Stationary => [15.5, 46.0, 8.5],
            Self::Local => [
                15.5 + (frame * 0.01).sin() * 2.0,
                46.0,
                8.5 + (frame * 0.01).cos() * 2.0,
            ],
            Self::Travel => [15.5, 46.0, 8.5 - frame * 0.5],
        }
    }
}

/// Perspective camera looking at a target.
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    /// Eye position in world space.
    pub eye: [f32; 3],
    /// Look-at target.
    pub target: [f32; 3],
    /// Up vector.
    pub up: [f32; 3],
    /// Vertical field of view in degrees.
    pub fov_y_deg: f32,
    /// Near plane.
    pub near: f32,
    /// Far plane.
    pub far: f32,
}

impl Camera {
    /// Look from `eye` at `target`.
    pub fn new(eye: [f32; 3], target: [f32; 3]) -> Self {
        Self {
            eye,
            target,
            up: [0.0, 1.0, 0.0],
            fov_y_deg: 55.0,
            near: 0.1,
            far: 400.0,
        }
    }

    /// Column-major view-projection matrix.
    pub fn view_projection(&self, aspect: f32) -> [f32; 16] {
        let projection = perspective(self.fov_y_deg, aspect, self.near, self.far);
        let view = look_at(self.eye, self.target, self.up);
        multiply(projection, view)
    }
}

/// One frame of movement intent, normalized to -1..1 per axis.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct MoveInput {
    /// Forward when positive, backward when negative.
    pub forward: f32,
    /// Right when positive, left when negative.
    pub strafe: f32,
    /// World up when positive, down when negative.
    pub vertical: f32,
    /// Look yaw delta in radians, positive turns right.
    pub yaw_delta: f32,
    /// Look pitch delta in radians, positive looks up.
    pub pitch_delta: f32,
}

/// Free-flying camera driven by yaw and pitch instead of a look-at target.
///
/// Yaw zero faces negative z; positive yaw turns right. Pitch is clamped
/// below the poles so the up vector in `look_at` never degenerates.
#[derive(Clone, Copy, Debug)]
pub struct FlyCamera {
    /// Eye position in world space.
    pub eye: [f32; 3],
    /// Yaw around the world up axis in radians.
    pub yaw: f32,
    /// Pitch from the horizon in radians.
    pub pitch: f32,
    /// Movement speed in blocks per second.
    pub speed: f32,
    /// Vertical field of view in degrees.
    pub fov_y_deg: f32,
    /// Near plane.
    pub near: f32,
    /// Far plane.
    pub far: f32,
    /// Lowest eye position the camera may descend to.
    pub min_y: f32,
    /// Highest eye position the camera may rise to.
    pub max_y: f32,
}

impl FlyCamera {
    /// Build a camera at `eye` with default flight parameters.
    pub fn new(eye: [f32; 3]) -> Self {
        Self {
            eye,
            yaw: 0.0,
            pitch: -0.25,
            speed: 12.0,
            fov_y_deg: 55.0,
            near: 0.1,
            far: 400.0,
            min_y: 1.0,
            max_y: 256.0,
        }
    }

    /// Unit forward vector derived from yaw and pitch.
    pub fn forward(&self) -> [f32; 3] {
        let cos_pitch = self.pitch.cos();
        [
            -self.yaw.sin() * cos_pitch,
            self.pitch.sin(),
            -self.yaw.cos() * cos_pitch,
        ]
    }

    /// Unit right vector on the horizontal plane.
    pub fn right(&self) -> [f32; 3] {
        [self.yaw.cos(), 0.0, -self.yaw.sin()]
    }

    /// Apply look deltas with pitch clamped just inside the poles.
    pub fn look(&mut self, yaw_delta: f32, pitch_delta: f32) {
        self.yaw += yaw_delta;
        let limit = 89.0f32.to_radians();
        self.pitch = (self.pitch + pitch_delta).clamp(-limit, limit);
    }

    /// Integrate one frame of movement in seconds.
    pub fn advance(&mut self, input: MoveInput, delta_seconds: f32) {
        self.look(input.yaw_delta, input.pitch_delta);
        let forward = [self.forward()[0], 0.0, self.forward()[2]];
        let forward = normalize(forward);
        let mut motion = [0.0f32; 3];
        for (component, (ahead, side)) in motion.iter_mut().zip(forward.iter().zip(self.right())) {
            *component = ahead * input.forward + side * input.strafe;
        }
        motion[1] += input.vertical;
        let motion = normalize(motion);
        let step = self.speed * delta_seconds;
        for (eye, drift) in self.eye.iter_mut().zip(motion) {
            *eye += drift * step;
        }
        self.eye[1] = self.eye[1].clamp(self.min_y, self.max_y);
    }

    /// Column-major view-projection matrix.
    pub fn view_projection(&self, aspect: f32) -> [f32; 16] {
        let target = [
            self.eye[0] + self.forward()[0],
            self.eye[1] + self.forward()[1],
            self.eye[2] + self.forward()[2],
        ];
        let projection = perspective(self.fov_y_deg, aspect, self.near, self.far);
        let view = look_at(self.eye, target, [0.0, 1.0, 0.0]);
        multiply(projection, view)
    }
}

fn perspective(fov_y_deg: f32, aspect: f32, near: f32, far: f32) -> [f32; 16] {
    let f = 1.0 / (fov_y_deg.to_radians() / 2.0).tan();
    let mut matrix = [0.0f32; 16];
    matrix[0] = f / aspect;
    matrix[5] = f;
    matrix[10] = far / (near - far);
    matrix[11] = -1.0;
    matrix[14] = near * far / (near - far);
    matrix
}

fn look_at(eye: [f32; 3], target: [f32; 3], up: [f32; 3]) -> [f32; 16] {
    let forward = normalize(sub(target, eye));
    let right = normalize(cross(forward, up));
    let true_up = cross(right, forward);
    [
        right[0],
        true_up[0],
        -forward[0],
        0.0,
        right[1],
        true_up[1],
        -forward[1],
        0.0,
        right[2],
        true_up[2],
        -forward[2],
        0.0,
        -dot(right, eye),
        -dot(true_up, eye),
        dot(forward, eye),
        1.0,
    ]
}

fn multiply(left: [f32; 16], right: [f32; 16]) -> [f32; 16] {
    let mut result = [0.0f32; 16];
    for column in 0..4 {
        for row in 0..4 {
            let mut sum = 0.0;
            for index in 0..4 {
                sum += left[index * 4 + row] * right[column * 4 + index];
            }
            result[column * 4 + row] = sum;
        }
    }
    result
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

fn normalize(value: [f32; 3]) -> [f32; 3] {
    let length = dot(value, value).sqrt();
    if length == 0.0 {
        return [0.0, 0.0, 0.0];
    }
    [value[0] / length, value[1] / length, value[2] / length]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perspective_maps_near_to_zero_and_far_to_one() {
        let matrix = perspective(60.0, 1.0, 0.1, 100.0);
        // Column-major: clip_z = matrix[10] * z + matrix[14], clip_w = -z.
        let near_clip = matrix[10] * -0.1 + matrix[14];
        let near_w = 0.1;
        assert!((near_clip / near_w).abs() < 1e-5);
        let far_clip = matrix[10] * -100.0 + matrix[14];
        let far_w = 100.0;
        assert!((far_clip / far_w - 1.0).abs() < 1e-4);
    }

    #[test]
    fn camera_looking_forward_maps_target_to_negative_z() {
        let camera = Camera::new([0.0, 0.0, 5.0], [0.0, 0.0, 0.0]);
        let matrix = camera.view_projection(1.0);
        // The point at the origin lands on the negative z axis in view space
        // before projection, which the matrix multiplication preserves.
        let x = matrix[0] * 0.0 + matrix[4] * 0.0 + matrix[8] * 0.0 + matrix[12];
        let y = matrix[1] * 0.0 + matrix[5] * 0.0 + matrix[9] * 0.0 + matrix[13];
        let z = matrix[2] * 0.0 + matrix[6] * 0.0 + matrix[10] * 0.0 + matrix[14];
        let w = matrix[3] * 0.0 + matrix[7] * 0.0 + matrix[11] * 0.0 + matrix[15];
        assert!(w > 0.0);
        assert!(x.abs() < 1e-5 && y.abs() < 1e-5);
        assert!(z / w > 0.0 && z / w < 1.0);
    }

    #[test]
    fn fly_camera_starts_facing_negative_z() {
        let mut camera = FlyCamera::new([0.0, 10.0, 0.0]);
        // The constructor pitches slightly down; level out first.
        camera.pitch = 0.0;
        assert!((camera.forward()[0]).abs() < 1e-6);
        assert!((camera.forward()[1]).abs() < 1e-6);
        assert!((camera.forward()[2] + 1.0).abs() < 1e-6);
    }

    #[test]
    fn fly_camera_clamps_pitch_inside_the_poles() {
        let mut camera = FlyCamera::new([0.0, 10.0, 0.0]);
        camera.look(0.0, 2.0);
        assert!(camera.pitch < 89.5f32.to_radians());
        camera.look(0.0, -4.0);
        assert!(camera.pitch > -89.5f32.to_radians());
    }

    #[test]
    fn fly_camera_moves_along_input_axes() {
        let mut camera = FlyCamera::new([0.0, 10.0, 0.0]);
        camera.advance(
            MoveInput {
                forward: 1.0,
                ..MoveInput::default()
            },
            1.0,
        );
        assert!(camera.eye[2] < -0.9);
        assert!((camera.eye[1] - 10.0).abs() < 1e-3);

        let mut camera = FlyCamera::new([0.0, 10.0, 0.0]);
        camera.advance(
            MoveInput {
                strafe: 1.0,
                ..MoveInput::default()
            },
            1.0,
        );
        assert!(camera.eye[0] > 0.9);

        let mut camera = FlyCamera::new([0.0, 10.0, 0.0]);
        camera.advance(
            MoveInput {
                vertical: -1.0,
                ..MoveInput::default()
            },
            0.5,
        );
        assert!((camera.eye[1] - 4.0).abs() < 1e-3);
    }

    #[test]
    fn fly_camera_stops_at_its_vertical_bounds() {
        let mut camera = FlyCamera::new([0.0, 10.0, 0.0]);
        camera.advance(
            MoveInput {
                vertical: -1.0,
                ..MoveInput::default()
            },
            60.0,
        );
        assert!((camera.eye[1] - camera.min_y).abs() < 1e-3);
    }
}
