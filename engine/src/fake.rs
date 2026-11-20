use std::collections::VecDeque;

use elanfp_core::{EP_CMD_IN, EP_CMD_OUT, EP_IMG_IN, Transport, Waiting, cmd};
use papilla::sift::Plane;

use crate::session::Timing;
use crate::{BoxError, Msg, Policy, Reporter};

pub const VID: u16 = elanfp_core::VENDOR_ID;
pub const PID: u16 = elanfp_core::PRODUCT_ID;
pub const SIDE: usize = 80;
const EMPTY: u16 = 3000;

pub const FAST: Timing = Timing {
    detect: crate::session::Detect::Sensor,
    min_hold: std::time::Duration::ZERO,
    hold_limit: std::time::Duration::from_millis(300),
    stable_frames: 3,
    poll_ms: 1,
    drain_ms: 1,
    release_limit: std::time::Duration::from_millis(100),
    base_timeout: std::time::Duration::from_millis(200),
    base_retry: std::time::Duration::from_millis(1),
    pause: std::time::Duration::ZERO,
};

pub const FAST_POLLING: Timing = Timing { detect: crate::session::Detect::Frames { interval: std::time::Duration::from_millis(1) }, ..FAST };
const HELD_READS: usize = 8;
const DRIFT: usize = 5;
const PLACES: usize = 4;
const LIFTED_READS: usize = 2;

pub fn policy() -> Policy {
    let mut policy = crate::POLICY;
    policy.profile.enrolling = papilla::Enrolling { min_touches: 3, max_touches: 6, streak: 2, ..policy.profile.enrolling };
    policy.verify_wait = std::time::Duration::from_millis(250);
    policy.verify_budget = std::time::Duration::from_secs(5);
    policy.contention_quiet = std::time::Duration::from_millis(150);
    policy.idle_quiet = std::time::Duration::from_millis(150);
    policy.idle_quiet_limit = std::time::Duration::from_millis(400);
    policy.enroll_wait = std::time::Duration::from_millis(250);
    policy.enroll_idle_limit = 2;
    policy.max_fingers = 2;
    policy
}

#[derive(Debug, PartialEq, Eq)]
pub enum Fault {
    Timeout,
    Unplugged,
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

pub struct FakeTouch {
    pub delay_polls: usize,
    pub status: u8,
    pub frame: Vec<u8>,
}

#[derive(PartialEq, Eq)]
enum Pending {
    Nothing,
    Finger,
    Release,
}

#[derive(Clone, Default)]
pub struct Log(std::rc::Rc<std::cell::RefCell<Vec<Vec<u8>>>>);

impl Log {
    pub fn sent(&self, command: &[u8]) -> usize {
        self.0.borrow().iter().filter(|c| c.as_slice() == command).count()
    }

    pub fn last(&self) -> Option<Vec<u8>> {
        self.0.borrow().last().cloned()
    }
}

pub struct FakeSensor {
    pub firmware: [u8; 2],
    pub dirty_base_reads: usize,
    pub log: Log,
    pub unplugged: bool,
    pub frames_mode: bool,
    pub stale_image: Vec<u8>,
    pub stale_reply: Option<Vec<u8>>,
    image_requested: bool,
    held_reads: usize,
    lifted_reads: usize,
    touches: VecDeque<FakeTouch>,
    pending: Pending,
    reply: Option<Vec<u8>>,
    finger_down: bool,
}

fn raw(levels: impl Iterator<Item = u16>) -> Vec<u8> {
    levels.flat_map(u16::to_le_bytes).collect()
}

pub fn empty_frame() -> Vec<u8> {
    raw(std::iter::repeat_n(EMPTY, SIDE * SIDE))
}

pub fn finger_frame(seed: u32) -> Vec<u8> {
    placed_frame(seed, 0)
}

pub fn placed_frame(seed: u32, step: usize) -> Vec<u8> {
    let skin = SIDE + DRIFT * (PLACES - 1);
    let mut state = seed.wrapping_mul(2654435761).wrapping_add(1);
    let mut noise = vec![0f32; skin * skin];
    for v in noise.iter_mut() {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        *v = (state >> 8) as f32 / (1u32 << 24) as f32;
    }
    let plane = Plane { width: skin, height: skin, data: noise }.blurred(2.0);
    let lo = plane.data.iter().copied().fold(f32::MAX, f32::min);
    let hi = plane.data.iter().copied().fold(f32::MIN, f32::max);
    let (right, down) = (step % PLACES * DRIFT, step / PLACES % PLACES * DRIFT);
    let level = |at: usize| plane.data[(at / SIDE + down) * skin + at % SIDE + right];
    raw((0..SIDE * SIDE).map(|at| EMPTY + 2500 + ((level(at) - lo) / (hi - lo) * 3000.0) as u16))
}

pub fn partial_frame(seed: u32) -> Vec<u8> {
    let (full, empty) = (finger_frame(seed), empty_frame());
    let half = full.len() / 2;
    [&empty[..half], &full[half..]].concat()
}

pub fn flat_frame() -> Vec<u8> {
    raw(std::iter::repeat_n(EMPTY + 3000, SIDE * SIDE))
}

impl Default for FakeSensor {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeSensor {
    pub fn new() -> Self {
        FakeSensor {
            firmware: [0x26, 0x30],
            dirty_base_reads: 0,
            log: Log::default(),
            unplugged: false,
            frames_mode: false,
            stale_image: Vec::new(),
            stale_reply: None,
            image_requested: false,
            held_reads: 0,
            lifted_reads: LIFTED_READS,
            touches: VecDeque::new(),
            pending: Pending::Nothing,
            reply: None,
            finger_down: false,
        }
    }

