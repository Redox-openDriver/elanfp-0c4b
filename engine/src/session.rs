use std::time::{Duration, Instant};

use elanfp_core::identity::Identity;
use elanfp_core::quality::{self, MAX_MOTION, Reject, Verdict};
use elanfp_core::{Error, Finger, Sensor, SensorDim, Transport, Waiting};
use zeroize::Zeroizing;

use crate::dynamics::Sample;
use crate::{BoxError, Explain, Fail, Msg, Presence, Reporter, hex};

pub type FingerFrame = Zeroizing<Vec<u8>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detect {
    Sensor,
    Frames { interval: Duration },
}

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub detect: Detect,
    pub min_hold: Duration,
    pub hold_limit: Duration,
    pub stable_frames: usize,
    pub poll_ms: u32,
    pub drain_ms: u32,
    pub release_limit: Duration,
    pub base_timeout: Duration,
    pub base_retry: Duration,
    pub pause: Duration,
}

impl Timing {
    pub const SENSOR: Timing = Timing {
        detect: Detect::Sensor,
        min_hold: Duration::from_millis(700),
        hold_limit: Duration::from_secs(4),
        stable_frames: 3,
        poll_ms: 500,
        drain_ms: 20,
        release_limit: Duration::from_secs(15),
        base_timeout: Duration::from_secs(20),
        base_retry: Duration::from_millis(500),
        pause: Duration::from_secs(1),
    };

    pub const POLLING: Timing = Timing { detect: Detect::Frames { interval: Duration::from_millis(120) }, ..Timing::SENSOR };

    pub const fn suited_to(waiting: Waiting) -> Timing {
        match waiting {
            Waiting::Bounded => Timing::SENSOR,
            Waiting::Unbounded => Timing::POLLING,
        }
    }

    pub const fn fits(&self, waiting: Waiting) -> bool {
        !matches!((self.detect, waiting), (Detect::Sensor, Waiting::Unbounded))
    }
}

pub const EMPTY_MARGIN: f64 = 1000.0;
pub const MAX_TRACE: usize = 256;
pub const UNASKED: &str = "?";
pub const PRESENT_COVERAGE: f32 = 0.10;
pub const RELEASED_COVERAGE: f32 = 0.03;

pub enum Touch {
    Accepted { frame: FingerFrame, verdict: Verdict, trace: Vec<Sample> },
    Rejected { reason: &'static str, verdict: Verdict },
    Missed(u8),
    Timeout,
}

pub struct Session<T: Transport> {
    sensor: Sensor<T>,
    identity: Identity,
    dim: SensorDim,
    base: FingerFrame,
    timing: Timing,
    lift_pending: bool,
}

pub fn sensor_fail<T: Transport>(command: &'static str) -> impl FnOnce(Error<T::Error>) -> Fail
where
    T::Error: Explain,
{
    move |e| match e {
        Error::Transport(u) if T::is_timeout(&u) => Fail::new("usb-timeout").arg("command", command),
        Error::Transport(u) => Fail::new("usb-error").arg("command", command).nest("detail", u.explain()),
        Error::Unsupported(seen) => Fail::new("sensor-unsupported")
            .arg("vid", format!("{:04x}", seen.vendor_id))
            .arg("pid", format!("{:04x}", seen.product_id))
            .arg("fw", seen.firmware.map_or_else(|| UNASKED.to_string(), |fw| format!("{:02x}.{:02x}", fw[0], fw[1])))
            .arg("gen", seen.generation.map_or_else(|| UNASKED.to_string(), |generation| format!("{generation:02x}")))
            .arg("dim", seen.dim.map_or_else(|| UNASKED.to_string(), |dim| hex(&dim))),
        Error::ShortWrite { expected, got } => Fail::new("sensor-short-write").arg("command", command).num("expected", expected).num("got", got),
        Error::ShortReply { expected, got } => Fail::new("sensor-short-reply").arg("command", command).num("expected", expected).num("got", got),
        Error::BufferTooSmall { expected, got } => Fail::new("sensor-short-reply").arg("command", command).num("expected", expected).num("got", got),
    }
}

fn explain(reject: Option<Reject>) -> &'static str {
    match reject {
        Some(Reject::PartialTouch) | None => "reject-partial",
        Some(Reject::NoRidges) => "reject-ridges",
        Some(Reject::Moving) => "reject-moving",
    }
}

pub fn pixels(frame: &[u8]) -> impl Iterator<Item = i32> + '_ {
    frame.as_chunks::<2>().0.iter().map(|p| i32::from(u16::from_le_bytes(*p)))
}

