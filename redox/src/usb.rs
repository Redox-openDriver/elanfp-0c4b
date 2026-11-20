use std::cell::RefCell;
use std::rc::Rc;

use elanfp_core::identity::{Identity, Pipe, Wiring, pipe_position, wired_as_verified};
use elanfp_core::{EP_CMD_IN, EP_CMD_OUT, EP_IMG_IN, PRODUCT_ID, Sensor, Transport, VENDOR_ID, Waiting};
use elanfp_engine::service::Device;
use elanfp_engine::session::{Session, sensor_fail};
use elanfp_engine::{BoxError, Explain, Fail, Msg, Reporter};
use xhcid_interface::{ConfigureEndpointsReq, PortId, PortTransferStatus, PortTransferStatusKind, XhciClientHandle, XhciClientHandleError, XhciEndpHandle};
use zeroize::Zeroizing;

const MAX_TRANSFER: usize = 65536;

#[derive(Debug)]
pub enum UsbError {
    Client(XhciClientHandleError),
    Stalled,
    Failed,
    Endpoint(u8),
    TooLarge(usize),
    Dead,
}

impl Explain for UsbError {
    fn explain(&self) -> Msg {
        match self {
            UsbError::Client(e) => Msg::new("usb-xhcid-failed").arg("cause", e),
            UsbError::Stalled => Msg::new("usb-stalled"),
            UsbError::Failed => Msg::new("usb-transfer-unknown"),
            UsbError::Endpoint(ep) => Msg::new("usb-endpoint-missing").arg("endpoint", format!("0x{ep:02x}")),
            UsbError::TooLarge(bytes) => Msg::new("usb-transfer-too-large").num("bytes", bytes).num("limit", MAX_TRANSFER),
            UsbError::Dead => Msg::new("usb-link-dead"),
        }
    }
}

struct Channel {
    handle: XhciEndpHandle,
    packet: usize,
}

struct Port {
    command_out: Channel,
    command_in: Channel,
    image_in: Channel,
    dead: bool,
}

fn transferred(status: PortTransferStatus) -> Result<usize, UsbError> {
    match status.kind {
        PortTransferStatusKind::Success | PortTransferStatusKind::ShortPacket => Ok(status.bytes_transferred as usize),
        PortTransferStatusKind::Stalled => Err(UsbError::Stalled),
        PortTransferStatusKind::Unknown => Err(UsbError::Failed),
    }
}

impl Port {
    fn carried(&mut self, outcome: Result<PortTransferStatus, XhciClientHandleError>) -> Result<usize, UsbError> {
        let moved = outcome.map_err(UsbError::Client).and_then(transferred);
        self.dead |= moved.is_err();
        moved
    }

    fn bulk_out(&mut self, ep: u8, data: &[u8]) -> Result<usize, UsbError> {
        if self.dead {
            return Err(UsbError::Dead);
        }
        if ep != EP_CMD_OUT {
            return Err(UsbError::Endpoint(ep));
        }
        if data.len() > MAX_TRANSFER {
            return Err(UsbError::TooLarge(data.len()));
        }
        let outcome = self.command_out.handle.transfer_write(data);
        self.carried(outcome)
    }

    fn bulk_in(&mut self, ep: u8, buf: &mut [u8]) -> Result<usize, UsbError> {
        if self.dead {
            return Err(UsbError::Dead);
        }
        let channel = match ep {
            EP_CMD_IN => &mut self.command_in,
            EP_IMG_IN => &mut self.image_in,
            _ => return Err(UsbError::Endpoint(ep)),
        };
        let request = buf.len().div_ceil(channel.packet).max(1) * channel.packet;
        if request > MAX_TRANSFER {
            return Err(UsbError::TooLarge(request));
        }
        let mut data = Zeroizing::new(vec![0u8; request]);
        let outcome = channel.handle.transfer_read(&mut data);
        let got = self.carried(outcome)?.min(request);
        let kept = got.min(buf.len());
        buf[..kept].copy_from_slice(&data[..kept]);
        Ok(got)
    }
}

