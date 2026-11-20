use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use zbus::blocking::Connection;
use zbus::blocking::connection::Builder;
use zbus::zvariant::OwnedValue;

const PATIENCE: Duration = Duration::from_secs(20);

#[derive(Default)]
struct Seen {
    shown: Vec<Shown>,
    closed: Vec<u32>,
    issued: u32,
    markup: bool,
    changes_its_mind: bool,
}

struct Shown {
    app: String,
    replaces: u32,
    title: String,
    body: String,
    expires: i32,
    at: Instant,
}

struct Play {
    lines: Vec<String>,
    pace: Duration,
    stays: bool,
}

fn play(lines: &[&str]) -> Play {
    Play { lines: lines.iter().map(|line| line.to_string()).collect(), pace: Duration::from_millis(150), stays: true }
}

type Shared = Arc<Mutex<Seen>>;

fn lock(seen: &Shared) -> MutexGuard<'_, Seen> {
    seen.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

struct Board(Shared);

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Board {
    fn get_capabilities(&self) -> Vec<String> {
        if lock(&self.0).markup { vec!["body".to_string(), "body-markup".to_string()] } else { vec!["body".to_string()] }
    }

    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app: String,
        replaces: u32,
        _icon: String,
        title: String,
        body: String,
        _actions: Vec<String>,
        _hints: HashMap<String, OwnedValue>,
        expires: i32,
    ) -> u32 {
        let mut seen = lock(&self.0);
        seen.issued += 1;
        seen.shown.push(Shown { app, replaces, title, body, expires, at: Instant::now() });
        seen.markup ^= seen.changes_its_mind;
        seen.issued
    }

    fn close_notification(&self, id: u32) {
        lock(&self.0).closed.push(id);
    }
}

struct Stand {
    bus: Child,
    agent: Child,
    address: String,
    dir: PathBuf,
    seen: Shared,
    _board: Connection,
}