impl<T: Transport> Session<T>
where
    T::Error: Explain,
{
    pub fn open(sensor: Sensor<T>, vendor_id: u16, product_id: u16, report: &mut dyn Reporter) -> Result<Self, BoxError> {
        Self::open_with(sensor, vendor_id, product_id, Timing::suited_to(T::WAITING), report)
    }

    pub fn open_with(mut sensor: Sensor<T>, vendor_id: u16, product_id: u16, timing: Timing, report: &mut dyn Reporter) -> Result<Self, BoxError> {
        if !timing.fits(T::WAITING) {
            return Err(Fail::new("transport-without-timeouts").into());
        }
        sensor.stop().map_err(sensor_fail::<T>("00 0b"))?;
        sensor.drain(timing.drain_ms).map_err(sensor_fail::<T>("drain"))?;
        let identity = sensor.identify(vendor_id, product_id).map_err(sensor_fail::<T>("identify"))?;
        let dim = SensorDim { raw: identity.dim };
        let empty_level = f64::from(sensor.calibration_mean().map_err(sensor_fail::<T>("40 24"))?) + EMPTY_MARGIN;
        let mut base: FingerFrame = Zeroizing::new(vec![0u8; dim.frame_bytes()]);
        let started = Instant::now();
        let mut warned = false;
        loop {
            sensor.read_frame(&dim, &mut base).map_err(sensor_fail::<T>("00 09"))?;
            let mean = pixels(&base).map(f64::from).sum::<f64>() / (base.len() / 2) as f64;
            if mean <= empty_level {
                break;
            }
            if started.elapsed() > timing.base_timeout || !report.alive() {
                return Err(Fail::new("touch-stuck").into());
            }
            if !warned {
                report.say(Msg::new("touch-finger-on-sensor"))?;
                warned = true;
            }
            report.rest(timing.base_retry);
        }
        Ok(Session { sensor, identity, dim, base, timing, lift_pending: false })
    }

    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn dim(&self) -> &SensorDim {
        &self.dim
    }

    pub fn base(&self) -> &[u8] {
        &self.base
    }

    pub fn sensor(&mut self) -> &mut Sensor<T> {
        &mut self.sensor
    }

    pub fn pause(&self, report: &mut dyn Reporter) {
        report.rest(self.timing.pause);
    }

    fn cancel(&mut self) -> Result<(), Fail> {
        self.sensor.stop().map_err(sensor_fail::<T>("00 0b"))
    }

    fn coverage_now(&mut self) -> Result<f32, Fail> {
        let mut frame: FingerFrame = Zeroizing::new(vec![0u8; self.dim.frame_bytes()]);
        self.sensor.read_frame(&self.dim, &mut frame).map_err(sensor_fail::<T>("00 09"))?;
        Ok(quality::coverage(&frame, &self.base, self.dim.width(), self.dim.height()))
    }

    fn wait_finger(&mut self, report: &mut dyn Reporter, wait: Duration) -> Result<Option<Finger>, BoxError> {
        let deadline = Instant::now() + wait;
        if let Detect::Sensor = self.timing.detect {
            self.sensor.start_wait_finger().map_err(sensor_fail::<T>("40 3f"))?;
        }
        report.presence(Presence::Needed);
        loop {
            match self.timing.detect {
                Detect::Sensor => {
                    if let Some(finger) = self.sensor.poll_finger(self.timing.poll_ms).map_err(sensor_fail::<T>("40 3f"))? {
                        if finger == Finger::Present {
                            report.presence(Presence::Present);
                        }
                        return Ok(Some(finger));
                    }
                }
                Detect::Frames { interval } => {
                    if self.coverage_now()? >= PRESENT_COVERAGE {
                        report.presence(Presence::Present);
                        return Ok(Some(Finger::Present));
                    }
                    report.rest(interval);
                }
            }
            if !report.alive() {
                self.cancel()?;
                return Err(Fail::new("client-gone").into());
            }
            if Instant::now() >= deadline {
                self.cancel()?;
                return Ok(None);
            }
        }
    }

    fn wait_release(&mut self, report: &mut dyn Reporter) -> Result<bool, BoxError> {
        let deadline = Instant::now() + self.timing.release_limit;
        if let Detect::Sensor = self.timing.detect {
            self.sensor.start_wait_release().map_err(sensor_fail::<T>("40 3e"))?;
        }
        loop {
            let released = match self.timing.detect {
                Detect::Sensor => self.sensor.poll_release(self.timing.poll_ms).map_err(sensor_fail::<T>("40 3e"))?.is_some(),
                Detect::Frames { interval } => {
                    let lifted = self.coverage_now()? <= RELEASED_COVERAGE;
                    if !lifted {
                        report.rest(interval);
                    }
                    lifted
                }
            };
            let gone = !released && !report.alive();
            if released || gone || Instant::now() >= deadline {
                self.cancel()?;
                if released {
                    report.presence(Presence::Away);
                }
                return if gone { Err(Fail::new("client-gone").into()) } else { Ok(released) };
            }
        }
    }

    pub fn touch(&mut self, report: &mut dyn Reporter, wait: Duration) -> Result<Touch, BoxError> {
        if self.lift_pending {
            if self.coverage_now()? > RELEASED_COVERAGE && !self.wait_release(report)? {
                return Ok(Touch::Timeout);
            }
            self.lift_pending = false;
        }
        let finger = match self.wait_finger(report, wait)? {
            Some(finger) => finger,
            None => return Ok(Touch::Timeout),
        };
        if let Finger::Absent(status) = finger {
            self.cancel()?;
            return Ok(Touch::Missed(status));
        }
        let held = Instant::now();
        let mut previous: Option<FingerFrame> = None;
        let mut stable: Vec<(FingerFrame, Verdict)> = Vec::new();
        let mut accepted = None;
        let mut last = None;
        let mut trace = Vec::new();
        while held.elapsed() < self.timing.hold_limit {
            let mut frame: FingerFrame = Zeroizing::new(vec![0u8; self.dim.frame_bytes()]);
            self.sensor.read_frame(&self.dim, &mut frame).map_err(sensor_fail::<T>("00 09"))?;
            let verdict = Verdict::measure(&frame, previous.as_ref().map(|p| p.as_slice()), &self.base, self.dim.width(), self.dim.height());
            last = Some(verdict);
            if trace.len() < MAX_TRACE {
                trace.push(Sample { at: held.elapsed(), verdict });
            }
            previous = Some(frame.clone());
            if !report.alive() {
                self.cancel()?;
                return Err(Fail::new("client-gone").into());
            }
            if !verdict.accepted() {
                stable.clear();
                continue;
            }
            stable.push((frame, verdict));
            if stable.len() >= self.timing.stable_frames && held.elapsed() >= self.timing.min_hold {
                accepted = stable.drain(..).max_by_key(|(_, v)| v.contrast);
                break;
            }
        }
        let told = report.say(Msg::new("touch-lift"));
        let released = self.wait_release(report);
        told?;
        self.lift_pending = !released?;
        Ok(match accepted {
            Some((frame, verdict)) => Touch::Accepted { frame, verdict, trace },
            None => {
                let verdict = last.unwrap_or(Verdict { coverage: 0.0, contrast: 0, motion: 1.0 });
                Touch::Rejected { reason: explain(verdict.reject()), verdict }
            }
        })
    }

    pub fn image(&self, frame: &[u8]) -> Zeroizing<Vec<u8>> {
        let diff: Zeroizing<Vec<i32>> = Zeroizing::new(pixels(frame).zip(pixels(&self.base)).map(|(f, b)| f - b).collect());
        let lo = diff.iter().copied().min().unwrap_or(0);
        let hi = diff.iter().copied().max().unwrap_or(0);
        let span = (hi - lo).max(1);
        Zeroizing::new(diff.iter().map(|&v| ((v - lo) * 255 / span) as u8).collect())
    }

    pub fn close(mut self) -> Result<(), BoxError> {
        self.cancel()?;
        Ok(())
    }
}

