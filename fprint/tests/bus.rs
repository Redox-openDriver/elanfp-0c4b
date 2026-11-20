use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use elanfp_ipc::Event;
use zbus::blocking::connection::Builder;
use zbus::blocking::{Connection, MessageIterator};
use zbus::fdo::RequestNameFlags;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const NAME: &str = "net.reactivated.Fprint";
const DEVICE: &str = "/net/reactivated/Fprint/Device/0";
const INTERFACE: &str = "net.reactivated.Fprint.Device";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const BUS: &str = "org.freedesktop.DBus";
const BUS_PATH: &str = "/org/freedesktop/DBus";
const SESSION: &str = "/org/freedesktop/login1/session/c1";
const VERIFY: &str = "org.elanfp.verify";
const ENROLL: &str = "org.elanfp.enroll";
const OTHERS: &str = "org.elanfp.others";
const DENIED: &str = "net.reactivated.Fprint.Error.PermissionDenied";
const NOT_CLAIMED: &str = "net.reactivated.Fprint.Error.ClaimDevice";
const IN_USE: &str = "net.reactivated.Fprint.Error.AlreadyInUse";
const NO_PRINTS: &str = "net.reactivated.Fprint.Error.NoEnrolledPrints";
const NO_ACTION: &str = "net.reactivated.Fprint.Error.NoActionInProgress";
const BAD_FINGER: &str = "net.reactivated.Fprint.Error.InvalidFingername";
const INDEX: &str = "right-index-finger";
const THUMB: &str = "left-thumb";
const PATIENCE: Duration = Duration::from_secs(20);
const QUIET: Duration = Duration::from_millis(400);
const STAGES: usize = 120;
const PER_SENDER: usize = 4;
const PER_USER: usize = 8;
const MANAGER: &str = "/net/reactivated/Fprint/Manager";
const MANAGER_INTERFACE: &str = "net.reactivated.Fprint.Manager";

#[derive(Clone)]
enum Step {
    Say(String),
    Gate,
}

fn say(line: &str) -> Step {
    Step::Say(line.to_string())
}

struct Script {
    fingers: Vec<String>,
    verify: Vec<Vec<Step>>,
    enroll: Vec<Step>,
    requests: Vec<String>,
    open: bool,
    granted: Vec<&'static str>,
    asked: Vec<String>,
    remote: bool,
    sitting: u32,
    class: &'static str,
    held: bool,
    broken: bool,
    ended: Vec<String>,
    gated_list: bool,
}

type Shared = Arc<Mutex<Script>>;

fn lock(script: &Shared) -> MutexGuard<'_, Script> {
    script.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn own_uid() -> u32 {
    unsafe { libc::getuid() }
}