#[derive(Clone)]
pub struct Link(Rc<RefCell<Port>>);

impl Transport for Link {
    type Error = UsbError;

    const WAITING: Waiting = Waiting::Unbounded;

    fn bulk_out(&mut self, ep: u8, data: &[u8], _timeout_ms: u32) -> Result<usize, UsbError> {
        self.0.borrow_mut().bulk_out(ep, data)
    }

    fn bulk_in(&mut self, ep: u8, buf: &mut [u8], _timeout_ms: u32) -> Result<usize, UsbError> {
        self.0.borrow_mut().bulk_in(ep, buf)
    }

    fn is_timeout(_: &UsbError) -> bool {
        false
    }
}

pub struct Reader {
    link: Link,
    vendor_id: u16,
    product_id: u16,
}

impl Reader {
    pub fn identify(&self) -> Result<Identity, BoxError> {
        let mut sensor = Sensor::new(self.link.clone());
        sensor.stop().map_err(sensor_fail::<Link>("00 0b"))?;
        Ok(sensor.identify(self.vendor_id, self.product_id).map_err(sensor_fail::<Link>("identify"))?)
    }
}

impl Device for Reader {
    type Transport = Link;

    fn open(&self, report: &mut dyn Reporter) -> Result<Session<Link>, BoxError> {
        Session::open(Sensor::new(self.link.clone()), self.vendor_id, self.product_id, report)
    }
}

fn usb_fail(command: &'static str) -> impl Fn(XhciClientHandleError) -> Fail {
    move |e| Fail::new("usb-error").arg("command", command).arg("detail", e)
}

pub fn attach(scheme: &str, port: PortId, interface: u8) -> Result<Reader, BoxError> {
    let handle = XhciClientHandle::new(scheme.to_string(), port).map_err(usb_fail("open"))?;
    let device = handle.get_standard_descs().map_err(usb_fail("descriptors"))?;
    if (device.vendor, device.product) != (VENDOR_ID, PRODUCT_ID) {
        return Err(Fail::new("sensor-not-found").into());
    }
    let unverified = || Fail::new("sensor-layout-unverified");
    let configuration = device.config_descs.first().ok_or_else(unverified)?;
    let only = configuration.interface_descs.first().ok_or_else(unverified)?;
    let pipes: Vec<Pipe> =
        only.endpoints.iter().map(|endpoint| Pipe { address: endpoint.address, bulk: endpoint.is_bulk(), packet: endpoint.max_packet_size }).collect();
    let wiring = Wiring {
        configurations: device.config_descs.len(),
        interfaces: configuration.interface_descs.len(),
        class: [only.class, only.sub_class, only.protocol],
        pipes: &pipes,
    };
    if only.number != interface || !wired_as_verified(&wiring) {
        return Err(unverified().into());
    }
    handle
        .configure_endpoints(&ConfigureEndpointsReq {
            config_desc: configuration.configuration_value,
            interface_desc: Some(interface),
            alternate_setting: Some(only.alternate_setting),
            hub_ports: None,
        })
        .map_err(usb_fail("configure"))?;
    let open = |address: u8| -> Result<Channel, Fail> {
        let position = pipe_position(address).ok_or_else(unverified)?;
        let number = u8::try_from(position + 1).map_err(|_| unverified())?;
        Ok(Channel { handle: handle.open_endpoint(number).map_err(usb_fail("endpoint"))?, packet: usize::from(pipes[position].packet) })
    };
    let port = Port { command_out: open(EP_CMD_OUT)?, command_in: open(EP_CMD_IN)?, image_in: open(EP_IMG_IN)?, dead: false };
    Ok(Reader { link: Link(Rc::new(RefCell::new(port))), vendor_id: device.vendor, product_id: device.product })
}