impl<T: Transport> Drop for Session<T> {
    fn drop(&mut self) {
        let _ = self.sensor.stop();
    }
}

pub fn with_quality(msg: Msg, verdict: &Verdict) -> Msg {
    msg.sub("quality", "quality")
        .arg("coverage", format!("{:.0}", verdict.coverage * 100.0))
        .num("contrast", verdict.contrast)
        .arg("motion", format!("{:.1}", verdict.motion * 100.0))
        .arg("limit", format!("{:.0}", MAX_MOTION * 100.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::{FAST, FAST_POLLING, FakeSensor, FakeTouch, PID, Recorder, VID, fail_key, finger_frame, flat_frame, partial_frame};
    use elanfp_core::cmd;

    const WAIT: Duration = Duration::from_millis(30);

    fn open(sensor: FakeSensor, report: &mut Recorder) -> Session<FakeSensor> {
        Session::open_with(Sensor::new(sensor), VID, PID, FAST, report).expect("open")
    }

    #[test]
    fn a_steady_finger_gives_an_accepted_frame() {
        let mut report = Recorder::default();
        let fake = FakeSensor::with_touches(&[1]);
        let log = fake.log.clone();
        let mut session = open(fake, &mut report);
        let Touch::Accepted { frame, verdict, trace } = session.touch(&mut report, WAIT).unwrap() else { panic!("not accepted") };
        let dynamics = crate::dynamics::Dynamics::of(&trace).expect("dynamics of an accepted touch");
        assert!(dynamics.frames >= FAST.stable_frames && dynamics.frames <= MAX_TRACE);
        assert_eq!(frame.as_slice(), finger_frame(1).as_slice());
        assert!(verdict.accepted());
        assert_eq!(report.keys, vec!["touch-lift"]);
        assert_eq!(log.sent(&cmd::WAIT_FINGER), 1);
        assert_eq!(log.sent(&cmd::WAIT_RELEASE), 1);
        assert!(log.sent(&cmd::STOP) >= 2);
    }

    #[test]
    fn nobody_touching_times_out_and_cancels_the_wait() {
        let mut report = Recorder::default();
        let fake = FakeSensor::new();
        let log = fake.log.clone();
        let mut session = open(fake, &mut report);
        assert!(matches!(session.touch(&mut report, WAIT).unwrap(), Touch::Timeout));
        assert_eq!(log.last(), Some(cmd::STOP.to_vec()));
    }

    #[test]
    fn late_touch_within_the_wait_is_caught() {
        let mut report = Recorder::default();
        let mut fake = FakeSensor::new();
        fake.push(FakeTouch { delay_polls: 5, status: elanfp_core::FINGER_PRESENT, frame: finger_frame(2) });
        let mut session = open(fake, &mut report);
        assert!(matches!(session.touch(&mut report, Duration::from_secs(5)).unwrap(), Touch::Accepted { .. }));
    }

    #[test]
    fn frame_polling_detects_touch_and_release_without_blocking_waits() {
        let mut report = Recorder::default();
        let mut fake = FakeSensor::new();
        fake.frames_mode = true;
        fake.push(FakeTouch { delay_polls: 4, status: elanfp_core::FINGER_PRESENT, frame: finger_frame(6) });
        let log = fake.log.clone();
        let mut session = Session::open_with(Sensor::new(fake), VID, PID, FAST_POLLING, &mut report).expect("open");
        let Touch::Accepted { frame, .. } = session.touch(&mut report, Duration::from_secs(5)).unwrap() else { panic!("not accepted") };
        assert_eq!(frame.as_slice(), finger_frame(6).as_slice());
        assert_eq!((log.sent(&cmd::WAIT_FINGER), log.sent(&cmd::WAIT_RELEASE)), (0, 0));
        assert!(matches!(session.touch(&mut report, WAIT).unwrap(), Touch::Timeout));
    }

    #[test]
    fn a_finger_held_past_the_lift_limit_is_not_taken_for_a_new_touch() {
        let mut report = Recorder::default();
        let mut fake = FakeSensor::with_touches(&[4, 4]);
        fake.held_on_release = usize::MAX;
        let log = fake.log.clone();
        let mut session = open(fake, &mut report);
        assert!(matches!(session.touch(&mut report, Duration::from_secs(5)).unwrap(), Touch::Accepted { .. }));
        assert!(session.lift_pending);
        assert!(matches!(session.touch(&mut report, Duration::from_secs(5)).unwrap(), Touch::Timeout));
        assert_eq!(log.sent(&cmd::WAIT_FINGER), 1, "no second wait for a finger that never lifted");
    }

    #[test]
    fn leftovers_of_an_interrupted_session_are_discarded_at_start() {
        let mut fake = FakeSensor::new();
        fake.stale_reply = Some(vec![elanfp_core::FINGER_RELEASED]);
        fake.stale_image = finger_frame(3)[..5000].to_vec();
        let session = open(fake, &mut Recorder::default());
        assert_eq!(session.identity().firmware, [0x26, 0x30]);
        assert_eq!(session.base(), crate::fake::empty_frame().as_slice());
        let mut flood = FakeSensor::new();
        flood.stale_image = vec![0x55; elanfp_core::DRAIN_CHUNK * elanfp_core::DRAIN_READS + 1];
        assert!(Session::open_with(Sensor::new(flood), VID, PID, FAST, &mut Recorder::default()).is_err());
    }

    #[test]
    fn transport_without_timeouts_is_only_ever_polled() {
        use crate::fake::NoTimeouts;
        let mut report = Recorder::default();
        let refused = Session::open_with(Sensor::new(NoTimeouts(FakeSensor::new())), VID, PID, FAST, &mut report).err().expect("must be refused");
        assert_eq!(fail_key(&refused), "transport-without-timeouts");
        assert_eq!(Timing::suited_to(Waiting::Unbounded).detect, Timing::POLLING.detect);
        assert_eq!(Timing::suited_to(Waiting::Bounded).detect, Detect::Sensor);
        let mut fake = FakeSensor::new();
        fake.frames_mode = true;
        fake.push(FakeTouch { delay_polls: 3, status: elanfp_core::FINGER_PRESENT, frame: finger_frame(6) });
        let log = fake.log.clone();
        let mut session = Session::open_with(Sensor::new(NoTimeouts(fake)), VID, PID, FAST_POLLING, &mut report).expect("open");
        assert!(matches!(session.touch(&mut report, Duration::from_secs(5)).unwrap(), Touch::Accepted { .. }));
        assert!(matches!(session.touch(&mut report, Duration::from_millis(300)).unwrap(), Touch::Timeout));
        assert_eq!((log.sent(&cmd::WAIT_FINGER), log.sent(&cmd::WAIT_RELEASE)), (0, 0));
    }

    #[test]
    fn frame_polling_notices_a_client_that_hung_up() {
        let mut report = Recorder { hang_up_after: Some(2), ..Recorder::default() };
        let mut fake = FakeSensor::new();
        fake.frames_mode = true;
        let mut session = Session::open_with(Sensor::new(fake), VID, PID, FAST_POLLING, &mut Recorder::default()).expect("open");
        assert_eq!(fail_key(&session.touch(&mut report, Duration::from_secs(30)).err().expect("must fail")), "client-gone");
    }

    #[test]
    fn sensor_reporting_no_finger_is_a_miss() {
        let mut report = Recorder::default();
        let mut fake = FakeSensor::new();
        fake.push(FakeTouch { delay_polls: 0, status: 0xca, frame: Vec::new() });
        let mut session = open(fake, &mut report);
        assert!(matches!(session.touch(&mut report, WAIT).unwrap(), Touch::Missed(0xca)));
    }

    #[test]
    fn partial_and_ridgeless_touches_are_rejected_with_a_reason() {
        for (frame, expected) in [(partial_frame(3), "reject-partial"), (flat_frame(), "reject-ridges")] {
            let mut report = Recorder::default();
            let mut fake = FakeSensor::new();
            fake.press(frame);
            let mut session = open(fake, &mut report);
            let Touch::Rejected { reason, .. } = session.touch(&mut report, WAIT).unwrap() else { panic!("accepted") };
            assert_eq!(reason, expected);
        }
    }

    #[test]
    fn a_client_that_hung_up_frees_the_sensor() {
        let mut report = Recorder { hang_up_after: Some(2), ..Recorder::default() };
        let fake = FakeSensor::new();
        let log = fake.log.clone();
        let mut session = open(fake, &mut report);
        let error = session.touch(&mut report, Duration::from_secs(30)).err().expect("must fail");
        assert_eq!(fail_key(&error), "client-gone");
        assert_eq!(log.last(), Some(cmd::STOP.to_vec()));
        drop(session);
        assert_eq!(log.last(), Some(cmd::STOP.to_vec()));
    }

    #[test]
    fn finger_on_the_sensor_at_start_is_not_taken_as_background() {
        let mut report = Recorder::default();
        let mut fake = FakeSensor::new();
        fake.dirty_base_reads = 3;
        let session = open(fake, &mut report);
        assert_eq!(report.count("touch-finger-on-sensor"), 1);
        assert_eq!(session.base(), crate::fake::empty_frame().as_slice());
        let mut stuck = FakeSensor::new();
        stuck.dirty_base_reads = usize::MAX;
        let error = Session::open_with(Sensor::new(stuck), VID, PID, FAST, &mut Recorder::default()).err().expect("stuck");
        assert_eq!(fail_key(&error), "touch-stuck");
    }

    #[test]
    fn unverified_firmware_is_refused_before_any_capture() {
        let mut fake = FakeSensor::new();
        fake.firmware = [0x27, 0x10];
        let error = Session::open_with(Sensor::new(fake), VID, PID, FAST, &mut Recorder::default()).err().expect("unsupported");
        assert_eq!(fail_key(&error), "sensor-unsupported");
    }

    #[test]
    fn transport_failures_are_reported_not_hidden() {
        let mut fake = FakeSensor::new();
        fake.unplugged = true;
        let error = Session::open_with(Sensor::new(fake), VID, PID, FAST, &mut Recorder::default()).err().expect("unplugged");
        assert_eq!(fail_key(&error), "usb-error");
    }

    #[test]
    fn image_is_the_normalised_difference_from_the_background() {
        let mut report = Recorder::default();
        let session = open(FakeSensor::new(), &mut report);
        let image = session.image(&finger_frame(4));
        assert_eq!(image.len(), 6400);
        assert_eq!((*image.iter().min().unwrap(), *image.iter().max().unwrap()), (0, 255));
    }
}
