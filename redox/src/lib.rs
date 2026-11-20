pub mod keys;
pub mod scheme;
pub mod usb;

pub const SCHEME: &str = "fingerprint.elanfp";
pub const RESOURCE: &str = "request";

pub fn resource_path(scheme: &str) -> String {
    format!("/scheme/{scheme}/{RESOURCE}")
}
