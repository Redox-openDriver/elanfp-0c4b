use std::path::Path;
use std::time::Duration;

use elanfp_core::identity::{Pipe, Wiring, wired_as_verified};
use elanfp_core::{PRODUCT_ID, Sensor, Transport, VENDOR_ID, Waiting};
use elanfp_engine::{BoxError, Explain, Fail, Msg};
use nusb::descriptors::TransferType;
use nusb::transfer::{Buffer, Bulk, In, Out, TransferError};
use nusb::{Interface, MaybeFuture};

pub struct NusbTransport {
    iface: Interface,
}

#[derive(Debug)]
pub enum UsbError {
    Open(nusb::Error),
    Transfer(TransferError),
    Packet,
}

impl Explain for UsbError {
    fn explain(&self) -> Msg {
        match self {
            UsbError::Open(e) => Msg::new("usb-endpoint-unavailable").arg("cause", e),
            UsbError::Transfer(e) => Msg::new("usb-transfer-failed").arg("cause", e),
            UsbError::Packet => Msg::new("usb-endpoint-unavailable").arg("cause", 0),
        }
    }
}

impl Transport for NusbTransport {
    type Error = UsbError;

    const WAITING: Waiting = Waiting::Bounded;

    fn bulk_out(&mut self, ep: u8, data: &[u8], timeout_ms: u32) -> Result<usize, UsbError> {
        let mut endpoint = self.iface.endpoint::<Bulk, Out>(ep).map_err(UsbError::Open)?;
        let done = endpoint.transfer_blocking(Buffer::from(data.to_vec()), Duration::from_millis(timeout_ms.into()));
        if let Err(TransferError::Stall) = done.status {
            let _ = endpoint.clear_halt().wait();
        }
        done.status.map_err(UsbError::Transfer)?;
        Ok(done.actual_len)
    }

    fn bulk_in(&mut self, ep: u8, buf: &mut [u8], timeout_ms: u32) -> Result<usize, UsbError> {
        let mut endpoint = self.iface.endpoint::<Bulk, In>(ep).map_err(UsbError::Open)?;
        let packet = endpoint.max_packet_size();
        if packet == 0 {
            return Err(UsbError::Packet);
        }
        let request = buf.len().div_ceil(packet).max(1) * packet;
        let done = endpoint.transfer_blocking(endpoint.allocate(request), Duration::from_millis(timeout_ms.into()));
        match done.status {
            Ok(()) => {}
            Err(TransferError::Cancelled) if done.actual_len > 0 => {}
            Err(TransferError::Stall) => {
                let _ = endpoint.clear_halt().wait();
                return Err(UsbError::Transfer(TransferError::Stall));
            }
            Err(e) => return Err(UsbError::Transfer(e)),
        }
        let n = done.actual_len.min(buf.len());
        buf[..n].copy_from_slice(&done.buffer[..n]);
        Ok(done.actual_len)
    }

    fn is_timeout(error: &UsbError) -> bool {
        matches!(error, UsbError::Transfer(TransferError::Cancelled))
    }
}

