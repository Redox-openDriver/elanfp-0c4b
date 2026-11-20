mod bus;
mod relay;

use std::path::PathBuf;
use std::sync::Arc;

use elanfp_engine::{BoxError, Fail, Msg, POLICY, as_msg, catalog};

pub const BUS_NAME: &str = "net.reactivated.Fprint";
pub const MANAGER_PATH: &str = "/net/reactivated/Fprint/Manager";
pub const DEVICE_PATH: &str = "/net/reactivated/Fprint/Device/0";
pub const DEVICE_INTERFACE: &str = "net.reactivated.Fprint.Device";
pub const DEVICE_NAME: &str = "ELAN 04f3:0c4b";
pub const SCAN_TYPE: &str = "press";
pub const ANY_FINGER: &str = "any";
pub const SOCKET: &str = "/run/elanfp/socket";
pub const SERVICE_USER: &str = "elanfp";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    Logind,
    Any,
}

#[derive(Debug, Clone)]
pub struct Settings {
    pub socket: PathBuf,
    pub service_uid: u32,
    pub address: Option<String>,
    pub polkit: bool,
    pub console: Console,
    pub stages: i32,
}

impl Settings {
    fn from_environment() -> Result<Self, Fail> {
        let set = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
        let switch = |name: &str, on: &str, off: &str| match set(name).as_deref() {
            None => Ok(true),
            Some(value) if value == on => Ok(true),
            Some(value) if value == off => Ok(false),
            Some(value) => Err(Fail::new("fprint-bad-setting").arg("name", name).arg("value", value)),
        };
        let service_user = set("ELANFP_SERVICE_USER").unwrap_or_else(|| SERVICE_USER.to_string());
        Ok(Settings {
            socket: set("ELANFP_SOCKET").map_or_else(|| PathBuf::from(SOCKET), PathBuf::from),
            service_uid: elanfp_ipc::uid_of(&service_user).ok_or_else(|| Fail::new("fprint-no-service-account").arg("name", service_user))?,
            address: set("ELANFP_FPRINTD_ADDRESS"),
            polkit: switch("ELANFP_FPRINTD_POLKIT", "on", "off")?,
            console: if switch("ELANFP_FPRINTD_CONSOLE", "logind", "any")? { Console::Logind } else { Console::Any },
            stages: i32::try_from(POLICY.profile.enrolling.max_touches).unwrap_or(i32::MAX),
        })
    }
}

fn run() -> Result<(), BoxError> {
    let settings = Arc::new(Settings::from_environment()?);
    let connection = bus::serve(Arc::clone(&settings)).map_err(|e| Fail::new("fprint-bus").arg("detail", e))?;
    eprintln!("{}", catalog().render(&Msg::new("fprint-started").arg("name", BUS_NAME).arg("socket", settings.socket.display())));
    bus::watch(connection)
}

fn main() {
    unsafe { std::env::set_var(bus::POOL_VARIABLE, bus::POOL_THREADS) };
    if let Err(e) = elanfp_ipc::harden_process().map_err(|e| Fail::new("process-unprotected").arg("detail", e).into()).and_then(|()| run()) {
        let texts = catalog();
        eprintln!("{}", texts.render(&Msg::new("fatal").arg("text", texts.render(&as_msg(e.as_ref(), "error-internal")))));
        std::process::exit(1);
    }
}