fn own_name() -> String {
    let output = Command::new("id").arg("-un").output().unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn eventually(what: &str, mut ready: impl FnMut() -> bool) {
    let started = Instant::now();
    while !ready() {
        assert!(started.elapsed() < PATIENCE * 3, "timed out: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn answer(script: &Shared, request: &str) -> Vec<Step> {
    let mut script = lock(script);
    script.requests.push(request.to_string());
    let mut words = request.split(' ');
    match words.next() {
        Some("LIST") => {
            let listed = match script.fingers.is_empty() {
                true => say("OK list-empty"),
                false => Step::Say(format!("OK {}", Event::new("list").with("fingers", &script.fingers.join(", ")).encode())),
            };
            if script.gated_list { vec![Step::Gate, listed] } else { vec![listed] }
        }
        Some("VERIFY") if script.verify.len() > 1 => script.verify.remove(0),
        Some("VERIFY") => script.verify.first().cloned().unwrap_or_default(),
        Some("ENROLL") => script.enroll.clone(),
        Some("DELETE") => {
            let finger = words.nth(1).unwrap_or_default().to_string();
            script.fingers.retain(|stored| *stored != finger);
            vec![say("OK deleted")]
        }
        _ => vec![say("ERR bad-request")],
    }
}

fn serve(listener: UnixListener, script: Shared) {
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { break };
        let script = Arc::clone(&script);
        std::thread::spawn(move || {
            let mut request = String::new();
            if BufReader::new(&stream).read_line(&mut request).is_err() {
                return;
            }
            let asked = request.trim_end().to_string();
            for step in answer(&script, &asked) {
                match step {
                    Step::Gate => {
                        let started = Instant::now();
                        while !lock(&script).open && started.elapsed() < PATIENCE {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                    }
                    Step::Say(line) => {
                        if stream.write_all(format!("{line}\n").as_bytes()).is_err() {
                            lock(&script).ended.push(format!("hangup:{asked}"));
                            return;
                        }
                    }
                }
            }
            lock(&script).ended.push(format!("done:{asked}"));
        });
    }
}

type Verdict = (bool, bool, HashMap<String, String>);

struct Authority(Shared);

#[zbus::interface(name = "org.freedesktop.PolicyKit1.Authority")]
impl Authority {
    fn check_authorization(
        &self,
        subject: (String, HashMap<String, OwnedValue>),
        action_id: String,
        _details: HashMap<String, String>,
        _flags: u32,
        _cancellation_id: String,
    ) -> zbus::fdo::Result<(Verdict,)> {
        let named = subject.1.get("name").and_then(|name| String::try_from(name.clone()).ok());
        if subject.0 != "system-bus-name" || !named.is_some_and(|name| name.starts_with(':')) {
            return Err(zbus::fdo::Error::InvalidArgs("the subject is not a bus connection".into()));
        }
        lock(&self.0).asked.push(action_id.clone());
        let started = Instant::now();
        while lock(&self.0).held && started.elapsed() < PATIENCE {
            std::thread::sleep(Duration::from_millis(5));
        }
        let script = lock(&self.0);
        if script.broken {
            return Err(zbus::fdo::Error::Failed("the authority is out of order".into()));
        }
        Ok(((script.granted.iter().any(|action| *action == action_id), false, HashMap::new()),))
    }
}

struct Seat;

#[zbus::interface(name = "org.freedesktop.login1.Seat")]
impl Seat {
    #[zbus(property)]
    fn active_session(&self) -> (String, OwnedObjectPath) {
        ("c1".to_string(), OwnedObjectPath::try_from(SESSION).unwrap())
    }
}

struct Sitting(Shared);

#[zbus::interface(name = "org.freedesktop.login1.Session")]
impl Sitting {
    #[zbus(property)]
    fn remote(&self) -> bool {
        lock(&self.0).remote
    }

    #[zbus(property)]
    fn user(&self) -> (u32, OwnedObjectPath) {
        (lock(&self.0).sitting, OwnedObjectPath::try_from("/org/freedesktop/login1/user/self").unwrap())
    }

    #[zbus(property)]
    fn class(&self) -> String {
        lock(&self.0).class.to_string()
    }
}

struct Stand {
    bus: Child,
    bridge: Child,
    address: String,
    dir: PathBuf,
    script: Shared,
    _system: Connection,
}

impl Stand {
    fn start(console: &str) -> Stand {
        Stand::start_with(console, true)
    }

    fn start_with(console: &str, authority: bool) -> Stand {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let spawned = Command::new("dbus-daemon").args(["--session", "--nofork", "--print-address"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn();
        let mut bus = spawned.expect("the bridge tests need dbus-daemon");
        let mut address = String::new();
        BufReader::new(bus.stdout.take().unwrap()).read_line(&mut address).unwrap();
        let address = address.trim_end().to_string();
        let script = Arc::new(Mutex::new(Script {
            fingers: vec![INDEX.to_string()],
            verify: Vec::new(),
            enroll: Vec::new(),
            requests: Vec::new(),
            open: false,
            granted: vec![VERIFY],
            asked: Vec::new(),
            remote: false,
            sitting: own_uid(),
            class: "user",
            held: false,
            broken: false,
            ended: Vec::new(),
            gated_list: false,
        }));
        let system = Builder::address(address.as_str())
            .unwrap()
            .name(if authority { "org.freedesktop.PolicyKit1" } else { "org.example.NoAuthorityHere" })
            .unwrap()
            .name("org.freedesktop.login1")
            .unwrap()
            .serve_at("/org/freedesktop/PolicyKit1/Authority", Authority(Arc::clone(&script)))
            .unwrap()
            .serve_at("/org/freedesktop/login1/seat/seat0", Seat)
            .unwrap()
            .serve_at(SESSION, Sitting(Arc::clone(&script)))
            .unwrap()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join(format!("elanfp-fprint-test-{}-{}", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let listener = UnixListener::bind(dir.join("socket")).unwrap();
        let served = Arc::clone(&script);
        std::thread::spawn(move || serve(listener, served));
        let bridge = Command::new(env!("CARGO_BIN_EXE_elanfp-fprintd"))
            .env("ELANFP_FPRINTD_ADDRESS", &address)
            .env("ELANFP_SOCKET", dir.join("socket"))
            .env("ELANFP_FPRINTD_CONSOLE", console)
            .env("ELANFP_FPRINTD_POLKIT", "on")
            .env("ELANFP_SERVICE_USER", own_name())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stand = Stand { bus, bridge, address, dir, script, _system: system };
        let probe = stand.connect();
        eventually("the bridge takes its name", || {
            probe.call_method(Some(BUS), BUS_PATH, Some(BUS), "NameHasOwner", &(NAME,)).ok().and_then(|reply| reply.body().deserialize::<bool>().ok())
                == Some(true)
        });
        stand
    }

    fn connect(&self) -> Connection {
        Builder::address(self.address.as_str()).unwrap().build().unwrap()
    }

    fn script(&self) -> MutexGuard<'_, Script> {
        lock(&self.script)
    }

    fn requests(&self) -> Vec<String> {
        self.script().requests.clone()
    }
}

impl Drop for Stand {
    fn drop(&mut self) {
        let _ = self.bridge.kill();
        let _ = self.bridge.wait();
        let _ = self.bus.kill();
        let _ = self.bus.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn failure(error: zbus::Error) -> String {
    match error {
        zbus::Error::MethodError(name, _, _) => name.to_string(),
        other => other.to_string(),
    }
}

fn describe(message: &zbus::Message) -> Option<(String, String)> {
    let header = message.header();
    if header.message_type() != zbus::message::Type::Signal {
        return None;
    }
    let member = header.member()?.to_string();
    let body = message.body();
    let text = match member.as_str() {
        "PropertiesChanged" => {
            let (_, changed, _) = body.deserialize::<(String, HashMap<String, Value<'_>>, Vec<String>)>().ok()?;
            let on = |name: &str| matches!(changed.get(name), Some(Value::Bool(true)));
            match (on("finger-needed"), on("finger-present")) {
                (true, _) => "needed",
                (_, true) => "present",
                _ => "away",
            }
            .to_string()
        }
        "VerifyFingerSelected" => body.deserialize::<(String,)>().ok()?.0,
        _ => {
            let (status, done) = body.deserialize::<(String, bool)>().ok()?;
            format!("{status} {done}")
        }
    };
    Some((member, text))
}

struct Client {
    connection: Connection,
    signals: Receiver<(String, String)>,
}

impl Client {
    fn join(stand: &Stand) -> Client {
        let connection = stand.connect();
        connection.call_method(Some(BUS), BUS_PATH, Some(BUS), "AddMatch", &(format!("type='signal',path='{DEVICE}'"),)).unwrap();
        let (sender, signals) = channel();
        let incoming = MessageIterator::from(&connection);
        std::thread::spawn(move || {
            for message in incoming.flatten() {
                if let Some(signal) = describe(&message)
                    && sender.send(signal).is_err()
                {
                    break;
                }
            }
        });
        Client { connection, signals }
    }

    fn with(&self, method: &str, argument: &str) -> Result<zbus::Message, String> {
        self.connection.call_method(Some(NAME), DEVICE, Some(INTERFACE), method, &(argument,)).map_err(failure)
    }

    fn plain(&self, method: &str) -> Result<zbus::Message, String> {
        self.connection.call_method(Some(NAME), DEVICE, Some(INTERFACE), method, &()).map_err(failure)
    }

    fn property(&self, name: &str) -> OwnedValue {
        let reply = self.connection.call_method(Some(NAME), DEVICE, Some(PROPERTIES), "Get", &(INTERFACE, name)).unwrap();
        reply.body().deserialize::<OwnedValue>().unwrap()
    }

    fn flag(&self, name: &str) -> bool {
        bool::try_from(self.property(name)).unwrap()
    }

    fn listed(&self, user: &str) -> Result<Vec<String>, String> {
        self.with("ListEnrolledFingers", user).map(|reply| reply.body().deserialize::<Vec<String>>().unwrap())
    }

    fn signal(&self) -> (String, String) {
        self.signals.recv_timeout(PATIENCE).expect("a signal from the bridge")
    }

    fn until_done(&self) -> Vec<(String, String)> {
        let mut seen = Vec::new();
        loop {
            let signal = self.signal();
            let last = signal.1.ends_with(" true");
            seen.push(signal);
            if last {
                return seen;
            }
        }
    }

    fn hears_nothing(&self) -> bool {
        self.signals.recv_timeout(QUIET).is_err()
    }
}

fn signal(member: &str, text: &str) -> (String, String) {
    (member.to_string(), text.to_string())
}

#[test]
fn a_verification_is_reported_to_its_claimant_and_to_nobody_else() {
    let stand = Stand::start("logind");
    stand.script().verify = vec![vec![
        say("MSG state-finger-needed"),
        Step::Gate,
        say("MSG state-finger-present"),
        say("MSG state-finger-away"),
        say(&format!("OK verified finger={INDEX}")),
    ]];
    let (claimant, onlooker) = (Client::join(&stand), Client::join(&stand));
    assert_eq!(claimant.plain("VerifyStop").unwrap_err(), NOT_CLAIMED);
    claimant.with("Claim", "").unwrap();
    assert_eq!(claimant.with("VerifyStart", "no-such-finger").unwrap_err(), BAD_FINGER);
    assert_eq!(claimant.with("VerifyStart", THUMB).unwrap_err(), NO_PRINTS);
    claimant.with("VerifyStart", "any").unwrap();
    assert_eq!(claimant.signal(), signal("VerifyFingerSelected", "any"));
    assert_eq!(claimant.signal(), signal("PropertiesChanged", "needed"));
    assert!(claimant.flag("finger-needed") && !claimant.flag("finger-present"));
    assert!(!onlooker.flag("finger-needed") && !onlooker.flag("finger-present"));
    assert_eq!(claimant.with("VerifyStart", "any").unwrap_err(), IN_USE);
    stand.script().open = true;
    assert_eq!(
        claimant.until_done(),
        vec![signal("PropertiesChanged", "present"), signal("PropertiesChanged", "away"), signal("VerifyStatus", "verify-match true")]
    );
    assert!(onlooker.hears_nothing());
    claimant.plain("VerifyStop").unwrap();
    assert_eq!(claimant.plain("VerifyStop").unwrap_err(), NO_ACTION);
    claimant.plain("Release").unwrap();
    let uid = own_uid();
    assert_eq!(
        stand.requests(),
        vec![format!("LIST {uid} as={uid} fmt=keys"), format!("LIST {uid} as={uid} fmt=keys"), format!("VERIFY {uid} wait=60 as={uid} fmt=keys")]
    );
    assert_eq!(i32::try_from(claimant.property("num-enroll-stages")).unwrap(), STAGES as i32);
    assert_eq!(String::try_from(claimant.property("scan-type")).unwrap(), "press");
}

#[test]
fn outcomes_of_the_service_become_the_statuses_desktops_know() {
    let stand = Stand::start("any");
    stand.script().verify = vec![
        vec![say("MSG verify-prompt attempt=1"), say("MSG verify-bad-frame reason=@reject-partial"), say("MSG verify-no-match"), Step::Gate],
        vec![say("ERR locked minutes=3")],
        vec![say("ERR sensor-not-found")],
        vec![say("BUSY busy")],
        vec![say("ERR no-touch")],
        vec![say("MSG touch-finger-on-sensor"), say(&format!("OK verified finger={INDEX}"))],
        vec![say("NO not-verified")],
        vec![say("ERR contended")],
    ];
    let client = Client::join(&stand);
    client.with("Claim", "").unwrap();
    let round = |expected: &[&str]| {
        client.with("VerifyStart", INDEX).unwrap();
        let seen: Vec<String> = client.until_done().into_iter().filter(|(member, _)| member == "VerifyStatus").map(|(_, text)| text).collect();
        assert_eq!(seen, expected);
        client.plain("VerifyStop").unwrap();
    };
    round(&["verify-finger-not-centered false", "verify-no-match true"]);
    round(&["verify-unknown-error true"]);
    round(&["verify-disconnected true"]);
    round(&["verify-remove-and-retry false", "verify-match true"]);
    round(&["verify-no-match true"]);
    round(&["verify-unknown-error true"]);
    let uid = own_uid();
    let verifications = stand.requests().into_iter().filter(|request| request.starts_with("VERIFY")).collect::<Vec<_>>();
    assert_eq!(verifications.len(), 8);
    assert!(verifications.iter().all(|request| *request == format!("VERIFY {uid} {INDEX} wait=60 as={uid} fmt=keys")), "{verifications:?}");
}

#[test]
fn a_claim_is_exclusive_and_ends_with_its_owner() {
    let stand = Stand::start("any");
    stand.script().verify = vec![vec![say("MSG state-finger-needed"), Step::Gate]];
    let (first, second) = (Client::join(&stand), Client::join(&stand));
    first.with("Claim", "").unwrap();
    assert_eq!(first.with("Claim", "").unwrap_err(), IN_USE);
    assert_eq!(second.with("Claim", "").unwrap_err(), IN_USE);
    assert_eq!(second.with("VerifyStart", "any").unwrap_err(), NOT_CLAIMED);
    assert_eq!(second.plain("Release").unwrap_err(), NOT_CLAIMED);
    assert_eq!(second.with("DeleteEnrolledFinger", INDEX).unwrap_err(), NOT_CLAIMED);
    first.with("VerifyStart", "any").unwrap();
    assert_eq!(first.signal(), signal("VerifyFingerSelected", "any"));
    assert_eq!(first.signal(), signal("PropertiesChanged", "needed"));
    first.connection.close().unwrap();
    eventually("the claim of a vanished client is dropped", || second.with("Claim", "").is_ok());
    assert!(!second.flag("finger-needed"));
    assert!(second.hears_nothing());
    second.with("VerifyStart", "any").unwrap();
    assert_eq!(second.signal(), signal("VerifyFingerSelected", "any"));
}

#[test]
fn verification_needs_the_person_at_the_console() {
    let stand = Stand::start("logind");
    stand.script().verify = vec![vec![say(&format!("OK verified finger={INDEX}"))]];
    let client = Client::join(&stand);
    client.with("Claim", "").unwrap();
    let attempt = || {
        let outcome = client.with("VerifyStart", "any").map(|_| ());
        if outcome.is_ok() {
            assert_eq!(client.until_done().last(), Some(&signal("VerifyStatus", "verify-match true")));
            client.plain("VerifyStop").unwrap();
        }
        outcome
    };
    assert_eq!(attempt(), Ok(()));
    stand.script().remote = true;
    assert_eq!(attempt(), Err(DENIED.to_string()));
    stand.script().remote = false;
    stand.script().sitting = own_uid() + 1;
    assert_eq!(attempt(), Err(DENIED.to_string()));
    stand.script().class = "greeter";
    assert_eq!(attempt(), Ok(()));
    stand.script().remote = true;
    assert_eq!(attempt(), Err(DENIED.to_string()));
    assert_eq!(stand.requests().iter().filter(|request| request.starts_with("VERIFY")).count(), 2);
}

#[test]
fn enrolling_and_deleting_ask_the_right_authority() {
    let stand = Stand::start("any");
    let uid = own_uid();
    let root = uid == 0;
    let mut touches = vec![say("MSG enroll-intro finger=x"), say("MSG state-finger-needed"), say("MSG touch-rejected-retry reason=@reject-moving")];
    touches.extend((0..STAGES + 10).map(|_| say("MSG enroll-accepted points=90 novelty=new score=0 streak=0 need=10")));
    touches.push(say("OK enroll-done finger=x count=120 algorithm=A"));
    stand.script().enroll = touches;
    stand.script().fingers = vec![THUMB.to_string(), INDEX.to_string(), "spare".to_string()];
    let client = Client::join(&stand);
    assert_eq!(client.listed("").unwrap(), vec![THUMB, INDEX]);
    client.with("Claim", "").unwrap();
    if !root {
        assert_eq!(client.with("EnrollStart", INDEX).unwrap_err(), DENIED);
        assert_eq!(client.with("DeleteEnrolledFinger", INDEX).unwrap_err(), DENIED);
        assert_eq!(client.plain("DeleteEnrolledFingers2").unwrap_err(), DENIED);
        assert_eq!(client.with("DeleteEnrolledFingers", "").unwrap_err(), DENIED);
        assert_eq!(stand.script().asked, vec![VERIFY, VERIFY, ENROLL, ENROLL, ENROLL, ENROLL]);
        stand.script().granted = vec![VERIFY, ENROLL];
    }
    assert_eq!(client.with("EnrollStart", "nose").unwrap_err(), BAD_FINGER);
    client.with("EnrollStart", "right-index").unwrap();
    let statuses: Vec<String> = client.until_done().into_iter().filter(|(member, _)| member == "EnrollStatus").map(|(_, text)| text).collect();
    assert_eq!(statuses.first().map(String::as_str), Some("enroll-remove-and-retry false"));
    assert_eq!(statuses.last().map(String::as_str), Some("enroll-completed true"));
    assert_eq!(statuses.iter().filter(|status| *status == "enroll-stage-passed false").count(), STAGES - 1);
    assert_eq!(statuses.len(), STAGES + 1);
    client.plain("EnrollStop").unwrap();
    client.with("DeleteEnrolledFinger", THUMB).unwrap();
    assert_eq!(client.with("DeleteEnrolledFinger", THUMB).unwrap_err(), NO_PRINTS);
    assert_eq!(client.listed("").unwrap(), vec![INDEX]);
    client.plain("DeleteEnrolledFingers2").unwrap();
    assert_eq!(client.listed("").unwrap_err(), NO_PRINTS);
    client.plain("Release").unwrap();
    let requests = stand.requests();
    assert!(requests.contains(&format!("ENROLL {uid} {INDEX} as=0 fmt=keys")), "{requests:?}");
    for finger in [THUMB, INDEX, "spare"] {
        assert!(requests.contains(&format!("DELETE {uid} {finger} as=0 fmt=keys")), "{finger}: {requests:?}");
    }
    assert!(
        requests.iter().all(|request| !request.starts_with("LIST") || request.ends_with("as=0 fmt=keys") || request.ends_with(&format!("as={uid} fmt=keys")))
    );
}

#[test]
fn another_account_needs_an_administrator() {
    let stand = Stand::start("any");
    if own_uid() == 0 {
        return;
    }
    let client = Client::join(&stand);
    assert_eq!(client.with("Claim", "root").unwrap_err(), DENIED);
    assert_eq!(client.listed("root").unwrap_err(), DENIED);
    assert_eq!(client.with("DeleteEnrolledFingers", "root").unwrap_err(), DENIED);
    assert_eq!(client.with("Claim", "no-such-account-here").unwrap_err(), NOT_CLAIMED);
    assert_eq!(stand.script().asked, vec![OTHERS, OTHERS, OTHERS]);
    assert!(stand.requests().is_empty());
    stand.script().granted = vec![OTHERS];
    assert_eq!(client.listed("root").unwrap(), vec![INDEX]);
    client.with("Claim", "root").unwrap();
    client.with("DeleteEnrolledFinger", INDEX).unwrap();
    assert_eq!(stand.requests(), vec!["LIST 0 as=0 fmt=keys", "LIST 0 as=0 fmt=keys", "DELETE 0 right-index-finger as=0 fmt=keys"]);
}

#[test]
fn losing_the_bus_is_a_failure_the_service_manager_can_see() {
    let mut stand = Stand::start("any");
    stand.bus.kill().unwrap();
    stand.bus.wait().unwrap();
    let started = Instant::now();
    let status = loop {
        if let Some(status) = stand.bridge.try_wait().unwrap() {
            break status;
        }
        assert!(started.elapsed() < PATIENCE, "the bridge outlived its bus");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(1));
}

#[test]
fn a_claim_made_for_a_client_that_already_left_does_not_stick() {
    let stand = Stand::start("any");
    if own_uid() == 0 {
        return;
    }
    stand.script().held = true;
    let leaver = stand.connect();
    let asking = std::thread::spawn({
        let leaver = leaver.clone();
        move || leaver.call_method(Some(NAME), DEVICE, Some(INTERFACE), "Claim", &("",)).map(|_| ())
    });
    eventually("the authority is asked about the claim", || !stand.script().asked.is_empty());
    leaver.close().unwrap();
    let _ = asking.join();
    stand.script().held = false;
    let next = Client::join(&stand);
    eventually("the device is free for the next client", || next.with("Claim", "").is_ok());
}

#[test]
fn nothing_starts_for_a_client_that_released_while_it_was_being_checked() {
    let stand = Stand::start("any");
    if own_uid() == 0 {
        return;
    }
    stand.script().verify = vec![vec![say("MSG state-finger-needed"), Step::Gate]];
    let client = Client::join(&stand);
    client.with("Claim", "").unwrap();
    stand.script().asked.clear();
    stand.script().held = true;
    let starting = std::thread::spawn({
        let connection = client.connection.clone();
        move || connection.call_method(Some(NAME), DEVICE, Some(INTERFACE), "VerifyStart", &("any",)).map(|_| ()).map_err(failure)
    });
    eventually("the authority is asked about the start", || !stand.script().asked.is_empty());
    client.plain("Release").unwrap();
    stand.script().held = false;
    assert_eq!(starting.join().unwrap(), Err(NOT_CLAIMED.to_string()));
    assert!(client.hears_nothing());
    assert!(stand.requests().iter().all(|request| !request.starts_with("VERIFY")));
    let next = Client::join(&stand);
    next.with("Claim", "").unwrap();
    next.with("VerifyStart", "any").unwrap();
    assert_eq!(next.signal(), signal("VerifyFingerSelected", "any"));
}

#[test]
fn an_authority_that_fails_or_is_missing_is_a_refusal() {
    if own_uid() == 0 {
        return;
    }
    let failing = Stand::start("any");
    failing.script().broken = true;
    let missing = Stand::start_with("any", false);
    for stand in [&failing, &missing] {
        let client = Client::join(stand);
        assert_eq!(client.with("Claim", "").unwrap_err(), DENIED);
        assert_eq!(client.listed("").unwrap_err(), DENIED);
        assert!(stand.requests().is_empty());
    }
}

#[test]
fn a_service_that_cannot_be_reached_is_reported_not_passed_over_in_silence() {
    let stand = Stand::start("any");
    stand.script().granted = vec![VERIFY, ENROLL];
    let client = Client::join(&stand);
    client.with("Claim", "").unwrap();
    std::fs::remove_file(stand.dir.join("socket")).unwrap();
    client.with("EnrollStart", INDEX).unwrap();
    assert_eq!(client.until_done().last(), Some(&signal("EnrollStatus", "enroll-disconnected true")));
    client.plain("EnrollStop").unwrap();
}

#[test]
fn a_decided_verification_hangs_up_on_the_service() {
    let stand = Stand::start("any");
    stand.script().verify = vec![vec![say("MSG verify-no-match"), Step::Gate, say("MSG verify-prompt attempt=2"), say(&format!("OK verified finger={INDEX}"))]];
    let client = Client::join(&stand);
    client.with("Claim", "").unwrap();
    client.with("VerifyStart", "any").unwrap();
    assert_eq!(client.until_done().last(), Some(&signal("VerifyStatus", "verify-no-match true")));
    stand.script().open = true;
    eventually("the service notices that nobody listens any more", || stand.script().ended.iter().any(|end| end.starts_with("hangup:VERIFY")));
    assert!(client.hears_nothing());
}

fn later(connection: &Connection, method: &'static str, argument: Option<&'static str>) -> std::thread::JoinHandle<Result<(), String>> {
    let connection = connection.clone();
    std::thread::spawn(move || match argument {
        Some(argument) => connection.call_method(Some(NAME), DEVICE, Some(INTERFACE), method, &(argument,)).map(|_| ()).map_err(failure),
        None => connection.call_method(Some(NAME), DEVICE, Some(INTERFACE), method, &()).map(|_| ()).map_err(failure),
    })
}

fn asked_to_list(stand: &Stand) -> usize {
    stand.requests().iter().filter(|request| request.starts_with("LIST")).count()
}

fn asked_to_delete(stand: &Stand) -> usize {
    stand.requests().iter().filter(|request| request.starts_with("DELETE")).count()
}

fn threads_of(pid: u32) -> usize {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status.lines().find_map(|line| line.strip_prefix("Threads:")).and_then(|count| count.trim().parse().ok()).unwrap()
}

#[test]
fn nothing_is_deleted_for_a_claim_released_while_the_list_was_being_read() {
    let stand = Stand::start("any");
    if own_uid() == 0 {
        return;
    }
    stand.script().granted = vec![VERIFY, ENROLL];
    stand.script().fingers = vec![THUMB.to_string(), INDEX.to_string()];
    let client = Client::join(&stand);
    client.with("Claim", "").unwrap();
    stand.script().gated_list = true;
    let deleting = later(&client.connection, "DeleteEnrolledFingers2", None);
    eventually("the service is asked for the list", || asked_to_list(&stand) == 1);
    client.plain("Release").unwrap();
    let next = Client::join(&stand);
    next.with("Claim", "").unwrap();
    stand.script().open = true;
    assert_eq!(deleting.join().unwrap(), Err(NOT_CLAIMED.to_string()));
    assert_eq!((asked_to_delete(&stand), stand.script().fingers.len()), (0, 2));
}

#[test]
fn nothing_is_deleted_for_a_client_that_vanished_while_the_list_was_being_read() {
    let stand = Stand::start("any");
    if own_uid() == 0 {
        return;
    }
    stand.script().granted = vec![VERIFY, ENROLL];
    stand.script().fingers = vec![THUMB.to_string(), INDEX.to_string()];
    let client = Client::join(&stand);
    client.with("Claim", "").unwrap();
    stand.script().gated_list = true;
    let deleting = later(&client.connection, "DeleteEnrolledFinger", Some(THUMB));
    eventually("the service is asked for the list", || asked_to_list(&stand) == 1);
    client.connection.close().unwrap();
    let _ = deleting.join();
    let next = Client::join(&stand);
    eventually("the device is free for the next client", || next.with("Claim", "").is_ok());
    stand.script().open = true;
    eventually("the list was answered", || stand.script().ended.iter().any(|end| end.starts_with("done:LIST")));
    std::thread::sleep(QUIET);
    assert_eq!((asked_to_delete(&stand), stand.script().fingers.len()), (0, 2));
}

#[test]
fn a_claim_that_is_still_held_deletes_every_finger() {
    let stand = Stand::start("any");
    if own_uid() == 0 {
        return;
    }
    stand.script().granted = vec![VERIFY, ENROLL];
    stand.script().fingers = vec![THUMB.to_string(), INDEX.to_string()];
    let client = Client::join(&stand);
    client.with("Claim", "").unwrap();
    client.plain("DeleteEnrolledFingers2").unwrap();
    assert_eq!((asked_to_delete(&stand), stand.script().fingers.len()), (2, 0));
}

#[test]
fn the_console_is_checked_again_right_before_the_reader_is_asked() {
    let stand = Stand::start("logind");
    if own_uid() == 0 {
        return;
    }
    stand.script().verify = vec![vec![say("MSG state-finger-needed"), Step::Gate]];
    let client = Client::join(&stand);
    client.with("Claim", "").unwrap();
    stand.script().asked.clear();
    stand.script().held = true;
    let starting = later(&client.connection, "VerifyStart", Some("any"));
    eventually("the authority is asked about the start", || !stand.script().asked.is_empty());
    stand.script().sitting = own_uid() + 1;
    stand.script().held = false;
    assert_eq!(starting.join().unwrap(), Err(DENIED.to_string()));
    std::thread::sleep(QUIET);
    assert!(stand.requests().iter().all(|request| !request.starts_with("VERIFY")), "{:?}", stand.requests());
    stand.script().sitting = own_uid();
    client.with("VerifyStart", "any").unwrap();
    assert_eq!(client.signal(), signal("VerifyFingerSelected", "any"));
}

fn resident_kb(pid: u32) -> usize {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    status.lines().find_map(|line| line.strip_prefix("VmRSS:")).and_then(|size| size.trim().trim_end_matches("kB").trim().parse().ok()).unwrap()
}

fn list_call(answered: bool) -> zbus::Message {
    let call = zbus::Message::method_call(DEVICE, "ListEnrolledFingers").unwrap().destination(NAME).unwrap().interface(INTERFACE).unwrap();
    let call = if answered { call } else { call.with_flags(zbus::message::Flags::NoReplyExpected).unwrap() };
    call.build(&("",)).unwrap()
}

fn answers_at_once(stand: &Stand) -> bool {
    let probe = Builder::address(stand.address.as_str()).unwrap().method_timeout(PATIENCE).build().unwrap();
    probe.call_method(Some(NAME), MANAGER, Some(MANAGER_INTERFACE), "GetDefaultDevice", &()).is_ok()
}

#[test]
fn a_burst_of_calls_is_turned_away_without_growing_or_stalling_the_bridge() {
    const BURST: usize = 400;
    const FLOODERS: usize = 3;
    let stand = Stand::start("any");
    stand.script().verify = vec![vec![say("MSG state-finger-needed"), Step::Gate]];
    let pid = stand.bridge.id();
    let idle = threads_of(pid);
    stand.script().gated_list = true;
    let waiting = if own_uid() == 0 { FLOODERS * PER_SENDER } else { PER_USER.min(FLOODERS * PER_SENDER) };
    let flooders: Vec<Connection> = (0..FLOODERS).map(|_| stand.connect()).collect();
    let (told, refusals) = channel();
    for flooder in &flooders {
        let (answers, told) = (MessageIterator::from(flooder), told.clone());
        std::thread::spawn(move || {
            for answer in answers.flatten() {
                let header = answer.header();
                let refusal = header.message_type() == zbus::message::Type::Error && header.error_name().is_some_and(|name| name.as_str() == IN_USE);
                if refusal && told.send(()).is_err() {
                    break;
                }
            }
        });
    }
    for flooder in &flooders {
        for _ in 0..BURST {
            flooder.send(&list_call(true)).unwrap();
        }
    }
    let started = Instant::now();
    for _ in 0..FLOODERS * BURST - waiting {
        let left = PATIENCE.saturating_sub(started.elapsed());
        refusals.recv_timeout(left).expect("the bridge did not turn the burst away");
    }
    assert!(refusals.recv_timeout(QUIET).is_err(), "more calls were refused than the limits leave out");
    assert_eq!(asked_to_list(&stand), 1);
    let crowded = threads_of(pid);
    assert!(crowded <= idle + 6, "the bridge grew from {idle} to {crowded} threads");
    assert!(answers_at_once(&stand));
    let honest = Client::join(&stand);
    if own_uid() != 0 {
        assert_eq!(honest.listed("").unwrap_err(), IN_USE);
    }
    {
        let mut script = stand.script();
        script.open = true;
        script.gated_list = false;
    }
    eventually("every admitted call reached the service", || asked_to_list(&stand) == waiting);
    eventually("the bridge takes calls again", || honest.listed("").is_ok());
    honest.with("Claim", "").unwrap();
    honest.with("VerifyStart", "any").unwrap();
    assert_eq!(honest.signal(), signal("VerifyFingerSelected", "any"));
    assert!(threads_of(pid) <= idle + 6);
}

#[test]
fn a_flood_that_wants_no_answers_does_not_pile_up_inside_the_bridge() {
    const FLOOD: usize = 20_000;
    let stand = Stand::start("any");
    stand.script().gated_list = true;
    let pid = stand.bridge.id();
    assert!(answers_at_once(&stand));
    let before = resident_kb(pid);
    let flooder = stand.connect();
    for _ in 0..FLOOD {
        flooder.send(&list_call(false)).unwrap();
    }
    eventually("the bridge has worked through the flood", || answers_at_once(&stand));
    let after = resident_kb(pid);
    assert!(after < before + 24 * 1024, "the bridge grew from {before} kB to {after} kB");
    assert!(asked_to_list(&stand) <= 2 * PER_SENDER, "{:?}", asked_to_list(&stand));
    {
        let mut script = stand.script();
        script.open = true;
        script.gated_list = false;
    }
    let honest = Client::join(&stand);
    eventually("the bridge takes calls again", || honest.listed("").is_ok());
}

#[test]
fn nobody_takes_the_name_away_from_a_running_bridge() {
    let stand = Stand::start("any");
    let rival = stand.connect();
    let taken = rival.request_name_with_flags(NAME, RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue);
    assert!(matches!(taken, Err(zbus::Error::NameTaken)), "{taken:?}");
    let client = Client::join(&stand);
    client.with("Claim", "").unwrap();
    assert_eq!(client.listed("").unwrap(), [INDEX]);
}
