#![no_std]

pub mod identity;
pub mod quality;

pub const VENDOR_ID: u16 = 0x04f3;
pub const PRODUCT_ID: u16 = 0x0c4b;

pub const EP_CMD_OUT: u8 = 0x01;
pub const EP_CMD_IN: u8 = 0x83;
pub const EP_IMG_IN: u8 = 0x82;

pub const TIMEOUT_MS: u32 = 2000;
pub const DRAIN_CHUNK: usize = 512;
pub const DRAIN_READS: usize = 32;
pub const FINGER_PRESENT: u8 = 0x55;
pub const FINGER_RELEASED: u8 = 0xaa;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Waiting {
    Bounded,
    Unbounded,
}

pub trait Transport {
    type Error: core::fmt::Debug;

    const WAITING: Waiting;

    fn bulk_out(&mut self, ep: u8, data: &[u8], timeout_ms: u32) -> Result<usize, Self::Error>;

    fn bulk_in(&mut self, ep: u8, buf: &mut [u8], timeout_ms: u32) -> Result<usize, Self::Error>;

    fn is_timeout(error: &Self::Error) -> bool;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finger {
    Present,
    Absent(u8),
}

impl From<u8> for Finger {
    fn from(status: u8) -> Self {
        if status == FINGER_PRESENT { Finger::Present } else { Finger::Absent(status) }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error<E> {
    Transport(E),
    Unsupported(identity::Unverified),
    ShortWrite { expected: usize, got: usize },
    ShortReply { expected: usize, got: usize },
    BufferTooSmall { expected: usize, got: usize },
}

impl<E> From<E> for Error<E> {
    fn from(e: E) -> Self {
        Error::Transport(e)
    }
}

pub mod cmd {
    pub const FW_VERSION: [u8; 2] = [0x40, 0x19];
    pub const SENSOR_DIM: [u8; 2] = [0x00, 0x0c];
    pub const GENERATION: [u8; 3] = [0x40, 0x21, 0x0a];
    pub const DAC: [u8; 3] = [0x40, 0x21, 0x06];
    pub const CALIB_MEAN: [u8; 2] = [0x40, 0x24];
    pub const SENSOR_STATUS: [u8; 2] = [0x40, 0x13];
    pub const WAIT_FINGER: [u8; 2] = [0x40, 0x3f];
    pub const WAIT_RELEASE: [u8; 2] = [0x40, 0x3e];
    pub const GET_IMAGE: [u8; 2] = [0x00, 0x09];
    pub const STOP: [u8; 2] = [0x00, 0x0b];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirmwareVersion {
    pub raw: [u8; 2],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SensorDim {
    pub raw: [u8; 4],
}

impl SensorDim {
    pub fn width(&self) -> usize {
        usize::from(self.raw[0]) + 1
    }

    pub fn height(&self) -> usize {
        usize::from(self.raw[2]) + 1
    }

    pub fn frame_bytes(&self) -> usize {
        self.width() * self.height() * 2
    }
}

pub struct Sensor<T: Transport> {
    transport: T,
}

impl<T: Transport> Sensor<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    pub fn into_transport(self) -> T {
        self.transport
    }

    fn send(&mut self, command: &[u8]) -> Result<(), Error<T::Error>> {
        let written = self.transport.bulk_out(EP_CMD_OUT, command, TIMEOUT_MS)?;
        if written != command.len() {
            return Err(Error::ShortWrite { expected: command.len(), got: written });
        }
        Ok(())
    }

    fn reply<const N: usize>(&mut self, timeout_ms: u32) -> Result<[u8; N], Error<T::Error>> {
        let mut reply = [0u8; N];
        let got = self.transport.bulk_in(EP_CMD_IN, &mut reply, timeout_ms)?;
        if got != N {
            return Err(Error::ShortReply { expected: N, got });
        }
        Ok(reply)
    }

    fn query<const N: usize>(&mut self, command: &[u8]) -> Result<[u8; N], Error<T::Error>> {
        self.send(command)?;
        self.reply(TIMEOUT_MS)
    }

    fn poll(&mut self, timeout_ms: u32) -> Result<Option<u8>, Error<T::Error>> {
        match self.reply::<1>(timeout_ms) {
            Ok(byte) => Ok(Some(byte[0])),
            Err(Error::Transport(e)) if T::is_timeout(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn firmware_version(&mut self) -> Result<FirmwareVersion, Error<T::Error>> {
        self.query::<2>(&cmd::FW_VERSION).map(|raw| FirmwareVersion { raw })
    }

    pub fn sensor_dim(&mut self) -> Result<SensorDim, Error<T::Error>> {
        self.query::<4>(&cmd::SENSOR_DIM).map(|raw| SensorDim { raw })
    }

    pub fn generation(&mut self) -> Result<u8, Error<T::Error>> {
        self.query::<1>(&cmd::GENERATION).map(|r| r[0])
    }

    pub fn dac(&mut self) -> Result<u8, Error<T::Error>> {
        self.query::<1>(&cmd::DAC).map(|r| r[0])
    }

    pub fn calibration_mean(&mut self) -> Result<u16, Error<T::Error>> {
        self.query::<2>(&cmd::CALIB_MEAN).map(u16::from_be_bytes)
    }

    pub fn sensor_status(&mut self) -> Result<u8, Error<T::Error>> {
        self.query::<1>(&cmd::SENSOR_STATUS).map(|r| r[0])
    }

    pub fn start_wait_finger(&mut self) -> Result<(), Error<T::Error>> {
        self.send(&cmd::WAIT_FINGER)
    }

    pub fn poll_finger(&mut self, timeout_ms: u32) -> Result<Option<Finger>, Error<T::Error>> {
        Ok(self.poll(timeout_ms)?.map(Finger::from))
    }

    pub fn start_wait_release(&mut self) -> Result<(), Error<T::Error>> {
        self.send(&cmd::WAIT_RELEASE)
    }

    pub fn poll_release(&mut self, timeout_ms: u32) -> Result<Option<u8>, Error<T::Error>> {
        self.poll(timeout_ms)
    }

    pub fn stop(&mut self) -> Result<(), Error<T::Error>> {
        self.send(&cmd::STOP)
    }

    pub fn drain(&mut self, timeout_ms: u32) -> Result<(), Error<T::Error>> {
        if T::WAITING == Waiting::Unbounded {
            return Ok(());
        }
        let mut scratch = [0u8; DRAIN_CHUNK];
        for ep in [EP_CMD_IN, EP_IMG_IN] {
            for _ in 0..DRAIN_READS {
                match self.transport.bulk_in(ep, &mut scratch, timeout_ms) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(e) if T::is_timeout(&e) => break,
                    Err(e) => return Err(Error::Transport(e)),
                }
            }
        }
        Ok(())
    }

    pub fn read_frame(&mut self, dim: &SensorDim, frame: &mut [u8]) -> Result<(), Error<T::Error>> {
        let expected = dim.frame_bytes();
        if frame.len() < expected {
            return Err(Error::BufferTooSmall { expected, got: frame.len() });
        }
        self.send(&cmd::GET_IMAGE)?;
        let got = self.transport.bulk_in(EP_IMG_IN, &mut frame[..expected], TIMEOUT_MS)?;
        if got != expected {
            return Err(Error::ShortReply { expected, got });
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    pub(crate) enum Fault {
        Timeout,
        Broken,
    }

    pub(crate) struct Scripted<'a> {
        pub sent: [u8; 64],
        pub sent_len: usize,
        pub replies: &'a [Result<&'a [u8], Fault>],
        pub next: usize,
    }

    pub(crate) fn scripted<'a>(replies: &'a [Result<&'a [u8], Fault>]) -> Scripted<'a> {
        Scripted { sent: [0; 64], sent_len: 0, replies, next: 0 }
    }

    impl Transport for Scripted<'_> {
        type Error = Fault;

        fn bulk_out(&mut self, ep: u8, data: &[u8], _: u32) -> Result<usize, Fault> {
            assert_eq!(ep, EP_CMD_OUT);
            self.sent[self.sent_len..self.sent_len + data.len()].copy_from_slice(data);
            self.sent_len += data.len();
            Ok(data.len())
        }

        fn bulk_in(&mut self, _: u8, buf: &mut [u8], _: u32) -> Result<usize, Fault> {
            let reply = self.replies.get(self.next).ok_or(Fault::Broken)?;
            self.next += 1;
            match reply {
                Ok(bytes) => {
                    let n = bytes.len().min(buf.len());
                    buf[..n].copy_from_slice(&bytes[..n]);
                    Ok(bytes.len())
                }
                Err(Fault::Timeout) => Err(Fault::Timeout),
                Err(Fault::Broken) => Err(Fault::Broken),
            }
        }

        fn is_timeout(error: &Fault) -> bool {
            *error == Fault::Timeout
        }

        const WAITING: Waiting = Waiting::Bounded;
    }

    #[test]
    fn firmware_version_sends_40_19() {
        let mut s = Sensor::new(scripted(&[Ok(&[0x26, 0x30])]));
        assert_eq!(s.firmware_version(), Ok(FirmwareVersion { raw: [0x26, 0x30] }));
        let t = s.into_transport();
        assert_eq!(&t.sent[..t.sent_len], &[0x40, 0x19]);
    }

    #[test]
    fn dimension_bytes_are_last_index() {
        let d = SensorDim { raw: [0x4f, 0x00, 0x4f, 0x00] };
        assert_eq!((d.width(), d.height(), d.frame_bytes()), (80, 80, 12800));
    }

    #[test]
    fn short_and_long_replies_are_rejected() {
        let mut s = Sensor::new(scripted(&[Ok(&[0x00]), Ok(&[1, 2, 3, 4, 5])]));
        assert_eq!(s.sensor_dim(), Err(Error::ShortReply { expected: 4, got: 1 }));
        assert_eq!(s.sensor_dim(), Err(Error::ShortReply { expected: 4, got: 5 }));
    }

    #[test]
    fn polling_distinguishes_timeout_from_failure() {
        let mut s = Sensor::new(scripted(&[Err(Fault::Timeout), Ok(&[0x55]), Ok(&[0xca]), Err(Fault::Broken)]));
        s.start_wait_finger().unwrap();
        assert_eq!(s.poll_finger(500), Ok(None));
        assert_eq!(s.poll_finger(500), Ok(Some(Finger::Present)));
        assert_eq!(s.poll_finger(500), Ok(Some(Finger::Absent(0xca))));
        assert_eq!(s.poll_finger(500), Err(Error::Transport(Fault::Broken)));
        let t = s.into_transport();
        assert_eq!(&t.sent[..t.sent_len], &[0x40, 0x3f]);
    }

    #[test]
    fn release_wait_and_stop_use_their_commands() {
        let mut s = Sensor::new(scripted(&[Err(Fault::Timeout), Ok(&[0xaa])]));
        s.start_wait_release().unwrap();
        assert_eq!(s.poll_release(500), Ok(None));
        assert_eq!(s.poll_release(500), Ok(Some(FINGER_RELEASED)));
        s.stop().unwrap();
        let t = s.into_transport();
        assert_eq!(&t.sent[..t.sent_len], &[0x40, 0x3e, 0x00, 0x0b]);
    }

    #[test]
    fn frame_needs_a_large_enough_buffer_and_a_full_reply() {
        let dim = SensorDim { raw: [0x4f, 0, 0x4f, 0] };
        let mut s = Sensor::new(scripted(&[Ok(&[0u8; 100])]));
        let mut small = [0u8; 16];
        assert_eq!(s.read_frame(&dim, &mut small), Err(Error::BufferTooSmall { expected: 12800, got: 16 }));
        let mut frame = [0u8; 12800];
        assert_eq!(s.read_frame(&dim, &mut frame), Err(Error::ShortReply { expected: 12800, got: 100 }));
    }
}
