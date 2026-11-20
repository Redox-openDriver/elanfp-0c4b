pub mod client;
pub mod dynamics;
#[cfg(any(test, feature = "simulator"))]
pub mod fake;
pub mod flows;
pub mod lockout;
pub mod service;
pub mod session;
pub mod store;

use std::time::Duration;

pub use drv_i18n::{Catalog, Fail, Msg, as_msg};
use elanfp_ipc::Detail;
pub use papilla;
use papilla::{Detector, Enrolling, Matching, Polarity, Profile};

pub type BoxError = Box<dyn std::error::Error>;

pub const PROFILE: Profile = Profile {
    name: "elan-04f3-0c4b",
    revision: 1,
    width: 80,
    height: 80,
    dpi: 500,
    polarity: Polarity::RidgesBright,
    detector: Detector::SMALL_SENSOR,
    matching: Matching::VALIDATED,
    enrolling: Enrolling { min_touches: 40, max_touches: 120, streak: 10, confident_score: 48, redundant_score: 300, adapt_limit: 60 },
    accept_score: 24,
}
.validated();

#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub profile: Profile,
    pub verify_attempts: usize,
    pub unusable_limit: usize,
    pub verify_wait: Duration,
    pub verify_budget: Duration,
    pub contention_quiet: Duration,
    pub idle_requests: u32,
    pub idle_quiet: Duration,
    pub idle_quiet_limit: Duration,
    pub enroll_wait: Duration,
    pub enroll_idle_limit: usize,
    pub enroll_budget: Duration,
    pub max_fingers: usize,
}

pub const POLICY: Policy = Policy {
    profile: PROFILE,
    verify_attempts: 2,
    unusable_limit: 4,
    verify_wait: Duration::from_secs(15),
    verify_budget: Duration::from_secs(60),
    contention_quiet: Duration::from_secs(30),
    idle_requests: 3,
    idle_quiet: Duration::from_secs(30),
    idle_quiet_limit: Duration::from_secs(600),
    enroll_wait: Duration::from_secs(20),
    enroll_idle_limit: 6,
    enroll_budget: Duration::from_secs(1200),
    max_fingers: 10,
};

pub const REGIONS: usize = 9;
pub const MAX_NAME: usize = 64;

pub const LOCALES: [(&str, &str); 4] =
    [("ru", include_str!("../../locales/ru.ftl")), ("en", include_str!("../../locales/en.ftl")), papilla::LOCALES[0], papilla::LOCALES[1]];

pub fn catalog() -> Catalog {
    Catalog::new(&LOCALES)
}

pub fn catalog_for(language: Option<&str>) -> Catalog {
    match language {
        Some(lang) => Catalog::for_languages(&LOCALES, &[lang.to_string()]),
        None => catalog(),
    }
}

pub struct Catalogs {
    journal: Catalog,
    languages: Vec<(&'static str, Catalog)>,
}

impl Catalogs {
    pub fn new() -> Self {
        let mut names: Vec<&'static str> = LOCALES.iter().map(|(language, _)| *language).collect();
        names.sort_unstable();
        names.dedup();
        Catalogs { journal: catalog(), languages: names.into_iter().map(|name| (name, catalog_for(Some(name)))).collect() }
    }

    pub fn journal(&self) -> &Catalog {
        &self.journal
    }

    pub fn get(&self, language: Option<&str>) -> &Catalog {
        let named = |wanted: &str| self.languages.iter().find(|(name, _)| *name == wanted).map(|(_, catalog)| catalog);
        match language {
            None => &self.journal,
            Some(wanted) => named(wanted).or_else(|| named(drv_i18n::FALLBACK)).unwrap_or(&self.journal),
        }
    }
}

impl Default for Catalogs {
    fn default() -> Self {
        Self::new()
    }
}

pub trait Explain {
    fn explain(&self) -> Msg;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Needed,
    Present,
    Away,
}

impl Presence {
    pub fn key(self) -> &'static str {
        match self {
            Presence::Needed => "state-finger-needed",
            Presence::Present => "state-finger-present",
            Presence::Away => "state-finger-away",
        }
    }
}

pub trait Reporter {
    fn say(&mut self, message: Msg) -> Result<(), BoxError>;

    fn alive(&mut self) -> bool {
        true
    }

    fn presence(&mut self, _state: Presence) {}

    fn rest(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

fn spread(event: &mut elanfp_ipc::Event, prefix: &str, message: &Msg) {
    for (name, value) in &message.args {
        let name = format!("{prefix}{}", name.replace('-', "_"));
        match value {
            drv_i18n::Value::Text(text) | drv_i18n::Value::Number(text) => event.args.push((name, Detail::Text(text.clone()))),
            drv_i18n::Value::Key(key) => event.args.push((name, Detail::Key(key.clone()))),
            drv_i18n::Value::Nested(inner) => {
                event.args.push((name.clone(), Detail::Key(inner.key.clone())));
                spread(event, &format!("{name}_"), inner);
            }
        }
    }
}

pub fn event(message: &Msg) -> elanfp_ipc::Event {
    let mut event = elanfp_ipc::Event::new(&message.key);
    spread(&mut event, "", message);
    event
}

pub struct Console {
    pub catalog: Catalog,
}

impl Console {
    pub fn new() -> Self {
        Console { catalog: catalog() }
    }

    pub fn print(&self, message: &Msg) {
        println!("{}", self.catalog.render(message));
    }
}

impl Default for Console {
    fn default() -> Self {
        Self::new()
    }
}

impl Reporter for Console {
    fn say(&mut self, message: Msg) -> Result<(), BoxError> {
        self.print(&message);
        Ok(())
    }
}

pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_NAME
        && bytes[0].is_ascii_alphanumeric()
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub fn check_name(name: &str) -> Result<(), Fail> {
    if valid_name(name) { Ok(()) } else { Err(Fail::new("finger-name-invalid")) }
}

pub fn region_key(n: usize) -> String {
    format!("region-{}", n % REGIONS)
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

pub fn library_fail(key: &'static str, args: Vec<(&'static str, String)>) -> Fail {
    Fail::new(key).args(args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_carry_nested_messages_and_tell_keys_from_text() {
        let cause = Msg::new("usb-transfer-failed").arg("cause", "stall @ep 0x82");
        let message = Msg::new("usb-error").arg("command", "00 09").nest("detail", cause).sub("hint", "reject-partial").arg("mail", "@home");
        let event = event(&message);
        assert_eq!((event.key_of("detail"), event.text_of("detail_cause")), (Some("usb-transfer-failed"), Some("stall @ep 0x82")));
        assert_eq!((event.key_of("hint"), event.text_of("hint")), (Some("reject-partial"), None));
        assert_eq!((event.text_of("mail"), event.key_of("mail")), (Some("@home"), None));
        assert_eq!(elanfp_ipc::Event::parse(&event.encode()), Some(event));
    }

    #[test]
    fn names_are_safe_path_components() {
        for good in ["right-index", "a", "L.1_x", "0finger"] {
            assert!(valid_name(good), "{good}");
        }
        let long = "a".repeat(65);
        for bad in ["", ".", "..", "-x", "_x", "a/b", "a b", "палец", "a\n", "a\0", long.as_str()] {
            assert!(!valid_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn profile_meets_the_calibrated_margins() {
        assert_eq!(PROFILE.check(), Ok(()));
        const { assert!(PROFILE.enrolling.confident_score >= 2 * PROFILE.accept_score) };
        assert_eq!(region_key(REGIONS + 2), "region-2");
    }
}
