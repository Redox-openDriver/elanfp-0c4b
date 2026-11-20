use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Duration;

use elanfp_ipc::{Bounded, Event, MAX_REPLY_LINE, MAX_REPLY_TOTAL, Reply, Request, Verb};

pub const STEP_TIMEOUT: Duration = Duration::from_secs(120);
pub const SHORT_TOTAL: Duration = Duration::from_secs(30);
pub const VERIFY_TOTAL: Duration = Duration::from_secs(180);
pub const ENROLL_TOTAL: Duration = Duration::from_secs(1500);
const ROOT: u32 = 0;
pub const STAGE_PASSED: &str = "enroll-stage-passed";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok(Event),
    No(Event),
    Busy,
    Err(Event),
    Lost,
}

pub fn open(socket: &Path, service_uid: u32) -> std::io::Result<UnixStream> {
    let stream = elanfp_ipc::connect(socket, STEP_TIMEOUT)?;
    match elanfp_ipc::peer_uid(&stream)? {
        peer if peer == service_uid || peer == ROOT => Ok(stream),
        _ => Err(std::io::ErrorKind::PermissionDenied.into()),
    }
}

pub fn converse(mut stream: UnixStream, request: &Request, total: Duration, mut on_event: impl FnMut(&Event) -> bool) -> Outcome {
    let Some(line) = request.encode() else { return Outcome::Lost };
    if stream.write_all(line.as_bytes()).is_err() {
        return Outcome::Lost;
    }
    let mut reader = BufReader::new(Bounded::new(stream, STEP_TIMEOUT, total).take(MAX_REPLY_TOTAL));
    loop {
        let mut line = Vec::new();
        match (&mut reader).take(MAX_REPLY_LINE as u64).read_until(b'\n', &mut line) {
            Ok(read) if read > 0 => {}
            _ => return Outcome::Lost,
        }
        let Some(reply) = String::from_utf8(line).ok().and_then(|text| Reply::parse(&text)) else { return Outcome::Lost };
        let parsed = |text: &str| Event::parse(text);
        match reply {
            Reply::Message(text) => match parsed(&text) {
                Some(event) if on_event(&event) => {}
                Some(_) => return Outcome::Lost,
                None => return Outcome::Lost,
            },
            Reply::Ok(text) => return parsed(&text).map_or(Outcome::Lost, Outcome::Ok),
            Reply::No(text) => return parsed(&text).map_or(Outcome::Lost, Outcome::No),
            Reply::Err(text) => return parsed(&text).map_or(Outcome::Lost, Outcome::Err),
            Reply::Busy(_) => return Outcome::Busy,
        }
    }
}

pub fn ask(socket: &Path, service_uid: u32, request: &Request) -> Outcome {
    match open(socket, service_uid) {
        Ok(stream) => converse(stream, request, SHORT_TOTAL, |_| true),
        Err(_) => Outcome::Lost,
    }
}

pub fn stored_fingers(socket: &Path, service_uid: u32, uid: u32, acting: u32) -> Result<Vec<String>, Outcome> {
    let request = Request::new(Verb::List, uid).acting(Some(acting)).keys(true);
    match ask(socket, service_uid, &request) {
        Outcome::Ok(event) => Ok(event.text_of("fingers").map(|list| list.split(", ").map(str::to_string).collect()).unwrap_or_default()),
        other => Err(other),
    }
}

pub fn delete(socket: &Path, service_uid: u32, uid: u32, finger: &str) -> Outcome {
    ask(socket, service_uid, &Request::new(Verb::Delete, uid).finger(Some(finger.to_string())).acting(Some(ROOT)).keys(true))
}

pub fn verify_status(event: &Event) -> Option<&'static str> {
    match event.key.as_str() {
        "touch-finger-on-sensor" => Some("verify-remove-and-retry"),
        "verify-bad-frame" => Some(match event.key_of("reason") {
            Some("reject-partial") => "verify-finger-not-centered",
            _ => "verify-retry-scan",
        }),
        _ => None,
    }
}