    pub fn with_touches(seeds: &[u32]) -> Self {
        let mut s = Self::new();
        let pressed_again = seeds.len() > 1 && seeds.iter().all(|seed| *seed == seeds[0]);
        assert!(seeds.len() < PLACES * PLACES, "the fake finger has only {} distinct places", PLACES * PLACES - 1);
        for (index, &seed) in seeds.iter().enumerate() {
            s.press(placed_frame(seed, if pressed_again { index + 1 } else { 0 }));
        }
        s
    }

    pub fn press(&mut self, frame: Vec<u8>) {
        self.touches.push_back(FakeTouch { delay_polls: 0, status: elanfp_core::FINGER_PRESENT, frame });
    }

    pub fn push(&mut self, touch: FakeTouch) {
        self.touches.push_back(touch);
    }

    fn next_polled_frame(&mut self) -> Vec<u8> {
        if !self.finger_down {
            if self.lifted_reads > 0 {
                self.lifted_reads -= 1;
                return empty_frame();
            }
            let Some(touch) = self.touches.front_mut() else { return empty_frame() };
            if touch.delay_polls > 0 {
                touch.delay_polls -= 1;
                return empty_frame();
            }
            self.finger_down = true;
            self.held_reads = 0;
        }
        self.held_reads += 1;
        let frame = self.touches.front().map_or_else(empty_frame, |t| t.frame.clone());
        if self.held_reads >= HELD_READS {
            self.finger_down = false;
            self.lifted_reads = LIFTED_READS;
            self.touches.pop_front();
        }
        frame
    }
}

impl Transport for FakeSensor {
    type Error = Fault;

    const WAITING: Waiting = Waiting::Bounded;

    fn bulk_out(&mut self, ep: u8, data: &[u8], _: u32) -> Result<usize, Fault> {
        assert_eq!(ep, EP_CMD_OUT);
        if self.unplugged {
            return Err(Fault::Unplugged);
        }
        self.log.0.borrow_mut().push(data.to_vec());
        self.reply = None;
        match data {
            d if d == cmd::FW_VERSION => self.reply = Some(self.firmware.to_vec()),
            d if d == cmd::GENERATION => self.reply = Some(vec![0x21]),
            d if d == cmd::SENSOR_DIM => self.reply = Some(vec![0x4f, 0x00, 0x4f, 0x00]),
            d if d == cmd::CALIB_MEAN => self.reply = Some(3010u16.to_be_bytes().to_vec()),
            d if d == cmd::SENSOR_STATUS => self.reply = Some(vec![0x80]),
            d if d == cmd::DAC => self.reply = Some(vec![0x2d]),
            d if d == cmd::WAIT_FINGER => self.pending = Pending::Finger,
            d if d == cmd::WAIT_RELEASE => self.pending = Pending::Release,
            d if d == cmd::STOP => self.pending = Pending::Nothing,
            d if d == cmd::GET_IMAGE => self.image_requested = true,
            other => panic!("unexpected command {other:02x?}"),
        }
        Ok(data.len())
    }