fn eventually(what: &str, mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(started.elapsed() < PATIENCE, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn own_name() -> String {
    let output = Command::new("id").arg("-un").output().unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

impl Stand {
    fn agent(address: &str, dir: &std::path::Path) -> Child {
        Command::new(env!("CARGO_BIN_EXE_elanfp-notify"))
            .env("DBUS_SESSION_BUS_ADDRESS", address)
            .env("ELANFP_SOCKET", dir.join("socket"))
            .env("ELANFP_SERVICE_USER", own_name())
            .env("LC_ALL", "en_US.UTF-8")
            .env_remove("LANGUAGE")
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    fn start(tag: &str, markup: bool, plays: Vec<Play>) -> Stand {
        let spawned = Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        let mut bus = spawned.expect("the agent tests need dbus-daemon");
        let mut address = String::new();
        BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address).unwrap();
        let address = address.trim_end().to_string();
        let seen = Arc::new(Mutex::new(Seen { markup, ..Seen::default() }));
        let board = Builder::address(address.as_str())
            .unwrap()
            .name("org.freedesktop.Notifications")
            .unwrap()
            .serve_at("/org/freedesktop/Notifications", Board(Arc::clone(&seen)))
            .unwrap()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("elanfp-notify-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("socket")).unwrap();
        std::thread::spawn(move || {
            let mut plays = plays.into_iter();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = String::new();
                if BufReader::new(&stream).read_line(&mut request).is_err() || !request.starts_with("WATCH ") || !request.trim_end().ends_with("fmt=keys") {
                    let _ = stream.write_all(b"ERR bad-request\n");
                    continue;
                }
                let Some(play) = plays.next() else { continue };
                for line in &play.lines {
                    std::thread::sleep(play.pace);
                    if stream.write_all(format!("{line}\n").as_bytes()).is_err() {
                        break;
                    }
                }
                if play.stays {
                    std::thread::spawn(move || {
                        std::thread::sleep(PATIENCE);
                        drop(stream);
                    });
                }
            }
        });
        let agent = Stand::agent(&address, &dir);
        Stand { bus, agent, address, dir, seen, _board: board }
    }

    fn bodies(&self) -> Vec<String> {
        lock(&self.seen).shown.iter().map(|shown| shown.body.clone()).collect()
    }

    fn agent_is_settled(&self) -> bool {
        let asked = self._board.call_method(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            Some("org.freedesktop.DBus"),
            "NameHasOwner",
            &("org.elanfp.Notify",),
        );
        asked.ok().and_then(|reply| reply.body().deserialize::<bool>().ok()).unwrap_or(false)
    }
}

impl Drop for Stand {
    fn drop(&mut self) {
        let _ = self.agent.kill();
        let _ = self.agent.wait();
        let _ = self.bus.kill();
        let _ = self.bus.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn a_request_from_root_is_shown_and_taken_down_when_it_ends() {
    let script = [
        "MSG watch-idle",
        "MSG watch-verify by=self route=direct pid=0 program=%3f",
        "MSG watch-over",
        "MSG watch-verify by=root route=direct pid=0 program=sudo",
        "MSG watch-over",
        "MSG watch-enroll route=bridge pid=0",
        "MSG watch-over",
        "MSG watch-verify by=root route=bridge pid=0 program=%3f",
        "MSG watch-over",
    ];
    let stand = Stand::start("plain", false, vec![play(&script)]);
    eventually("three cards are shown and closed", || lock(&stand.seen).closed.len() == 3);
    let seen = lock(&stand.seen);
    let titles: Vec<&str> = seen.shown.iter().map(|shown| shown.title.as_str()).collect();
    assert_eq!(titles, ["A fingerprint was requested", "Enrolling a finger", "A fingerprint was requested"]);
    assert!(seen.shown.iter().all(|shown| shown.app == "Fingerprint reader" && shown.replaces == 0));
    let expiries: Vec<i32> = seen.shown.iter().map(|shown| shown.expires).collect();
    assert_eq!(expiries, [0, 0, 0]);
    let asked = &seen.shown[0].body;
    assert!(asked.starts_with("\"sudo\", a program running as root, asks") && asked.ends_with("do not touch the reader."), "{asked}");
    assert!(!asked.contains("desktop bridge") && !asked.contains("own words"), "{asked}");
    let bridged = &seen.shown[2].body;
    assert!(bridged.starts_with("A program running as root asks") && bridged.contains("through the desktop bridge"), "{bridged}");
    assert_eq!(seen.closed, [1, 2, 3]);
}

#[test]
fn what_a_program_says_about_itself_cannot_draw_on_the_card() {
    let own = std::process::id();
    let named = format!("MSG watch-verify by=root route=direct pid={own} program=%3cb%3eloud%3c/b%3e%26");
    let script = [named.as_str(), "MSG watch-over", named.as_str(), "MSG watch-over", named.as_str()];
    let stand = Stand::start("markup", false, vec![play(&script)]);
    lock(&stand.seen).changes_its_mind = true;
    eventually("the cards are shown", || lock(&stand.seen).shown.len() == 3);
    let bodies = stand.bodies();
    assert!(bodies[0].contains("<b>loud</b>&\","), "{}", bodies[0]);
    assert!(bodies[1].contains("&lt;b&gt;loud&lt;/b&gt;&amp;\",") && !bodies[1].contains('<') && !bodies[1].contains('>'), "{}", bodies[1]);
    assert_eq!(bodies[2], bodies[0]);
    let by_root = unsafe { libc::getuid() } == 0;
    let own_name = std::env::current_exe().unwrap().file_name().unwrap().to_str().unwrap().to_string();
    assert!(bodies.iter().all(|body| body.contains("own words") == by_root && body.contains(&own_name) == by_root), "{bodies:?}");
}

#[test]
fn a_card_is_escaped_when_the_desktop_does_not_say_what_it_understands() {
    struct Mute(Shared);

    #[zbus::interface(name = "org.freedesktop.Notifications")]
    impl Mute {
        #[allow(clippy::too_many_arguments)]
        fn notify(
            &self,
            app: String,
            replaces: u32,
            _icon: String,
            title: String,
            body: String,
            _actions: Vec<String>,
            _hints: HashMap<String, OwnedValue>,
            expires: i32,
        ) -> u32 {
            let mut seen = lock(&self.0);
            seen.issued += 1;
            seen.shown.push(Shown { app, replaces, title, body, expires, at: Instant::now() });
            seen.issued
        }
    }

    let spawned = Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let mut bus = spawned.expect("the agent tests need dbus-daemon");
    let mut address = String::new();
    BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address).unwrap();
    let address = address.trim_end().to_string();
    let seen: Shared = Arc::default();
    let board = Builder::address(address.as_str())
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .serve_at("/org/freedesktop/Notifications", Mute(Arc::clone(&seen)))
        .unwrap()
        .build()
        .unwrap();
    let dir = std::env::temp_dir().join(format!("elanfp-notify-test-{}-mute", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let listener = UnixListener::bind(dir.join("socket")).unwrap();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else { return };
        let mut request = String::new();
        let _ = BufReader::new(&stream).read_line(&mut request);
        let _ = stream.write_all(b"MSG watch-verify by=root route=direct pid=0 program=%3ci%3e\n");
        std::thread::sleep(PATIENCE);
    });
    let agent = Stand::agent(&address, &dir);
    let stand = Stand { bus, agent, address, dir, seen, _board: board };
    eventually("the card is shown", || !lock(&stand.seen).shown.is_empty());
    let body = stand.bodies().remove(0);
    assert!(body.contains("&lt;i&gt;") && !body.contains('<'), "{body}");
}

#[test]
fn one_agent_is_enough_for_a_session() {
    let script = ["MSG watch-verify by=root route=direct pid=0 program=sudo", "MSG watch-over"];
    let stand = Stand::start("single", false, vec![play(&script), play(&script)]);
    eventually("the first agent has settled", || stand.agent_is_settled());
    let mut second = Stand::agent(&stand.address, &stand.dir);
    let started = Instant::now();
    let status = loop {
        if let Some(status) = second.try_wait().unwrap() {
            break status;
        }
        assert!(started.elapsed() < PATIENCE, "the second agent stayed");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
    eventually("the card is shown and closed", || lock(&stand.seen).closed.len() == 1);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(lock(&stand.seen).shown.len(), 1);
}

#[test]
fn a_dropped_connection_is_taken_up_again_at_once() {
    let first = Play { stays: false, ..play(&["MSG watch-verify by=root route=direct pid=0 program=sudo"]) };
    let second = play(&["MSG watch-verify by=root route=direct pid=0 program=login"]);
    let stand = Stand::start("again", false, vec![first, second]);
    eventually("both cards are shown", || lock(&stand.seen).shown.len() == 2);
    let seen = lock(&stand.seen);
    assert!(seen.shown[1].at.duration_since(seen.shown[0].at) < Duration::from_secs(3), "the agent waited before coming back");
    assert!(seen.shown[1].body.starts_with("\"login\""), "{}", seen.shown[1].body);
    assert_eq!(seen.closed, [1]);
}

#[test]
fn a_long_watch_is_not_cut_short_by_how_much_was_said() {
    let filler = format!("MSG watch-idle pad={}", "x".repeat(3900));
    let mut lines = vec![filler; 300];
    lines.push("MSG watch-verify by=root route=direct pid=0 program=sudo".to_string());
    let stand = Stand::start("long", false, vec![Play { lines, pace: Duration::ZERO, stays: true }]);
    eventually("the card after a megabyte of notices is shown", || lock(&stand.seen).shown.len() == 1);
}

#[test]
fn the_agent_leaves_when_the_session_ends() {
    let mut stand = Stand::start("leave", false, vec![play(&["MSG watch-idle"; 80])]);
    std::thread::sleep(Duration::from_millis(600));
    stand.bus.kill().unwrap();
    stand.bus.wait().unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = stand.agent.try_wait().unwrap() {
            break status;
        }
        assert!(started.elapsed() < PATIENCE, "the agent outlived its session");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(status.code(), Some(1));
}

#[test]
fn a_repeated_notice_changes_nothing_and_an_idle_beat_takes_a_stale_card_down() {
    let asked = "MSG watch-verify by=root route=direct pid=0 program=sudo";
    let stand = Stand::start("beat", false, vec![play(&[asked, asked, asked, "MSG watch-idle", asked, "MSG watch-over"])]);
    eventually("the card came back after the idle beat and was closed again", || lock(&stand.seen).closed.len() == 2);
    let seen = lock(&stand.seen);
    assert_eq!((seen.shown.len(), seen.closed.clone()), (2, vec![1, 2]));
}

#[test]
fn a_stranger_holding_the_name_does_not_silence_the_agent() {
    let spawned = Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
    let mut bus = spawned.expect("the agent tests need dbus-daemon");
    let mut address = String::new();
    BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address).unwrap();
    let address = address.trim_end().to_string();
    let seen = Arc::new(Mutex::new(Seen::default()));
    let board = Builder::address(address.as_str())
        .unwrap()
        .name("org.freedesktop.Notifications")
        .unwrap()
        .name("org.elanfp.Notify")
        .unwrap()
        .serve_at("/org/freedesktop/Notifications", Board(Arc::clone(&seen)))
        .unwrap()
        .build()
        .unwrap();
    let dir = std::env::temp_dir().join(format!("elanfp-notify-test-{}-squat", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let listener = UnixListener::bind(dir.join("socket")).unwrap();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else { return };
        let mut request = String::new();
        let _ = BufReader::new(&stream).read_line(&mut request);
        let _ = stream.write_all(b"MSG watch-verify by=root route=direct pid=0 program=sudo\n");
        std::thread::sleep(PATIENCE);
    });
    let agent = Stand::agent(&address, &dir);
    let stand = Stand { bus, agent, address, dir, seen, _board: board };
    eventually("the card is shown although the name was taken", || !lock(&stand.seen).shown.is_empty());
}

#[test]
fn after_a_refusal_the_agent_asks_again_soon() {
    let refused = Play { lines: vec!["BUSY watch-crowded".to_string()], pace: Duration::ZERO, stays: false };
    let stand = Stand::start("refused", false, vec![refused, play(&["MSG watch-verify by=root route=direct pid=0 program=sudo"])]);
    let started = Instant::now();
    eventually("the card is shown after the second attempt", || !lock(&stand.seen).shown.is_empty());
    assert!(started.elapsed() < Duration::from_secs(10), "{:?}", started.elapsed());
}

#[test]
fn a_service_that_restarts_is_found_again_within_seconds() {
    let first = Play { stays: false, ..play(&["MSG watch-verify by=root route=direct pid=0 program=sudo"]) };
    let stand = Stand::start("restart", false, vec![first, play(&["MSG watch-verify by=root route=direct pid=0 program=login"])]);
    let (socket, away) = (stand.dir.join("socket"), stand.dir.join("socket.away"));
    eventually("the first card is shown", || lock(&stand.seen).shown.len() == 1);
    std::fs::rename(&socket, &away).unwrap();
    eventually("the card is taken down when the service goes away", || lock(&stand.seen).closed.len() == 1);
    let gone = Instant::now();
    std::thread::sleep(Duration::from_millis(1500));
    std::fs::rename(&away, &socket).unwrap();
    eventually("the second card is shown", || lock(&stand.seen).shown.len() == 2);
    assert!(gone.elapsed() < Duration::from_secs(8), "{:?}", gone.elapsed());
}