fn usb_fail(command: &'static str) -> impl Fn(nusb::Error) -> Fail {
    move |e| Fail::new("usb-error").arg("command", command).arg("detail", e)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Port {
    BuiltIn,
    Pluggable,
    Unknown,
}

const CONNECT_TYPE: &str = "port/connect_type";
const HARDWIRED: &str = "hardwired";
const HOTPLUG: &str = "hotplug";
const ROOT_HUB: &str = "usb";

pub fn port_of(device: &Path) -> Port {
    let Ok(mut node) = std::fs::canonicalize(device) else { return Port::Unknown };
    let mut described = true;
    loop {
        match std::fs::read_to_string(node.join(CONNECT_TYPE)).as_deref().map(str::trim) {
            Ok(HARDWIRED) => {}
            Ok(HOTPLUG) => return Port::Pluggable,
            _ => described = false,
        }
        let Some(parent) = node.parent().map(Path::to_path_buf) else { return Port::Unknown };
        if parent.file_name().is_some_and(|name| name.to_string_lossy().starts_with(ROOT_HUB)) {
            return if described { Port::BuiltIn } else { Port::Unknown };
        }
        node = parent;
    }
}

pub fn find() -> Result<nusb::DeviceInfo, BoxError> {
    let candidates: Vec<(nusb::DeviceInfo, Port)> = nusb::list_devices()
        .wait()
        .map_err(usb_fail("list"))?
        .filter(|d| d.vendor_id() == VENDOR_ID && d.product_id() == PRODUCT_ID)
        .map(|device| {
            let port = port_of(device.sysfs_path());
            (device, port)
        })
        .collect();
    let seen = |wanted: Port| candidates.iter().any(|(_, port)| *port == wanted);
    let (undescribed, pluggable) = (seen(Port::Unknown), seen(Port::Pluggable));
    let mut built_in = candidates.into_iter().filter(|(_, port)| *port == Port::BuiltIn).map(|(device, _)| device);
    match (built_in.next(), built_in.next()) {
        (Some(device), None) => Ok(device),
        (Some(_), Some(_)) => Err(Fail::new("sensor-ambiguous").into()),
        (None, _) if undescribed => Err(Fail::new("sensor-port-unknown").into()),
        (None, _) if pluggable => Err(Fail::new("sensor-not-built-in").into()),
        (None, _) => Err(Fail::new("sensor-not-found").into()),
    }
}

pub struct Opened {
    pub sensor: Sensor<NusbTransport>,
    pub vendor_id: u16,
    pub product_id: u16,
}

fn laid_out_as_verified(device: &nusb::Device) -> bool {
    let Ok(active) = device.active_configuration() else { return false };
    let settings: Vec<_> = active.interface_alt_settings().collect();
    let Some(only) = settings.first() else { return false };
    let pipes: Vec<Pipe> = only
        .endpoints()
        .map(|endpoint| Pipe {
            address: endpoint.address(),
            bulk: endpoint.transfer_type() == TransferType::Bulk,
            packet: u16::try_from(endpoint.max_packet_size()).unwrap_or(u16::MAX),
        })
        .collect();
    wired_as_verified(&Wiring {
        configurations: device.configurations().count(),
        interfaces: settings.len(),
        class: [only.class(), only.subclass(), only.protocol()],
        pipes: &pipes,
    })
}

pub fn open() -> Result<Opened, BoxError> {
    let info = find()?;
    let device = info.open().wait().map_err(usb_fail("open"))?;
    if !laid_out_as_verified(&device) {
        return Err(Fail::new("sensor-layout-unverified").into());
    }
    let iface = device.detach_and_claim_interface(0).wait().map_err(usb_fail("claim"))?;
    Ok(Opened { sensor: Sensor::new(NusbTransport { iface }), vendor_id: info.vendor_id(), product_id: info.product_id() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str, ports: &[(&str, Option<&str>)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("elanfp-usb-test-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut node = root.join("pci0000:00").join("usb3");
        for (device, connect_type) in ports {
            node = node.join(device);
            std::fs::create_dir_all(node.join("port")).unwrap();
            if let Some(kind) = connect_type {
                std::fs::write(node.join(CONNECT_TYPE), format!("{kind}\n")).unwrap();
            }
        }
        node
    }

    type Case = (&'static str, &'static [(&'static str, Option<&'static str>)], Port);

    #[test]
    fn only_a_port_the_firmware_calls_hardwired_all_the_way_up_is_built_in() {
        let cases: [Case; 8] = [
            ("direct", &[("3-3", Some("hardwired"))], Port::BuiltIn),
            ("socket", &[("3-1", Some("hotplug"))], Port::Pluggable),
            ("silent", &[("3-2", None)], Port::Unknown),
            ("unused", &[("3-2", Some("not used"))], Port::Unknown),
            ("inner-hub", &[("3-4", Some("hardwired")), ("3-4.1", Some("hardwired"))], Port::BuiltIn),
            ("outer-hub", &[("3-1", Some("hotplug")), ("3-1.2", None)], Port::Pluggable),
            ("hub-claims-fixed", &[("3-1", Some("hotplug")), ("3-1.2", Some("hardwired"))], Port::Pluggable),
            ("undescribed-hub", &[("3-5", None), ("3-5.1", Some("hardwired"))], Port::Unknown),
        ];
        for (name, ports, expected) in cases {
            let device = tree(name, ports);
            assert_eq!(port_of(&device), expected, "{name}");
        }
        assert_eq!(port_of(Path::new("/nonexistent/elanfp/3-3")), Port::Unknown);
        let _ = std::fs::remove_dir_all(std::env::temp_dir().join(format!("elanfp-usb-test-{}-direct", std::process::id())));
    }
}
