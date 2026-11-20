use std::convert::Infallible;

use elanfp_engine::service::{Access, Service};
use elanfp_engine::store::ALGORITHM;
use elanfp_engine::{BoxError, Catalogs, Fail, Msg, POLICY, as_msg, catalog, hex};
use elanfp_redox::scheme::{Claim, Daemon};
use elanfp_redox::{SCHEME, keys, resource_path, usb};
use xhcid_interface::PortId;

fn run() -> Result<Infallible, BoxError> {
    let mut args = std::env::args().skip(1);
    let usage = || Fail::new("daemon-usage-redox");
    let controller = args.next().ok_or_else(usage)?;
    let port: PortId = args.next().and_then(|text| text.parse().ok()).ok_or_else(usage)?;
    let interface: u8 = args.next().and_then(|text| text.parse().ok()).ok_or_else(usage)?;
    let reader = usb::attach(&controller, port, interface)?;
    let identity = reader.identify()?;
    let claim = Claim::new(SCHEME)?;
    let service = Service::new(reader, keys::system_store()?, POLICY, Access::System, Catalogs::new());
    let mut daemon = Daemon::new(claim, service);
    daemon.service().log(
        Msg::new("log-started-redox")
            .arg("path", resource_path(SCHEME))
            .arg("controller", controller)
            .arg("port", port)
            .arg("firmware", hex(&identity.firmware))
            .arg("algorithm", ALGORITHM),
    );
    daemon.serve()
}

fn main() {
    let journal = catalog();
    let Err(e) = run();
    eprintln!("{}", journal.render(&Msg::new("fatal").arg("text", journal.render(&as_msg(e.as_ref(), "error-internal")))));
    std::process::exit(1)
}
