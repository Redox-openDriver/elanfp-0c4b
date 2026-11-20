pub const BLOCK: usize = 8;
pub const FINGER_SIGNAL: i32 = 800;
pub const MIN_COVERAGE: f32 = 0.80;
pub const MAX_MOTION: f32 = 0.10;
pub const MIN_RIDGE_CONTRAST: i64 = 250;

pub fn pixel(frame: &[u8], i: usize) -> i32 {
    i32::from(u16::from_le_bytes([frame[2 * i], frame[2 * i + 1]]))
}

pub fn coverage(frame: &[u8], base: &[u8], width: usize, height: usize) -> f32 {
    let (bw, bh) = (width / BLOCK, height / BLOCK);
    let mut covered = 0;
    for by in 0..bh {
        for bx in 0..bw {
            let mut sum = 0;
            for y in by * BLOCK..(by + 1) * BLOCK {
                for x in bx * BLOCK..(bx + 1) * BLOCK {
                    let i = y * width + x;
                    sum += pixel(frame, i) - pixel(base, i);
                }
            }
            if sum / (BLOCK * BLOCK) as i32 > FINGER_SIGNAL {
                covered += 1;
            }
        }
    }
    covered as f32 / (bw * bh) as f32
}

pub fn ridge_contrast(frame: &[u8], base: &[u8], width: usize, height: usize) -> i64 {
    let (bw, bh) = (width / BLOCK, height / BLOCK);
    let n = (BLOCK * BLOCK) as i64;
    let (mut blocks, mut variance_sum) = (0i64, 0i64);
    for by in 0..bh {
        for bx in 0..bw {
            let (mut sum, mut squares) = (0i64, 0i64);
            for y in by * BLOCK..(by + 1) * BLOCK {
                for x in bx * BLOCK..(bx + 1) * BLOCK {
                    let i = y * width + x;
                    let v = i64::from(pixel(frame, i) - pixel(base, i));
                    sum += v;
                    squares += v * v;
                }
            }
            if sum / n > i64::from(FINGER_SIGNAL) {
                blocks += 1;
                variance_sum += squares / n - (sum / n) * (sum / n);
            }
        }
    }
    if blocks == 0 { 0 } else { isqrt(variance_sum / blocks) }
}

fn isqrt(v: i64) -> i64 {
    if v <= 0 {
        return 0;
    }
    let mut x = v;
    let mut y = (x + 1) / 2;
    while y < x {
        x = y;
        y = (x + v / x) / 2;
    }
    x
}

pub fn motion(previous: &[u8], current: &[u8], base: &[u8]) -> f32 {
    let pixels = current.len() / 2;
    let (mut change, mut signal) = (0i64, 0i64);
    for i in 0..pixels {
        let b = pixel(base, i);
        change += i64::from((pixel(current, i) - pixel(previous, i)).abs());
        signal += i64::from((pixel(current, i) - b).max(0));
    }
    if signal == 0 { 1.0 } else { change as f32 / signal as f32 }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Verdict {
    pub coverage: f32,
    pub contrast: i64,
    pub motion: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    PartialTouch,
    NoRidges,
    Moving,
}

impl Verdict {
    pub fn measure(frame: &[u8], previous: Option<&[u8]>, base: &[u8], width: usize, height: usize) -> Self {
        let expected = width * height * 2;
        if frame.len() != expected || base.len() != expected || previous.is_some_and(|p| p.len() != expected) {
            return Verdict { coverage: 0.0, contrast: 0, motion: 1.0 };
        }
        Verdict {
            coverage: coverage(frame, base, width, height),
            contrast: ridge_contrast(frame, base, width, height),
            motion: previous.map_or(1.0, |p| motion(p, frame, base)),
        }
    }

    pub fn reject(&self) -> Option<Reject> {
        if self.coverage < MIN_COVERAGE {
            Some(Reject::PartialTouch)
        } else if self.contrast < MIN_RIDGE_CONTRAST {
            Some(Reject::NoRidges)
        } else if self.motion > MAX_MOTION {
            Some(Reject::Moving)
        } else {
            None
        }
    }

    pub fn accepted(&self) -> bool {
        self.reject().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(value: u16) -> [u8; 2 * 16 * 16] {
        let mut f = [0u8; 2 * 16 * 16];
        for p in f.as_chunks_mut::<2>().0 {
            *p = value.to_le_bytes();
        }
        f
    }

    #[test]
    fn empty_sensor_has_no_coverage() {
        assert_eq!(coverage(&frame(3000), &frame(3000), 16, 16), 0.0);
    }

    fn ridges() -> [u8; 2 * 16 * 16] {
        let mut f = [0u8; 2 * 16 * 16];
        for (i, p) in f.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            let v: u16 = if (i % 16) % 4 < 2 { 7000 } else { 4500 };
            *p = v.to_le_bytes();
        }
        f
    }

    #[test]
    fn still_ridged_press_is_accepted() {
        let (base, finger) = (frame(3000), ridges());
        assert_eq!(Verdict::measure(&finger, Some(&finger), &base, 16, 16).reject(), None);
    }

    #[test]
    fn mismatched_buffers_are_rejected_instead_of_panicking() {
        let (base, finger) = (frame(3000), ridges());
        let v = Verdict::measure(&finger[..10], Some(&finger), &base, 16, 16);
        assert_eq!(v.reject(), Some(Reject::PartialTouch));
        assert!(!Verdict::measure(&finger, Some(&finger[..4]), &base, 16, 16).accepted());
        assert!(!Verdict::measure(&finger, None, &base, 80, 80).accepted());
    }

    #[test]
    fn wet_flat_press_has_no_ridges() {
        let (base, wet) = (frame(3000), frame(6000));
        assert_eq!(Verdict::measure(&wet, Some(&wet), &base, 16, 16).reject(), Some(Reject::NoRidges));
    }

    #[test]
    fn moving_finger_is_rejected() {
        let (base, a, b) = (frame(3000), frame(6000), frame(4000));
        assert!(motion(&a, &b, &base) > MAX_MOTION);
    }
}
