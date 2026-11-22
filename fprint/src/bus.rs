use std::collections::HashMap;
use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use elanfp_engine::{BoxError, Catalog, Fail, Msg, catalog};
use elanfp_ipc::{Event, FINGERS, Request, Verb, standard_finger, uid_of};
use zbus::fdo::RequestNameFlags;
use zbus::message::Header;
use zbus::names::BusName;
use zbus::object_server::SignalEmitter;
use zbus::proxy::CacheProperties;
use zbus::zvariant::{OwnedObjectPath, Value};
use zbus::{Connection, DBusError, interface, proxy};

use crate::relay::{self, Outcome};
use crate::{ANY_FINGER, BUS_NAME, Console, DEVICE_INTERFACE, DEVICE_NAME, DEVICE_PATH, MANAGER_PATH, SCAN_TYPE, Settings};

const ACTION_VERIFY: &str = "org.elanfp.verify";
const ACTION_ENROLL: &str = "org.elanfp.enroll";
const ACTION_OTHERS: &str = "org.elanfp.others";
const ALLOW_INTERACTION: u32 = 1;
const ROOT: u32 = 0;
const VERIFY_WAIT_S: u16 = 60;
const BUSY_RETRIES: usize = 10;
const BUSY_PAUSE: Duration = Duration::from_millis(300);
const BUS_CALL_TIMEOUT: Duration = Duration::from_secs(120);
const GREETER: &str = "greeter";
const RELAY_QUEUE: usize = 16;
const IN_FLIGHT: usize = 64;
const IN_FLIGHT_PER_SENDER: usize = 4;
const IN_FLIGHT_PER_USER: usize = 8;
const IN_FLIGHT_KEPT_FOR_ROOT: usize = 8;
pub const POOL_VARIABLE: &str = "BLOCKING_MAX_THREADS";
pub const POOL_THREADS: &str = "4";

#[derive(Debug, DBusError)]
#[zbus(prefix = "net.reactivated.Fprint.Error")]
pub enum Error {
    #[zbus(error)]
    ZBus(zbus::Error),
    ClaimDevice(String),
    AlreadyInUse(String),
    Internal(String),
    PermissionDenied(String),
    NoEnrolledPrints(String),
    NoActionInProgress(String),
    InvalidFingername(String),
    PrintsNotDeleted(String),
}

fn texts() -> &'static Catalog {
    static TEXTS: OnceLock<Catalog> = OnceLock::new();
    TEXTS.get_or_init(catalog)
}

fn text(key: &str) -> String {
    texts().text(key)
}

fn audit(key: &str, caller: u32, target: u32, fingers: &[String]) {
    eprintln!("{}", texts().render(&Msg::new(key).num("caller", caller).num("target", target).arg("fingers", fingers.join(", "))));
}

fn not_claimed() -> Error {
    Error::ClaimDevice(text("fprint-not-claimed"))
}

fn in_use() -> Error {
    Error::AlreadyInUse(text("fprint-in-use"))
}

fn denied() -> Error {
    Error::PermissionDenied(text("fprint-denied"))
}

fn internal() -> Error {
    Error::Internal(text("fprint-internal"))
}