pub fn enroll_status(event: &Event) -> Option<&'static str> {
    match event.key.as_str() {
        "enroll-accepted" => Some(STAGE_PASSED),
        "enroll-same-place" => Some("enroll-finger-not-centered"),
        "touch-finger-on-sensor" => Some("enroll-remove-and-retry"),
        "touch-missed-retry" | "papilla-enroll-featureless" => Some("enroll-retry-scan"),
        "touch-rejected-retry" => Some(match event.key_of("reason") {
            Some("reject-partial") => "enroll-finger-not-centered",
            Some("reject-moving") => "enroll-remove-and-retry",
            _ => "enroll-retry-scan",
        }),
        _ => None,
    }
}

pub fn enroll_failure(event: &Event) -> &'static str {
    match event.key.as_str() {
        "finger-exists" => "enroll-duplicate",
        "too-many-fingers" => "enroll-data-full",
        "sensor-not-found" | "sensor-not-built-in" | "sensor-port-unknown" | "sensor-ambiguous" | "sensor-layout-unverified" | "usb-error" | "usb-timeout" => {
            "enroll-disconnected"
        }
        _ => "enroll-failed",
    }
}

pub fn verify_failure(event: &Event) -> &'static str {
    match event.key.as_str() {
        "sensor-not-found" | "sensor-not-built-in" | "sensor-port-unknown" | "sensor-ambiguous" | "sensor-layout-unverified" | "usb-error" | "usb-timeout" => {
            "verify-disconnected"
        }
        _ => "verify-unknown-error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_events_map_to_the_statuses_desktops_know() {
        let bad = |reason: &str| Event::new("verify-bad-frame").with_key("reason", reason);
        assert_eq!(verify_status(&bad("reject-partial")), Some("verify-finger-not-centered"));
        assert_eq!(verify_status(&bad("reject-ridges")), Some("verify-retry-scan"));
        assert_eq!(verify_status(&Event::new("verify-bad-frame").with("reason", "reject-partial")), Some("verify-retry-scan"));
        assert_eq!(verify_status(&Event::new("verify-prompt")), None);
        assert_eq!(enroll_status(&Event::new("enroll-accepted")), Some(STAGE_PASSED));
        assert_eq!(enroll_status(&Event::new("enroll-same-place")), Some("enroll-finger-not-centered"));
        assert_eq!(enroll_status(&Event::new("touch-rejected-retry").with_key("reason", "reject-moving")), Some("enroll-remove-and-retry"));
        assert_eq!(enroll_status(&Event::new("enroll-prompt")), None);
        assert_eq!(enroll_failure(&Event::new("finger-exists")), "enroll-duplicate");
        assert_eq!(enroll_failure(&Event::new("too-many-fingers")), "enroll-data-full");
        assert_eq!(enroll_failure(&Event::new("anything-else")), "enroll-failed");
        assert_eq!(verify_failure(&Event::new("sensor-not-found")), "verify-disconnected");
        assert_eq!(verify_failure(&Event::new("locked")), "verify-unknown-error");
    }

    #[test]
    fn a_conversation_ends_on_the_final_line_or_when_the_peer_misbehaves() {
        let talk = |script: &'static [u8]| {
            let (client, mut server) = UnixStream::pair().unwrap();
            let writer = std::thread::spawn(move || {
                let _ = server.read(&mut [0u8; 128]);
                let _ = server.write_all(script);
            });
            let mut seen = Vec::new();
            let outcome = converse(client, &Request::new(Verb::Verify, 1000).keys(true), SHORT_TOTAL, |event| {
                seen.push(event.key.clone());
                true
            });
            writer.join().unwrap();
            (outcome, seen)
        };
        assert_eq!(
            talk(b"MSG verify-prompt attempt=1\nOK verified finger=right-index\n"),
            (Outcome::Ok(Event::new("verified").with("finger", "right-index")), vec!["verify-prompt".to_string()])
        );
        assert_eq!(talk(b"NO not-verified\n").0, Outcome::No(Event::new("not-verified")));
        assert_eq!(talk(b"ERR no-touch\n").0, Outcome::Err(Event::new("no-touch")));
        assert_eq!(talk(b"BUSY busy\n").0, Outcome::Busy);
        assert_eq!(talk(b"ERR locked minutes=3\n").0, Outcome::Err(Event::new("locked").with("minutes", "3")));
        for broken in [&b""[..], b"MSG not a key\n", b"OK Not-A-Key\n", b"WHAT\n", b"MSG half"] {
            assert_eq!(talk(broken).0, Outcome::Lost, "{broken:?}");
        }
    }
}