    fn bulk_in(&mut self, ep: u8, buf: &mut [u8], _: u32) -> Result<usize, Fault> {
        if self.unplugged {
            return Err(Fault::Unplugged);
        }
        let data = if ep == EP_IMG_IN {
            if !self.stale_image.is_empty() {
                let count = buf.len().min(self.stale_image.len());
                self.stale_image.drain(..count).collect()
            } else if !std::mem::take(&mut self.image_requested) {
                return Err(Fault::Timeout);
            } else if self.dirty_base_reads > 0 {
                self.dirty_base_reads -= 1;
                flat_frame()
            } else if self.frames_mode {
                self.next_polled_frame()
            } else if self.finger_down {
                self.touches.front().map_or_else(empty_frame, |t| t.frame.clone())
            } else {
                empty_frame()
            }
        } else {
            assert_eq!(ep, EP_CMD_IN);
            if let Some(stale) = self.stale_reply.take() {
                stale
            } else if let Some(reply) = self.reply.take() {
                reply
            } else {
                match self.pending {
                    Pending::Nothing => return Err(Fault::Timeout),
                    Pending::Finger => {
                        let Some(touch) = self.touches.front_mut() else { return Err(Fault::Timeout) };
                        if touch.delay_polls > 0 {
                            touch.delay_polls -= 1;
                            return Err(Fault::Timeout);
                        }
                        let status = touch.status;
                        self.pending = Pending::Nothing;
                        self.finger_down = status == elanfp_core::FINGER_PRESENT;
                        if !self.finger_down {
                            self.touches.pop_front();
                        }
                        vec![status]
                    }
                    Pending::Release => {
                        self.pending = Pending::Nothing;
                        self.finger_down = false;
                        self.touches.pop_front();
                        vec![elanfp_core::FINGER_RELEASED]
                    }
                }
            }
        };
        let n = data.len().min(buf.len());
        buf[..n].copy_from_slice(&data[..n]);
        Ok(data.len())
    }

    fn is_timeout(error: &Fault) -> bool {
        *error == Fault::Timeout
    }
}

impl crate::Explain for Fault {
    fn explain(&self) -> crate::Msg {
        crate::Msg::new("usb-transfer-failed").arg("cause", self)
    }
}

pub struct NoTimeouts(pub FakeSensor);

impl Transport for NoTimeouts {
    type Error = Fault;

    const WAITING: Waiting = Waiting::Unbounded;

    fn bulk_out(&mut self, ep: u8, data: &[u8], timeout_ms: u32) -> Result<usize, Fault> {
        self.0.bulk_out(ep, data, timeout_ms)
    }

    fn bulk_in(&mut self, ep: u8, buf: &mut [u8], timeout_ms: u32) -> Result<usize, Fault> {
        self.0.bulk_in(ep, buf, timeout_ms)
    }

    fn is_timeout(_: &Fault) -> bool {
        false
    }
}

#[derive(Default)]
pub struct Recorder {
    pub keys: Vec<String>,
    pub messages: Vec<Msg>,
    pub hang_up_after: Option<usize>,
    pub polls: usize,
}

impl Recorder {
    pub fn count(&self, key: &str) -> usize {
        self.keys.iter().filter(|k| k.as_str() == key).count()
    }
}

pub fn renders_everywhere(message: &Msg) {
    for language in ["ru", "en"] {
        if let Err(problems) = crate::catalog_for(Some(language)).try_render(message) {
            panic!("message {message} does not render in {language}: {problems:?}");
        }
    }
}

impl Reporter for Recorder {
    fn say(&mut self, message: Msg) -> Result<(), BoxError> {
        renders_everywhere(&message);
        self.keys.push(message.key.clone());
        self.messages.push(message);
        Ok(())
    }

    fn alive(&mut self) -> bool {
        self.polls += 1;
        self.hang_up_after.is_none_or(|limit| self.polls <= limit)
    }
}

pub fn fail_key(error: &BoxError) -> String {
    let message = crate::as_msg(error.as_ref(), "error-internal");
    renders_everywhere(&message);
    message.key
}

pub struct TempDir(pub std::path::PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("elanfp-test-{}-{tag}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        TempDir(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
