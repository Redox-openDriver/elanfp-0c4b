use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use elanfp_engine::{BoxError, Catalog, Fail, Msg, as_msg, catalog};
use elanfp_ipc::{Event, MAX_REPLY_LINE, Reply, Request, Verb};
use zbus::blocking::Connection;
use zbus::fdo::RequestNameFlags;
use zbus::zvariant::Value;

const SOCKET: &str = "/run/elanfp/socket";
const SERVICE_USER: &str = "elanfp";
const ROOT: u32 = 0;
const STEP_TIMEOUT: Duration = Duration::from_secs(90);
const BUS_CALL_TIMEOUT: Duration = Duration::from_secs(10);
const RETRIES: [Duration; 4] = [Duration::from_millis(200), Duration::from_secs(1), Duration::from_secs(2), Duration::from_secs(5)];
const RETRY_WHEN_REFUSED: Duration = Duration::from_secs(60);
const UNTIL_CLOSED: i32 = 0;
const COMMAND_LIMIT: usize = 160;
const QUOTES: [char; 10] = ['"', '«', '»', '“', '”', '„', '‟', '‹', '›', '`'];
const UNNAMED: &str = "?";
const CRITICAL: u8 = 2;
const MARKUP: [&str; 3] = ["body-markup", "body-hyperlinks", "body-images"];
const GONE: &str = " (deleted)";
const ICON: &str = "dialog-password";
const OWN_NAME: &str = "org.elanfp.Notify";
const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Launch {
    command: String,
    cut: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Card {
    Asked { program: Option<String>, launch: Option<Launch>, bridged: bool },
    Enrolling,
    Over,
}

fn card(event: &Event, launch_of: impl Fn(u32) -> Option<Launch>) -> Option<Card> {
    let process = || event.text_of("pid").and_then(|pid| pid.parse().ok()).filter(|pid| *pid != 0);
    match event.key.as_str() {
        "watch-verify" if event.text_of("by") == Some("root") => Some(Card::Asked {
            program: event.text_of("program").filter(|name| *name != UNNAMED).map(str::to_string),
            launch: process().and_then(launch_of),
            bridged: event.text_of("route") == Some("bridge"),
        }),
        "watch-enroll" => Some(Card::Enrolling),
        "watch-over" | "watch-idle" => Some(Card::Over),
        _ => None,
    }
}

fn shortened(command: &[u8]) -> Option<Launch> {
    let said = String::from_utf8_lossy(command).replace('\0', " ");
    let line: String = said.split_whitespace().collect::<Vec<&str>>().join(" ").chars().map(|c| if QUOTES.contains(&c) { '\'' } else { c }).collect();
    let command: String = line.chars().take(COMMAND_LIMIT).collect();
    let cut = command.len() < line.len();
    (!command.is_empty()).then_some(Launch { command, cut })
}

fn launch_of(pid: u32) -> Option<Launch> {
    let process = format!("/proc/{pid}");
    if std::fs::metadata(&process).ok()?.uid() != ROOT {
        return None;
    }
    shortened(&std::fs::read(format!("{process}/cmdline")).ok()?)
}

fn escaped(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn asked(program: Option<String>, launch: Option<Launch>, bridged: bool) -> Vec<Msg> {
    let who = match program {
        Some(program) => Msg::new("notify-who").arg("program", program),
        None => Msg::new("notify-who-unnamed"),
    };
    let how =
        launch.map(
            |launch| if launch.cut { Msg::new("notify-how-cut").arg("command", launch.command) } else { Msg::new("notify-how").arg("command", launch.command) },
        );
    let via = bridged.then(|| Msg::new("notify-via-bridge"));
    [who, Msg::new("notify-advice")].into_iter().chain(via).chain(how).collect()
}

struct Desk {
    bus: Connection,
    texts: Catalog,
    shown: u32,
    about: String,
}

fn program_of(pid: u32) -> Option<String> {
    let program = std::fs::read_link(format!("/proc/{pid}/exe")).ok()?.to_string_lossy().into_owned();
    Some(program.strip_suffix(GONE).unwrap_or(&program).to_string())
}

impl Desk {
    fn open() -> Result<Option<Desk>, BoxError> {
        let unreachable = |e: zbus::Error| Fail::new("notify-no-bus").arg("detail", e);
        let bus = zbus::blocking::connection::Builder::session().map_err(unreachable)?.method_timeout(BUS_CALL_TIMEOUT).build().map_err(unreachable)?;
        let texts = catalog();
        match bus.request_name_with_flags(OWN_NAME, RequestNameFlags::DoNotQueue.into()) {
            Ok(_) => {}
            Err(zbus::Error::NameTaken) if Desk::held_by_another_agent(&bus) => return Ok(None),
            Err(zbus::Error::NameTaken) => eprintln!("{}", texts.render(&Msg::new("notify-name-taken").arg("name", OWN_NAME))),
            Err(e) => return Err(unreachable(e).into()),
        }
        Ok(Some(Desk { bus, texts, shown: 0, about: String::new() }))
    }

    fn held_by_another_agent(bus: &Connection) -> bool {
        let holder = bus.call_method(Some(BUS), BUS_PATH, Some(BUS), "GetConnectionUnixProcessID", &(OWN_NAME,));
        let holder = holder.ok().and_then(|reply| reply.body().deserialize::<u32>().ok()).and_then(program_of);
        holder.is_some() && holder == program_of(std::process::id())
    }

    fn alive(&self) -> bool {
        let pinged = self.bus.call_method(Some(BUS), BUS_PATH, Some("org.freedesktop.DBus.Peer"), "Ping", &());
        !matches!(pinged, Err(zbus::Error::InputOutput(_)))
    }

    fn takes_markup(&self) -> bool {
        let abilities = self.bus.call_method(Some(NOTIFICATIONS), NOTIFICATIONS_PATH, Some(NOTIFICATIONS), "GetCapabilities", &());
        abilities
            .ok()
            .and_then(|reply| reply.body().deserialize::<Vec<String>>().ok())
            .is_none_or(|list| list.iter().any(|ability| MARKUP.contains(&ability.as_str())))
    }

    fn show(&mut self, title: Msg, lines: &[Msg]) {
        let (app, title) = (self.texts.text("notify-app"), self.texts.render(&title));
        let body = lines.iter().map(|line| self.texts.render(line)).collect::<Vec<String>>().join("\n");
        let body = if self.takes_markup() { escaped(&body) } else { body };
        let hints: HashMap<&str, Value<'_>> = HashMap::from([("urgency", Value::U8(CRITICAL))]);
        let request = (app.as_str(), self.shown, ICON, title.as_str(), body.as_str(), Vec::<&str>::new(), hints, UNTIL_CLOSED);
        let reply = self.bus.call_method(Some(NOTIFICATIONS), NOTIFICATIONS_PATH, Some(NOTIFICATIONS), "Notify", &request);
        self.shown = reply.ok().and_then(|reply| reply.body().deserialize::<u32>().ok()).unwrap_or(0);
    }

    fn hide(&mut self) {
        self.about.clear();
        let shown = std::mem::take(&mut self.shown);
        if shown != 0 {
            let _ = self.bus.call_method(Some(NOTIFICATIONS), NOTIFICATIONS_PATH, Some(NOTIFICATIONS), "CloseNotification", &(shown,));
        }
    }

    fn react(&mut self, event: &Event) {
        let shown = card(event, launch_of);
        let about = event.encode();
        if matches!(shown, Some(Card::Asked { .. } | Card::Enrolling)) && self.shown != 0 && self.about == about {
            return;
        }
        match shown {
            Some(Card::Asked { program, launch, bridged }) => self.show(Msg::new("notify-verify-title"), &asked(program, launch, bridged)),
            Some(Card::Enrolling) => self.show(Msg::new("notify-enroll-title"), &[Msg::new("notify-enroll")]),
            Some(Card::Over) => return self.hide(),
            None => return,
        }
        self.about = about;
    }
}

struct Settings {
    socket: PathBuf,
    service: Option<u32>,
    own: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ended {
    Refused,
    Dropped { heard: bool },
}

fn trusted(stream: &UnixStream, settings: &Settings) -> bool {
    elanfp_ipc::peer_uid(stream).is_ok_and(|peer| peer == ROOT || peer == settings.own || Some(peer) == settings.service)
}

fn follow(settings: &Settings, desk: &mut Desk) -> Ended {
    let mut heard = false;
    let dropped = |heard| Ended::Dropped { heard };
    let Some(stream) = elanfp_ipc::connect(Path::new(&settings.socket), STEP_TIMEOUT).ok().filter(|stream| trusted(stream, settings)) else {
        return dropped(heard);
    };
    let Some(request) = Request::new(Verb::Watch, settings.own).keys(true).encode() else { return dropped(heard) };
    if (&stream).write_all(request.as_bytes()).is_err() {
        return dropped(heard);
    }
    let mut reader = BufReader::new(&stream);
    loop {
        let mut line = Vec::new();
        if !(&mut reader).take(MAX_REPLY_LINE as u64).read_until(b'\n', &mut line).is_ok_and(|read| read > 0) {
            return dropped(heard);
        }
        match String::from_utf8(line).ok().and_then(|text| Reply::parse(&text)) {
            Some(Reply::Message(text)) => {
                let Some(event) = Event::parse(&text) else { return dropped(heard) };
                heard = true;
                desk.react(&event);
                if !desk.alive() {
                    return dropped(heard);
                }
            }
            Some(Reply::Busy(_) | Reply::Err(_)) => return Ended::Refused,
            _ => return dropped(heard),
        }
    }
}

fn run() -> Result<(), BoxError> {
    let set = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let settings = Settings {
        socket: set("ELANFP_SOCKET").map_or_else(|| PathBuf::from(SOCKET), PathBuf::from),
        service: elanfp_ipc::uid_of(&set("ELANFP_SERVICE_USER").unwrap_or_else(|| SERVICE_USER.to_string())),
        own: unsafe { libc::getuid() },
    };
    let Some(mut desk) = Desk::open()? else { return Ok(()) };
    let mut fruitless = 0;
    while desk.alive() {
        let ended = follow(&settings, &mut desk);
        desk.hide();
        fruitless = if ended == (Ended::Dropped { heard: true }) { 0 } else { fruitless + 1 };
        std::thread::sleep(match RETRIES.get(fruitless) {
            Some(soon) => *soon,
            None if ended == Ended::Refused => RETRY_WHEN_REFUSED,
            None => RETRIES[RETRIES.len() - 1],
        });
    }
    Err(Fail::new("notify-bus-lost").into())
}

fn main() {
    if let Err(e) = run() {
        let texts = catalog();
        eprintln!("{}", texts.render(&Msg::new("notify-fatal").arg("text", texts.render(&as_msg(e.as_ref(), "error-internal")))));
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn verify(by: &str, pid: &str, program: &str, route: &str) -> Event {
        Event::new("watch-verify").with("by", by).with("route", route).with("pid", pid).with("program", program)
    }

    fn launched(command: &str) -> Launch {
        Launch { command: command.to_string(), cut: false }
    }

    #[test]
    fn only_requests_made_for_the_owner_by_root_become_cards() {
        let named = |pid: u32| (pid == 4242).then(|| launched("sudo pacman -Syu"));
        let asked = |program: Option<&str>, launch: Option<Launch>, bridged| Some(Card::Asked { program: program.map(str::to_string), launch, bridged });
        assert_eq!(card(&verify("root", "4242", "sudo", "direct"), named), asked(Some("sudo"), Some(launched("sudo pacman -Syu")), false));
        assert_eq!(card(&verify("root", "17", "?", "direct"), named), asked(None, None, false));
        assert_eq!(card(&verify("root", "0", "?", "bridge"), named), asked(None, None, true));
        assert_eq!(card(&verify("root", "not-a-number", "polkit-1", "direct"), named), asked(Some("polkit-1"), None, false));
        assert_eq!(card(&verify("self", "4242", "sudo", "direct"), named), None);
        assert_eq!(card(&Event::new("watch-verify"), named), None);
        assert_eq!(card(&Event::new("watch-enroll").with("pid", "0"), named), Some(Card::Enrolling));
        assert_eq!(card(&Event::new("watch-over"), named), Some(Card::Over));
        assert_eq!(card(&Event::new("watch-idle"), named), Some(Card::Over));
        assert_eq!(card(&Event::new("verified"), named), None);
    }

    #[test]
    fn a_launch_line_says_when_it_is_cut_and_cannot_draw_on_the_card() {
        assert_eq!(shortened(b"sudo\0pacman\0-Syu\0"), Some(launched("sudo pacman -Syu")));
        assert_eq!(shortened(b""), None);
        assert_eq!(shortened(b"\0\0"), None);
        let long = shortened(&[b"sudo\0".as_slice(), &[b'x'; 500]].concat()).unwrap();
        assert!(long.cut && long.command.chars().count() == COMMAND_LIMIT);
        assert_eq!(shortened(&[0xff, 0xfe, 0]), Some(launched("\u{fffd}\u{fffd}")));
        let padded = format!("sudo\0-k\0true\0{}\0It is safe:\n\ttouch \"the\" «reader»\0", " ".repeat(60));
        assert_eq!(shortened(padded.as_bytes()), Some(launched("sudo -k true It is safe: touch 'the' 'reader'")));
        assert_eq!(escaped("sh -c 'a<b && <a href=\"x\">'"), "sh -c 'a&lt;b &amp;&amp; &lt;a href=\"x\"&gt;'");
        assert_eq!(launch_of(std::process::id()).is_some(), unsafe { libc::getuid() } == ROOT);
        let texts = catalog();
        let lines = asked(Some("sudo".into()), Some(Launch { command: "sudo \u{1b}[2J\u{202e}evil".into(), cut: true }), true);
        let keys: Vec<&str> = lines.iter().map(|line| line.key.as_str()).collect();
        assert_eq!(keys, ["notify-who", "notify-advice", "notify-via-bridge", "notify-how-cut"]);
        let shown: String = lines.iter().map(|line| texts.render(line)).collect();
        assert!(!shown.chars().any(|c| c.is_control() || c == '\u{202e}'), "{shown:?}");
        let bare: Vec<String> = asked(None, None, false).iter().map(|line| line.key.to_string()).collect();
        assert_eq!(bare, ["notify-who-unnamed", "notify-advice"]);
    }
}