#[proxy(
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_service = "org.freedesktop.PolicyKit1",
    default_path = "/org/freedesktop/PolicyKit1/Authority"
)]
trait Authority {
    fn check_authorization(
        &self,
        subject: &(&str, HashMap<&str, Value<'_>>),
        action_id: &str,
        details: HashMap<&str, &str>,
        flags: u32,
        cancellation_id: &str,
    ) -> zbus::Result<(bool, bool, HashMap<String, String>)>;
}

#[proxy(interface = "org.freedesktop.login1.Seat", default_service = "org.freedesktop.login1", default_path = "/org/freedesktop/login1/seat/seat0")]
trait Seat {
    #[zbus(property)]
    fn active_session(&self) -> zbus::Result<(String, OwnedObjectPath)>;
}

#[proxy(interface = "org.freedesktop.login1.Session", default_service = "org.freedesktop.login1")]
trait Session {
    #[zbus(property)]
    fn remote(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn user(&self) -> zbus::Result<(u32, OwnedObjectPath)>;

    #[zbus(property)]
    fn class(&self) -> zbus::Result<String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Claim {
    owner: String,
    caller: u32,
    target: u32,
}

impl Claim {
    fn acting(&self) -> u32 {
        if self.caller == self.target { self.caller } else { ROOT }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Verify,
    Enroll,
}

struct Running {
    kind: Kind,
    generation: u64,
    stream: Option<UnixStream>,
}

#[derive(Default)]
pub struct State {
    claim: Option<Claim>,
    running: Option<Running>,
    generation: u64,
    finger_present: bool,
    finger_needed: bool,
}

pub type Shared = Arc<Mutex<State>>;

fn lock(state: &Shared) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn forget(state: &mut State, owner: &str) -> bool {
    let theirs = state.claim.as_ref().is_some_and(|claim| claim.owner == owner);
    if theirs {
        halt(state, None);
        state.claim = None;
    }
    theirs
}

fn halt(state: &mut State, kind: Option<Kind>) -> bool {
    match state.running.take() {
        Some(running) if kind.is_none_or(|wanted| wanted == running.kind) => {
            if let Some(stream) = running.stream {
                let _ = stream.shutdown(Shutdown::Both);
            }
            state.generation += 1;
            state.finger_present = false;
            state.finger_needed = false;
            true
        }
        other => {
            state.running = other;
            false
        }
    }
}

fn sender(header: &Header<'_>) -> Result<String, Error> {
    header.sender().map(|name| name.to_string()).ok_or_else(internal)
}

async fn caller_uid(connection: &Connection, sender: &str) -> Result<u32, Error> {
    let bus = zbus::fdo::DBusProxy::new(connection).await?;
    let name = BusName::try_from(sender).map_err(|_| internal())?;
    Ok(bus.get_connection_unix_user(name).await.map_err(zbus::Error::from)?)
}

async fn still_connected(connection: &Connection, sender: &str) -> bool {
    let Ok(bus) = zbus::fdo::DBusProxy::new(connection).await else { return false };
    let Ok(name) = BusName::try_from(sender) else { return false };
    bus.name_has_owner(name).await.unwrap_or(false)
}

async fn polkit(connection: &Connection, sender: &str, action: &str) -> Option<bool> {
    let authority = AuthorityProxy::new(connection).await.ok()?;
    let mut subject = HashMap::new();
    subject.insert("name", Value::from(sender));
    let answer = authority.check_authorization(&("system-bus-name", subject), action, HashMap::new(), ALLOW_INTERACTION, "").await;
    answer.ok().map(|(authorized, _, _)| authorized)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Rule {
    Allow,
    Ask { action: &'static str, unasked: bool },
}

fn rule(caller: u32, target: u32, action: &'static str) -> Rule {
    if caller == ROOT {
        Rule::Allow
    } else if caller == target {
        Rule::Ask { action, unasked: action == ACTION_VERIFY }
    } else {
        Rule::Ask { action: ACTION_OTHERS, unasked: false }
    }
}

async fn allowed(connection: &Connection, settings: &Settings, sender: &str, caller: u32, target: u32, action: &'static str) -> bool {
    match rule(caller, target, action) {
        Rule::Allow => true,
        Rule::Ask { action, .. } if settings.polkit => polkit(connection, sender, action).await == Some(true),
        Rule::Ask { unasked, .. } => unasked,
    }
}

async fn console_belongs_to(connection: &Connection, target: u32) -> zbus::Result<bool> {
    let seat = SeatProxy::builder(connection).cache_properties(CacheProperties::No).build().await?;
    let (_, path) = seat.active_session().await?;
    let session = SessionProxy::builder(connection).path(path)?.cache_properties(CacheProperties::No).build().await?;
    if session.remote().await? {
        return Ok(false);
    }
    let (owner, _) = session.user().await?;
    Ok(owner == target || session.class().await? == GREETER)
}

fn console_belongs_to_now(connection: &zbus::blocking::Connection, target: u32) -> zbus::Result<bool> {
    let seat = SeatProxyBlocking::builder(connection).cache_properties(CacheProperties::No).build()?;
    let (_, path) = seat.active_session()?;
    let session = SessionProxyBlocking::builder(connection).path(path)?.cache_properties(CacheProperties::No).build()?;
    if session.remote()? {
        return Ok(false);
    }
    let (owner, _) = session.user()?;
    Ok(owner == target || session.class()? == GREETER)
}

async fn at_the_console(connection: &Connection, settings: &Settings, target: u32) -> bool {
    match settings.console {
        Console::Any => true,
        Console::Logind => console_belongs_to(connection, target).await.unwrap_or(false),
    }
}

fn emit(connection: &zbus::blocking::Connection, owner: &str, signal: &str, status: &str, done: Option<bool>) {
    let Ok(destination) = BusName::try_from(owner) else { return };
    let _ = match done {
        Some(done) => connection.emit_signal(Some(destination), DEVICE_PATH, DEVICE_INTERFACE, signal, &(status, done)),
        None => connection.emit_signal(Some(destination), DEVICE_PATH, DEVICE_INTERFACE, signal, &(status,)),
    };
}

fn presence(connection: &zbus::blocking::Connection, state: &Shared, generation: u64, owner: &str, event: &Event) {
    let (needed, present) = match event.key.as_str() {
        "state-finger-needed" => (true, false),
        "state-finger-present" => (false, true),
        "state-finger-away" => (false, false),
        _ => return,
    };
    {
        let mut state = lock(state);
        if state.generation != generation {
            return;
        }
        state.finger_needed = needed;
        state.finger_present = present;
    }
    let Ok(destination) = BusName::try_from(owner) else { return };
    let mut changed = HashMap::new();
    changed.insert("finger-needed", Value::from(needed));
    changed.insert("finger-present", Value::from(present));
    let _ = connection.emit_signal(
        Some(destination),
        DEVICE_PATH,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
        &(DEVICE_INTERFACE, changed, Vec::<&str>::new()),
    );
}

enum Link {
    Open(UnixStream),
    Unreachable,
    Superseded,
}

struct Job {
    connection: zbus::blocking::Connection,
    settings: Arc<Settings>,
    state: Shared,
    claim: Claim,
    generation: u64,
}

impl Job {
    fn current(&self) -> bool {
        lock(&self.state).generation == self.generation
    }

    fn at_the_console(&self) -> bool {
        match self.settings.console {
            Console::Any => true,
            Console::Logind => console_belongs_to_now(&self.connection, self.claim.target).unwrap_or(false),
        }
    }

    fn attach(&self) -> Link {
        let opened = relay::open(&self.settings.socket, self.settings.service_uid).and_then(|stream| Ok((stream.try_clone()?, stream)));
        let mut state = lock(&self.state);
        match (state.running.as_mut().filter(|running| running.generation == self.generation), opened) {
            (Some(running), Ok((held, stream))) => {
                running.stream = Some(held);
                Link::Open(stream)
            }
            (Some(_), Err(_)) => Link::Unreachable,
            (None, _) => Link::Superseded,
        }
    }

    fn detach(&self) {
        let mut state = lock(&self.state);
        if let Some(running) = state.running.as_mut().filter(|running| running.generation == self.generation)
            && let Some(stream) = running.stream.take()
        {
            let _ = stream.shutdown(Shutdown::Both);
        }
    }

    fn report(&self, signal: &str, status: &str, done: bool) {
        if self.current() {
            emit(&self.connection, &self.claim.owner, signal, status, Some(done));
        }
    }

    fn verify(&self, shown: &str, stored: Option<String>) {
        if self.current() {
            emit(&self.connection, &self.claim.owner, "VerifyFingerSelected", shown, None);
        }
        let mut busy = 0;
        let mut again = false;
        let status = loop {
            if std::mem::replace(&mut again, true) && !self.at_the_console() {
                break Some("verify-unknown-error");
            }
            let stream = match self.attach() {
                Link::Open(stream) => stream,
                Link::Unreachable => break Some("verify-disconnected"),
                Link::Superseded => break None,
            };
            let request =
                Request::new(Verb::Verify, self.claim.target).finger(stored.clone()).wait(Some(VERIFY_WAIT_S)).acting(Some(self.claim.acting())).keys(true);
            let mut mismatched = false;
            let outcome = relay::converse(stream, &request, relay::VERIFY_TOTAL, |event| {
                presence(&self.connection, &self.state, self.generation, &self.claim.owner, event);
                if event.key == "verify-no-match" {
                    mismatched = true;
                    return false;
                }
                if let Some(status) = relay::verify_status(event) {
                    self.report("VerifyStatus", status, false);
                }
                true
            });
            self.detach();
            if mismatched {
                break Some("verify-no-match");
            }
            match outcome {
                Outcome::Ok(_) => break Some("verify-match"),
                Outcome::No(_) => break Some("verify-no-match"),
                Outcome::Err(event) if event.key == "no-touch" && self.current() => std::thread::sleep(BUSY_PAUSE),
                Outcome::Busy if busy < BUSY_RETRIES && self.current() => {
                    busy += 1;
                    std::thread::sleep(BUSY_PAUSE);
                }
                Outcome::Busy => break Some("verify-unknown-error"),
                Outcome::Err(event) => break Some(relay::verify_failure(&event)),
                Outcome::Lost => break self.current().then_some("verify-disconnected"),
            }
        };
        if let Some(status) = status {
            self.report("VerifyStatus", status, true);
        }
    }

    fn enroll(&self, finger: &str) {
        let stream = match self.attach() {
            Link::Open(stream) => stream,
            Link::Unreachable => return self.report("EnrollStatus", "enroll-disconnected", true),
            Link::Superseded => return,
        };
        let request = Request::new(Verb::Enroll, self.claim.target).finger(Some(finger.to_string())).acting(Some(ROOT)).keys(true);
        let mut passed = 0;
        let outcome = relay::converse(stream, &request, relay::ENROLL_TOTAL, |event| {
            presence(&self.connection, &self.state, self.generation, &self.claim.owner, event);
            match relay::enroll_status(event) {
                Some(relay::STAGE_PASSED) if passed + 1 >= self.settings.stages => {}
                Some(relay::STAGE_PASSED) => {
                    passed += 1;
                    self.report("EnrollStatus", relay::STAGE_PASSED, false);
                }
                Some(status) => self.report("EnrollStatus", status, false),
                None => {}
            }
            true
        });
        self.detach();
        let status = match outcome {
            Outcome::Ok(_) => "enroll-completed",
            Outcome::Err(event) | Outcome::No(event) => relay::enroll_failure(&event),
            Outcome::Busy => "enroll-failed",
            Outcome::Lost if self.current() => "enroll-disconnected",
            Outcome::Lost => return,
        };
        self.report("EnrollStatus", status, true);
    }
}

pub struct Manager;

#[interface(name = "net.reactivated.Fprint.Manager")]
impl Manager {
    fn get_devices(&self) -> Vec<OwnedObjectPath> {
        OwnedObjectPath::try_from(DEVICE_PATH).into_iter().collect()
    }

    fn get_default_device(&self) -> Result<OwnedObjectPath, Error> {
        OwnedObjectPath::try_from(DEVICE_PATH).map_err(|_| internal())
    }
}

#[derive(Default)]
struct Load {
    all: usize,
    others: usize,
    senders: HashMap<String, usize>,
    users: HashMap<u32, usize>,
}

fn carried(load: &Mutex<Load>) -> MutexGuard<'_, Load> {
    load.lock().unwrap_or_else(PoisonError::into_inner)
}

fn leave<K: std::hash::Hash + Eq>(counts: &mut HashMap<K, usize>, key: &K) {
    if let Some(count) = counts.get_mut(key) {
        *count -= 1;
        if *count == 0 {
            counts.remove(key);
        }
    }
}

struct Admitted {
    load: Arc<Mutex<Load>>,
    sender: String,
    user: Option<u32>,
}

impl Admitted {
    fn charge(&mut self, user: u32) -> Result<(), Error> {
        if user == ROOT || self.user.is_some() {
            return Ok(());
        }
        let mut load = carried(&self.load);
        let held = load.users.get(&user).copied().unwrap_or(0);
        if held >= IN_FLIGHT_PER_USER || load.others >= IN_FLIGHT - IN_FLIGHT_KEPT_FOR_ROOT {
            return Err(in_use());
        }
        load.users.insert(user, held + 1);
        load.others += 1;
        self.user = Some(user);
        Ok(())
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        let mut load = carried(&self.load);
        load.all -= 1;
        leave(&mut load.senders, &self.sender);
        if let Some(user) = self.user {
            leave(&mut load.users, &user);
            load.others -= 1;
        }
    }
}

fn admit(load: &Arc<Mutex<Load>>, sender: &str) -> Result<Admitted, Error> {
    let mut carried = carried(load);
    let held = carried.senders.get(sender).copied().unwrap_or(0);
    if carried.all >= IN_FLIGHT || held >= IN_FLIGHT_PER_SENDER {
        return Err(in_use());
    }
    carried.all += 1;
    carried.senders.insert(sender.to_string(), held + 1);
    Ok(Admitted { load: Arc::clone(load), sender: sender.to_string(), user: None })
}

struct Queued(Arc<AtomicUsize>);

impl Drop for Queued {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

pub struct Device {
    state: Shared,
    settings: Arc<Settings>,
    turn: Arc<async_lock::Mutex<()>>,
    queued: Arc<AtomicUsize>,
    load: Arc<Mutex<Load>>,
}

impl Device {
    fn claimed_by(&self, sender: &str) -> Result<Claim, Error> {
        lock(&self.state).claim.clone().filter(|claim| claim.owner == sender).ok_or_else(not_claimed)
    }

    fn shown_to_claimant(&self, header: Option<&Header<'_>>, pick: impl Fn(&State) -> bool) -> bool {
        let state = lock(&self.state);
        let asker = header.and_then(|header| header.sender()).map(|name| name.as_str());
        state.claim.as_ref().is_some_and(|claim| asker == Some(claim.owner.as_str())) && pick(&state)
    }

    async fn target(&self, connection: &Connection, sender: &str, username: &str) -> Result<(u32, u32), Error> {
        let caller = caller_uid(connection, sender).await?;
        let target = if username.is_empty() { caller } else { uid_of(username).ok_or_else(|| Error::ClaimDevice(text("fprint-unknown-user")))? };
        Ok((caller, target))
    }

    fn admit(&self, sender: &str) -> Result<Admitted, Error> {
        admit(&self.load, sender)
    }

    fn still_holds(&self, claim: &Claim) -> Result<(), Error> {
        if lock(&self.state).claim.as_ref() == Some(claim) { Ok(()) } else { Err(not_claimed()) }
    }

    async fn in_turn<R: Send + 'static>(&self, ask: impl FnOnce() -> R + Send + 'static) -> Result<R, Error> {
        let ahead = self.queued.fetch_add(1, Ordering::Relaxed);
        let queued = Queued(Arc::clone(&self.queued));
        if ahead >= RELAY_QUEUE {
            return Err(in_use());
        }
        let turn = self.turn.lock_arc().await;
        let asked = blocking::unblock(move || {
            let _one_at_a_time = (turn, queued);
            ask()
        });
        Ok(asked.await)
    }

    async fn stored(&self, target: u32, acting: u32) -> Result<Vec<String>, Error> {
        let (socket, service) = (self.settings.socket.clone(), self.settings.service_uid);
        self.in_turn(move || relay::stored_fingers(&socket, service, target, acting)).await?.map_err(|_| internal())
    }

    async fn remove(&self, target: u32, names: Vec<String>, holding: Option<Claim>) -> Result<(), Error> {
        let (socket, service, state) = (self.settings.socket.clone(), self.settings.service_uid, Arc::clone(&self.state));
        let removing = move || {
            names.iter().try_for_each(|name| {
                if holding.as_ref().is_some_and(|claim| lock(&state).claim.as_ref() != Some(claim)) {
                    return Err(not_claimed());
                }
                match relay::delete(&socket, service, target, name) {
                    Outcome::Ok(_) => Ok(()),
                    _ => Err(Error::PrintsNotDeleted(text("fprint-not-deleted"))),
                }
            })
        };
        self.in_turn(removing).await?
    }

    fn start(&self, connection: &Connection, claim: Claim, kind: Kind, work: impl FnOnce(Job) + Send + 'static) -> Result<(), Error> {
        let generation = {
            let mut state = lock(&self.state);
            if state.claim.as_ref() != Some(&claim) {
                return Err(not_claimed());
            }
            if state.running.is_some() {
                return Err(in_use());
            }
            state.generation += 1;
            state.running = Some(Running { kind, generation: state.generation, stream: None });
            state.generation
        };
        let job = Job {
            connection: zbus::blocking::Connection::from(connection.clone()),
            settings: Arc::clone(&self.settings),
            state: Arc::clone(&self.state),
            claim,
            generation,
        };
        std::thread::Builder::new().name("elanfp-fprint-job".into()).spawn(move || work(job)).map(|_| ()).map_err(|_| {
            halt(&mut lock(&self.state), None);
            internal()
        })
    }

    fn stop(&self, sender: &str, kind: Kind) -> Result<(), Error> {
        self.claimed_by(sender)?;
        if halt(&mut lock(&self.state), Some(kind)) { Ok(()) } else { Err(Error::NoActionInProgress(text("fprint-no-action"))) }
    }
}

#[interface(name = "net.reactivated.Fprint.Device")]
impl Device {
    async fn list_enrolled_fingers(
        &self,
        username: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> Result<Vec<String>, Error> {
        let sender = sender(&header)?;
        let mut admitted = self.admit(&sender)?;
        let (caller, target) = self.target(connection, &sender, username).await?;
        admitted.charge(caller)?;
        if !allowed(connection, &self.settings, &sender, caller, target, ACTION_VERIFY).await {
            return Err(denied());
        }
        let stored = self.stored(target, if caller == target { caller } else { ROOT }).await?;
        let fingers: Vec<String> =
            FINGERS.iter().filter(|finger| stored.iter().any(|name| standard_finger(name) == Some(**finger))).map(|finger| finger.to_string()).collect();
        if fingers.is_empty() { Err(Error::NoEnrolledPrints(text("fprint-no-prints"))) } else { Ok(fingers) }
    }

    async fn delete_enrolled_fingers(
        &self,
        username: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> Result<(), Error> {
        let sender = sender(&header)?;
        let mut admitted = self.admit(&sender)?;
        let (caller, target) = self.target(connection, &sender, username).await?;
        admitted.charge(caller)?;
        if !allowed(connection, &self.settings, &sender, caller, target, ACTION_ENROLL).await {
            return Err(denied());
        }
        let stored = self.stored(target, ROOT).await?;
        audit("fprint-audit-delete", caller, target, &stored);
        self.remove(target, stored, None).await
    }

    async fn delete_enrolled_fingers2(&self, #[zbus(header)] header: Header<'_>, #[zbus(connection)] connection: &Connection) -> Result<(), Error> {
        let sender = sender(&header)?;
        let mut admitted = self.admit(&sender)?;
        let claim = self.claimed_by(&sender)?;
        admitted.charge(claim.caller)?;
        if !allowed(connection, &self.settings, &sender, claim.caller, claim.target, ACTION_ENROLL).await {
            return Err(denied());
        }
        self.still_holds(&claim)?;
        let stored = self.stored(claim.target, ROOT).await?;
        audit("fprint-audit-delete", claim.caller, claim.target, &stored);
        self.remove(claim.target, stored, Some(claim)).await
    }

    async fn delete_enrolled_finger(
        &self,
        finger_name: &str,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> Result<(), Error> {
        let sender = sender(&header)?;
        let mut admitted = self.admit(&sender)?;
        let claim = self.claimed_by(&sender)?;
        admitted.charge(claim.caller)?;
        let finger = standard_finger(finger_name).ok_or_else(|| Error::InvalidFingername(text("fprint-bad-finger")))?;
        if !allowed(connection, &self.settings, &sender, claim.caller, claim.target, ACTION_ENROLL).await {
            return Err(denied());
        }
        self.still_holds(&claim)?;
        let stored: Vec<String> = self.stored(claim.target, ROOT).await?.into_iter().filter(|name| standard_finger(name) == Some(finger)).collect();
        if stored.is_empty() {
            return Err(Error::NoEnrolledPrints(text("fprint-no-prints")));
        }
        audit("fprint-audit-delete", claim.caller, claim.target, &stored);
        self.remove(claim.target, stored, Some(claim)).await
    }

    async fn claim(&self, username: &str, #[zbus(header)] header: Header<'_>, #[zbus(connection)] connection: &Connection) -> Result<(), Error> {
        let sender = sender(&header)?;
        let mut admitted = self.admit(&sender)?;
        let (caller, target) = self.target(connection, &sender, username).await?;
        admitted.charge(caller)?;
        if !allowed(connection, &self.settings, &sender, caller, target, ACTION_VERIFY).await {
            return Err(denied());
        }
        {
            let mut state = lock(&self.state);
            if state.claim.is_some() {
                return Err(in_use());
            }
            state.claim = Some(Claim { owner: sender.clone(), caller, target });
        }
        if still_connected(connection, &sender).await {
            return Ok(());
        }
        forget(&mut lock(&self.state), &sender);
        Err(not_claimed())
    }

    async fn release(&self, #[zbus(header)] header: Header<'_>) -> Result<(), Error> {
        if forget(&mut lock(&self.state), &sender(&header)?) { Ok(()) } else { Err(not_claimed()) }
    }

    async fn verify_start(&self, finger_name: &str, #[zbus(header)] header: Header<'_>, #[zbus(connection)] connection: &Connection) -> Result<(), Error> {
        let sender = sender(&header)?;
        let mut admitted = self.admit(&sender)?;
        let claim = self.claimed_by(&sender)?;
        admitted.charge(claim.caller)?;
        let wanted = match finger_name {
            ANY_FINGER => None,
            name => Some(standard_finger(name).ok_or_else(|| Error::InvalidFingername(text("fprint-bad-finger")))?),
        };
        if lock(&self.state).running.is_some() {
            return Err(in_use());
        }
        if !at_the_console(connection, &self.settings, claim.target).await
            || !allowed(connection, &self.settings, &sender, claim.caller, claim.target, ACTION_VERIFY).await
        {
            return Err(denied());
        }
        let stored = self.stored(claim.target, claim.acting()).await?;
        let chosen = match wanted {
            None if stored.is_empty() => return Err(Error::NoEnrolledPrints(text("fprint-no-prints"))),
            None => None,
            Some(finger) => {
                Some(stored.into_iter().find(|name| standard_finger(name) == Some(finger)).ok_or_else(|| Error::NoEnrolledPrints(text("fprint-no-prints")))?)
            }
        };
        let shown = wanted.unwrap_or(ANY_FINGER);
        if !at_the_console(connection, &self.settings, claim.target).await {
            return Err(denied());
        }
        self.start(connection, claim, Kind::Verify, move |job| job.verify(shown, chosen))
    }

    async fn verify_stop(&self, #[zbus(header)] header: Header<'_>) -> Result<(), Error> {
        self.stop(&sender(&header)?, Kind::Verify)
    }

    async fn enroll_start(&self, finger_name: &str, #[zbus(header)] header: Header<'_>, #[zbus(connection)] connection: &Connection) -> Result<(), Error> {
        let sender = sender(&header)?;
        let mut admitted = self.admit(&sender)?;
        let claim = self.claimed_by(&sender)?;
        admitted.charge(claim.caller)?;
        let finger = standard_finger(finger_name).ok_or_else(|| Error::InvalidFingername(text("fprint-bad-finger")))?;
        if lock(&self.state).running.is_some() {
            return Err(in_use());
        }
        if !allowed(connection, &self.settings, &sender, claim.caller, claim.target, ACTION_ENROLL).await {
            return Err(denied());
        }
        audit("fprint-audit-enroll", claim.caller, claim.target, &[finger.to_string()]);
        self.start(connection, claim, Kind::Enroll, move |job| job.enroll(finger))
    }

    async fn enroll_stop(&self, #[zbus(header)] header: Header<'_>) -> Result<(), Error> {
        self.stop(&sender(&header)?, Kind::Enroll)
    }

    #[zbus(signal)]
    async fn verify_finger_selected(emitter: &SignalEmitter<'_>, finger_name: &str) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn verify_status(emitter: &SignalEmitter<'_>, result: &str, done: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn enroll_status(emitter: &SignalEmitter<'_>, result: &str, done: bool) -> zbus::Result<()>;

    #[zbus(property, name = "name")]
    fn device_name(&self) -> String {
        DEVICE_NAME.to_string()
    }

    #[zbus(property, name = "num-enroll-stages")]
    fn num_enroll_stages(&self) -> i32 {
        self.settings.stages
    }

    #[zbus(property, name = "scan-type")]
    fn scan_type(&self) -> String {
        SCAN_TYPE.to_string()
    }

    #[zbus(property, name = "finger-present")]
    fn finger_present(&self, #[zbus(header)] header: Option<Header<'_>>) -> bool {
        self.shown_to_claimant(header.as_ref(), |state| state.finger_present)
    }

    #[zbus(property, name = "finger-needed")]
    fn finger_needed(&self, #[zbus(header)] header: Option<Header<'_>>) -> bool {
        self.shown_to_claimant(header.as_ref(), |state| state.finger_needed)
    }
}

pub struct Served {
    departures: zbus::blocking::fdo::NameOwnerChangedIterator,
    state: Shared,
}

pub fn serve(settings: Arc<Settings>) -> zbus::Result<Served> {
    let state: Shared = Arc::new(Mutex::new(State::default()));
    let builder = match &settings.address {
        Some(address) => zbus::blocking::connection::Builder::address(address.as_str())?,
        None => zbus::blocking::connection::Builder::system()?,
    };
    let device = Device { state: Arc::clone(&state), settings, turn: Arc::default(), queued: Arc::default(), load: Arc::default() };
    let connection = builder.method_timeout(BUS_CALL_TIMEOUT).serve_at(MANAGER_PATH, Manager)?.serve_at(DEVICE_PATH, device)?.build()?;
    let departures = zbus::blocking::fdo::DBusProxy::new(&connection)?.receive_name_owner_changed()?;
    connection.request_name_with_flags(BUS_NAME, RequestNameFlags::ReplaceExisting | RequestNameFlags::DoNotQueue)?;
    Ok(Served { departures, state })
}

pub fn watch(served: Served) -> Result<(), BoxError> {
    for change in served.departures {
        let Ok(args) = change.args() else { continue };
        if args.new_owner().is_none() {
            forget(&mut lock(&served.state), args.name().as_str());
        }
    }
    Err(Fail::new("fprint-bus-lost").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_skips_the_question_and_other_accounts_always_need_an_administrator() {
        const USER: u32 = 1000;
        const OTHER: u32 = 1001;
        for action in [ACTION_VERIFY, ACTION_ENROLL] {
            assert_eq!(rule(ROOT, USER, action), Rule::Allow);
            assert_eq!(rule(ROOT, ROOT, action), Rule::Allow);
            assert_eq!(rule(USER, OTHER, action), Rule::Ask { action: ACTION_OTHERS, unasked: false });
            assert_eq!(rule(USER, ROOT, action), Rule::Ask { action: ACTION_OTHERS, unasked: false });
        }
        assert_eq!(rule(USER, USER, ACTION_VERIFY), Rule::Ask { action: ACTION_VERIFY, unasked: true });
        assert_eq!(rule(USER, USER, ACTION_ENROLL), Rule::Ask { action: ACTION_ENROLL, unasked: false });
    }

    #[test]
    fn places_are_counted_per_connection_and_per_user_and_some_are_kept_for_root() {
        let load: Arc<Mutex<Load>> = Arc::default();
        let place = |sender: String, user: u32| {
            let mut taken = admit(&load, &sender)?;
            taken.charge(user)?;
            Ok::<Admitted, Error>(taken)
        };
        let mut held: Vec<Admitted> = (0..IN_FLIGHT_PER_SENDER).map(|_| place(":1.1".into(), 1000).unwrap()).collect();
        assert!(place(":1.1".into(), 1000).is_err());
        held.extend((0..IN_FLIGHT_PER_USER - IN_FLIGHT_PER_SENDER).map(|_| place(":1.2".into(), 1000).unwrap()));
        assert!(place(":1.3".into(), 1000).is_err());
        let others = IN_FLIGHT - IN_FLIGHT_KEPT_FOR_ROOT;
        for index in IN_FLIGHT_PER_USER..others {
            held.push(place(format!(":2.{index}"), 2000 + (index / IN_FLIGHT_PER_USER) as u32).unwrap());
        }
        assert!(place(":3.1".into(), 3000).is_err());
        let counted = || {
            let now = carried(&load);
            (now.all, now.others)
        };
        assert_eq!(counted(), (others, others));
        held.extend((0..IN_FLIGHT_KEPT_FOR_ROOT).map(|index| place(format!(":4.{index}"), ROOT).unwrap()));
        assert!(place(":4.99".into(), ROOT).is_err());
        assert_eq!(counted(), (IN_FLIGHT, others));
        drop(held);
        let emptied = carried(&load);
        assert_eq!((emptied.all, emptied.others, emptied.senders.len(), emptied.users.len()), (0, 0, 0, 0));
    }
}
