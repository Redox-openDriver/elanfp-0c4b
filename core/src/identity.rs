use crate::{Error, Sensor, SensorDim, Transport};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity {
    pub vendor_id: u16,
    pub product_id: u16,
    pub firmware: [u8; 2],
    pub generation: u8,
    pub dim: [u8; 4],
}

impl Identity {
    pub fn tag(&self) -> [u8; 11] {
        let [v0, v1] = self.vendor_id.to_be_bytes();
        let [p0, p1] = self.product_id.to_be_bytes();
        [v0, v1, p0, p1, self.firmware[0], self.firmware[1], self.generation, self.dim[0], self.dim[1], self.dim[2], self.dim[3]]
    }
}

pub const VERIFIED: [Identity; 1] =
    [Identity { vendor_id: crate::VENDOR_ID, product_id: crate::PRODUCT_ID, firmware: [0x26, 0x30], generation: 0x21, dim: [0x4f, 0x00, 0x4f, 0x00] }];

pub fn verified(found: &Identity) -> bool {
    VERIFIED.contains(found)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unverified {
    pub vendor_id: u16,
    pub product_id: u16,
    pub firmware: Option<[u8; 2]>,
    pub generation: Option<u8>,
    pub dim: Option<[u8; 4]>,
}

impl Unverified {
    fn agrees_with(&self, good: &Identity) -> bool {
        (self.vendor_id, self.product_id) == (good.vendor_id, good.product_id)
            && self.firmware.is_none_or(|firmware| firmware == good.firmware)
            && self.generation.is_none_or(|generation| generation == good.generation)
            && self.dim.is_none_or(|dim| dim == good.dim)
    }

    fn known(&self) -> Option<Identity> {
        VERIFIED.iter().copied().find(|good| self.agrees_with(good))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pipe {
    pub address: u8,
    pub bulk: bool,
    pub packet: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wiring<'a> {
    pub configurations: usize,
    pub interfaces: usize,
    pub class: [u8; 3],
    pub pipes: &'a [Pipe],
}

pub const INTERFACE_CLASS: [u8; 3] = [0xff, 0x00, 0x00];
pub const PIPES: [Pipe; 5] = [
    Pipe { address: 0x81, bulk: true, packet: 64 },
    Pipe { address: crate::EP_CMD_OUT, bulk: true, packet: 64 },
    Pipe { address: crate::EP_IMG_IN, bulk: true, packet: 64 },
    Pipe { address: crate::EP_CMD_IN, bulk: true, packet: 64 },
    Pipe { address: 0x03, bulk: true, packet: 64 },
];

pub fn wired_as_verified(wiring: &Wiring<'_>) -> bool {
    wiring.configurations == 1 && wiring.interfaces == 1 && wiring.class == INTERFACE_CLASS && wiring.pipes == PIPES
}

pub fn pipe_position(address: u8) -> Option<usize> {
    PIPES.iter().position(|pipe| pipe.address == address)
}

impl<T: Transport> Sensor<T> {
    pub fn identify(&mut self, vendor_id: u16, product_id: u16) -> Result<Identity, Error<T::Error>> {
        let mut seen = Unverified { vendor_id, product_id, firmware: None, generation: None, dim: None };
        let refuse = |seen: Unverified| Err(Error::Unsupported(seen));
        if seen.known().is_none() {
            return refuse(seen);
        }
        seen.firmware = Some(self.firmware_version()?.raw);
        if seen.known().is_none() {
            return refuse(seen);
        }
        seen.generation = Some(self.generation()?);
        if seen.known().is_none() {
            return refuse(seen);
        }
        let SensorDim { raw: dim } = self.sensor_dim()?;
        seen.dim = Some(dim);
        seen.known().map_or_else(|| refuse(seen), Ok)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::scripted;

    #[test]
    fn only_the_tested_variant_is_accepted() {
        let good = VERIFIED[0];
        assert!(verified(&good));
        assert!(!verified(&Identity { firmware: [0x27, 0x10], ..good }));
        assert!(!verified(&Identity { generation: 0x22, ..good }));
        assert!(!verified(&Identity { product_id: 0x0c4c, ..good }));
        assert!(!verified(&Identity { dim: [0x3f, 0, 0x4f, 0], ..good }));
    }

    #[test]
    fn identify_queries_the_sensor_and_refuses_unknown_firmware() {
        let mut known = Sensor::new(scripted(&[Ok(&[0x26, 0x30]), Ok(&[0x21]), Ok(&[0x4f, 0, 0x4f, 0])]));
        assert_eq!(known.identify(crate::VENDOR_ID, crate::PRODUCT_ID), Ok(VERIFIED[0]));
        let seen = |firmware, generation, dim| Unverified { vendor_id: crate::VENDOR_ID, product_id: crate::PRODUCT_ID, firmware, generation, dim };
        let mut newer = Sensor::new(scripted(&[Ok(&[0x27, 0x10])]));
        assert_eq!(newer.identify(crate::VENDOR_ID, crate::PRODUCT_ID), Err(Error::Unsupported(seen(Some([0x27, 0x10]), None, None))));
        let mut other_chip = Sensor::new(scripted(&[Ok(&[0x26, 0x30]), Ok(&[0x22])]));
        assert_eq!(other_chip.identify(crate::VENDOR_ID, crate::PRODUCT_ID), Err(Error::Unsupported(seen(Some([0x26, 0x30]), Some(0x22), None))));
        let mut other_size = Sensor::new(scripted(&[Ok(&[0x26, 0x30]), Ok(&[0x21]), Ok(&[0x3f, 0, 0x4f, 0])]));
        let odd = seen(Some([0x26, 0x30]), Some(0x21), Some([0x3f, 0, 0x4f, 0]));
        assert_eq!(other_size.identify(crate::VENDOR_ID, crate::PRODUCT_ID), Err(Error::Unsupported(odd)));
        let mut stranger = Sensor::new(scripted(&[]));
        let nothing_asked = Unverified { vendor_id: crate::VENDOR_ID, product_id: 0x0c4c, firmware: None, generation: None, dim: None };
        assert_eq!(stranger.identify(crate::VENDOR_ID, 0x0c4c), Err(Error::Unsupported(nothing_asked)));
    }

    #[test]
    fn only_the_tested_usb_layout_is_accepted() {
        let good = Wiring { configurations: 1, interfaces: 1, class: INTERFACE_CLASS, pipes: &PIPES };
        assert!(wired_as_verified(&good));
        assert!(!wired_as_verified(&Wiring { configurations: 2, ..good }));
        assert!(!wired_as_verified(&Wiring { interfaces: 2, ..good }));
        assert!(!wired_as_verified(&Wiring { class: [0x03, 0, 0], ..good }));
        assert!(!wired_as_verified(&Wiring { pipes: &PIPES[..4], ..good }));
        let mut reordered = PIPES;
        reordered.swap(1, 2);
        assert!(!wired_as_verified(&Wiring { pipes: &reordered, ..good }));
        let mut interrupt = PIPES;
        interrupt[3].bulk = false;
        assert!(!wired_as_verified(&Wiring { pipes: &interrupt, ..good }));
        let mut wide = PIPES;
        wide[2].packet = 512;
        assert!(!wired_as_verified(&Wiring { pipes: &wide, ..good }));
        assert_eq!(
            (pipe_position(crate::EP_CMD_OUT), pipe_position(crate::EP_IMG_IN), pipe_position(crate::EP_CMD_IN), pipe_position(0x05)),
            (Some(1), Some(2), Some(3), None)
        );
    }

    #[test]
    fn tag_covers_every_identity_field() {
        let base = VERIFIED[0];
        let variants = [
            Identity { vendor_id: 1, ..base },
            Identity { product_id: 1, ..base },
            Identity { firmware: [0, 0], ..base },
            Identity { generation: 0, ..base },
            Identity { dim: [0; 4], ..base },
        ];
        for v in variants {
            assert_ne!(v.tag(), base.tag());
        }
    }
}
