use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use elanfp_core::Transport;
use elanfp_ipc::{Reply, Request, Verb};

use crate::dynamics::Dynamics;
use crate::flows::{self, Guard, Outcome};
use crate::lockout::{Charge, Lockouts, MAX_FAILURES};
use crate::session::Session;
use crate::store::FileStore;
use crate::{BoxError, Catalog, Catalogs, Explain, Fail, Msg, Policy, Presence, Reporter, as_msg};

pub const ROOT: u32 = 0;
const SECONDS_IN_MINUTE: u64 = 60;
const NOTICE_QUEUE: usize = 16;
const OBSERVERS: usize = 16;
const OBSERVERS_PER_USER: usize = 2;
const OBSERVE_TICK: Duration = Duration::from_secs(1);
const OBSERVE_BEAT: Duration = Duration::from_secs(30);
const NOBODY: u32 = u32::MAX;
const UNNAMED: &str = "?";

pub trait Device {
    type Transport: Transport;

    fn open(&self, report: &mut dyn Reporter) -> Result<Session<Self::Transport>, BoxError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    System,
    Personal,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    Ok(Msg),
    No(Msg),
    Busy(Msg),
    Err(Msg),
}

impl Answer {
    pub fn reply(&self, catalog: &Catalog) -> Reply {
        match self {
            Answer::Ok(m) => Reply::Ok(catalog.render(m)),
            Answer::No(m) => Reply::No(catalog.render(m)),
            Answer::Busy(m) => Reply::Busy(catalog.render(m)),
            Answer::Err(m) => Reply::Err(catalog.render(m)),
        }
    }

    pub fn message(&self) -> &Msg {
        match self {
            Answer::Ok(m) | Answer::No(m) | Answer::Busy(m) | Answer::Err(m) => m,
        }
    }

    pub fn reply_keys(&self) -> Reply {
        let line = crate::event(self.message()).encode();
        match self {
            Answer::Ok(_) => Reply::Ok(line),
            Answer::No(_) => Reply::No(line),
            Answer::Busy(_) => Reply::Busy(line),
            Answer::Err(_) => Reply::Err(line),
        }
    }
}

#[derive(Default)]
struct Calm {
    idle: u32,
    quiet_until: Option<Instant>,
    benched_until: Option<Instant>,
}

struct Running {
    uid: u32,
    challenged: Arc<AtomicBool>,
    crowded_by: Arc<AtomicU32>,
}

struct Origin<'a> {
    process: u32,
    program: &'a str,
    route: &'static str,
}

struct Observer {
    id: u64,
    uid: u32,
    notices: SyncSender<Msg>,
}

#[derive(Default)]
struct Observers {
    issued: u64,
    present: Vec<Observer>,
    current: Option<(u32, Msg)>,
}

pub struct Subscription<'a, D> {
    service: &'a Service<D>,
    id: u64,
    notices: Receiver<Msg>,
}

impl<D> Subscription<'_, D> {
    pub fn next(&self, patience: Duration) -> Option<Msg> {
        self.notices.recv_timeout(patience).ok()
    }
}

impl<D> Drop for Subscription<'_, D> {
    fn drop(&mut self) {
        lock(&self.service.observers).present.retain(|observer| observer.id != self.id);
    }
}

struct Announced<'a, D> {
    service: &'a Service<D>,
    uid: u32,
}

impl<D> Drop for Announced<'_, D> {
    fn drop(&mut self) {
        let mut observers = lock(&self.service.observers);
        observers.current = None;
        observers.tell(self.uid, &Msg::new("watch-over"));
    }
}

impl Observers {
    fn tell(&self, uid: u32, notice: &Msg) {
        for observer in self.present.iter().filter(|observer| observer.uid == uid) {
            let _ = observer.notices.try_send(notice.clone());
        }
    }
}

pub struct Service<D> {
    device: D,
    store: FileStore,
    policy: Policy,
    access: Access,
    sensor: Mutex<()>,
    lockouts: Lockouts,
    running: Mutex<Option<Running>>,
    calm: Mutex<HashMap<u32, Calm>>,
    observers: Mutex<Observers>,
    catalogs: Catalogs,
    delegate: Option<u32>,
    detailed: bool,
    #[cfg(test)]
    journal: Mutex<Vec<String>>,
}

pub fn needs_sensor(verb: Verb) -> bool {
    matches!(verb, Verb::Verify | Verb::Enroll)
}

impl<D> Service<D> {
    pub fn subscribe(&self, uid: u32) -> Option<Subscription<'_, D>> {
        let mut observers = lock(&self.observers);
        let theirs = observers.present.iter().filter(|observer| observer.uid == uid).count();
        if observers.present.len() >= OBSERVERS || theirs >= OBSERVERS_PER_USER {
            return None;
        }
        let (notices, received) = sync_channel(NOTICE_QUEUE);
        if let Some((_, under_way)) = observers.current.as_ref().filter(|(asked, _)| *asked == uid) {
            let _ = notices.try_send(under_way.clone());
        }
        observers.issued += 1;
        let id = observers.issued;
        observers.present.push(Observer { id, uid, notices });
        Some(Subscription { service: self, id, notices: received })
    }

    fn announce(&self, uid: u32, notice: Msg) -> Announced<'_, D> {
        let mut observers = lock(&self.observers);
        observers.tell(uid, &notice);
        observers.current = Some((uid, notice));
        Announced { service: self, uid }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Watch<'a, D> {
    service: &'a Service<D>,
    uid: u32,
    peer: u32,
    compared: bool,
}

struct Interruptible<'a> {
    inner: &'a mut dyn Reporter,
    challenged: &'a AtomicBool,
    contended: bool,
    touched: bool,
}

impl Reporter for Interruptible<'_> {
    fn say(&mut self, message: Msg) -> Result<(), BoxError> {
        self.inner.say(message)
    }

    fn alive(&mut self) -> bool {
        let live = self.inner.alive();
        if live && self.challenged.load(Ordering::Relaxed) {
            self.contended = true;
            return false;
        }
        live
    }

    fn presence(&mut self, state: Presence) {
        self.touched |= state == Presence::Present;
        self.inner.presence(state);
    }

    fn rest(&mut self, duration: Duration) {
        self.inner.rest(duration);
    }
}

impl<D: Device> Guard for Watch<'_, D>
where
    <D::Transport as Transport>::Error: Explain,
{
    fn allowed(&mut self) -> bool {
        self.service.locked_for(self.uid).is_none()
    }

    fn charge(&mut self) -> Result<bool, BoxError> {
        match self.service.lockouts.charge(&self.service.store, self.uid, Instant::now()) {
            Charge::Recorded => Ok(true),
            Charge::Locked(_) => Ok(false),
            Charge::Unrecorded(e) => {
                self.service.log(Msg::new("log-lockout-unsaved").arg("uid", self.uid).arg("detail", e));
                Err(Fail::new("attempt-unrecorded").into())
            }
        }
    }

    fn matched(&mut self, finger: &str, score: usize) {
        self.compared = true;
        if let Err(e) = self.service.lockouts.clear(&self.service.store, self.uid) {
            self.service.log(Msg::new("log-lockout-unsaved").arg("uid", self.uid).arg("detail", e));
        }
        self.service.log(Msg::new("log-verify-match").arg("uid", self.uid).arg("peer", self.peer).arg("finger", finger));
        self.service.detail(|| Msg::new("log-verify-score").arg("uid", self.uid).num("score", score));
    }

    fn mismatched(&mut self, best: usize) {
        self.compared = true;
        let failed = self.service.lockouts.failed(&self.service.store, self.uid, Instant::now());
        self.service.log(Msg::new("log-verify-no-match").arg("uid", self.uid).arg("peer", self.peer).num("count", failed.count));
        self.service.detail(|| Msg::new("log-verify-score").arg("uid", self.uid).num("score", best));
        if let Some(lasting) = failed.locked {
            self.service.log(Msg::new("log-lockout").arg("uid", self.uid).num("minutes", minutes(lasting)));
        }
    }

    fn skipped(&mut self, finger: &str, error: &BoxError) {
        let detail = self.service.catalogs.journal().render(&as_msg(error.as_ref(), "error-internal"));
        self.service.log(Msg::new("log-template-skipped").arg("uid", self.uid).arg("finger", finger).arg("detail", detail));
    }

    fn unsaved(&mut self, finger: &str, error: &BoxError) {
        let detail = self.service.catalogs.journal().render(&as_msg(error.as_ref(), "error-internal"));
        self.service.log(Msg::new("log-adapt-failed").arg("uid", self.uid).arg("finger", finger).arg("detail", detail));
    }

    fn observed(&mut self, dynamics: &Dynamics) {
        self.service.detail(|| {
            Msg::new("log-touch-dynamics")
                .arg("uid", self.uid)
                .num("frames", dynamics.frames)
                .num("settle", dynamics.settle_ms)
                .num("held", dynamics.held_ms)
                .arg("rise", format!("{:.3}", dynamics.coverage_rise))
                .arg("drift", format!("{:.3}", dynamics.contrast_drift))
                .arg("motion", format!("{:.3}", dynamics.restlessness))
        });
    }
}

fn minutes(lasting: Duration) -> u64 {
    lasting.as_secs().div_ceil(SECONDS_IN_MINUTE)
}

impl<D: Device> Service<D>
where
    <D::Transport as Transport>::Error: Explain,
{
    pub fn new(device: D, store: FileStore, policy: Policy, access: Access, catalogs: Catalogs) -> Self {
        Service {
            device,
            store,
            policy,
            access,
            sensor: Mutex::new(()),
            lockouts: Lockouts::default(),
            running: Mutex::new(None),
            calm: Mutex::new(HashMap::new()),
            observers: Mutex::default(),
            catalogs,
            delegate: None,
            detailed: false,
            #[cfg(test)]
            journal: Mutex::new(Vec::new()),
        }
    }

    pub fn trusting(mut self, delegate: Option<u32>) -> Self {
        self.delegate = delegate.filter(|&uid| uid != ROOT);
        self
    }

    pub fn detailed(mut self, detailed: bool) -> Self {
        self.detailed = detailed;
        self
    }

    fn detail(&self, line: impl FnOnce() -> Msg) {
        if self.detailed {
            self.log(line());
        }
    }

    pub fn store(&self) -> &FileStore {
        &self.store
    }

    pub fn catalog(&self, language: Option<&str>) -> &Catalog {
        self.catalogs.get(language)
    }

    pub fn log(&self, msg: Msg) {
        let _ = writeln!(std::io::stderr().lock(), "{}", self.catalogs.journal().render(&msg));
        #[cfg(test)]
        {
            crate::fake::renders_everywhere(&msg);
            lock(&self.journal).push(msg.key.to_string());
        }
    }

    fn sensor(&self) -> Option<MutexGuard<'_, ()>> {
        match self.sensor.try_lock() {
            Ok(guard) => Some(guard),
            Err(TryLockError::Poisoned(poisoned)) => Some(poisoned.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }

    fn locked_for(&self, uid: u32) -> Option<Duration> {
        self.lockouts.remaining(&self.store, uid, Instant::now())
    }

    fn paused(&self, uid: u32, supervised: bool) -> Option<Answer> {
        let now = Instant::now();
        let seconds = {
            let mut calm = lock(&self.calm);
            let entry = calm.get_mut(&uid)?;
            let ahead = |until: &mut Option<Instant>| {
                let left = until.and_then(|until| until.checked_duration_since(now)).filter(|left| !left.is_zero());
                *until = until.filter(|_| left.is_some());
                left
            };
            let benched = ahead(&mut entry.benched_until);
            let quiet = ahead(&mut entry.quiet_until).filter(|_| supervised);
            benched.max(quiet)?.as_secs() + 1
        };
        if supervised {
            self.log(Msg::new("log-refused-quiet").arg("uid", uid).num("seconds", seconds));
        }
        Some(Answer::Err(Msg::new("quiet").num("seconds", seconds)))
    }

    fn hush(&self, uid: u32, quiet: Duration) {
        let mut calm = lock(&self.calm);
        let entry = calm.entry(uid).or_default();
        let until = Instant::now() + quiet;
        entry.quiet_until = Some(entry.quiet_until.map_or(until, |current| current.max(until)));
    }

    fn bench(&self, uid: u32, held: Duration) {
        let mut calm = lock(&self.calm);
        let entry = calm.entry(uid).or_default();
        let until = Instant::now() + held;
        entry.benched_until = Some(entry.benched_until.map_or(until, |current| current.max(until)));
    }

    fn attended(&self, uid: u32) {
        if let Some(entry) = lock(&self.calm).get_mut(&uid) {
            entry.idle = 0;
        }
    }

    fn unattended(&self, uid: u32) -> u32 {
        let mut calm = lock(&self.calm);
        let entry = calm.entry(uid).or_default();
        entry.idle = entry.idle.saturating_add(1);
        if entry.idle >= self.policy.idle_requests {
            let doubling = (entry.idle - self.policy.idle_requests).min(16);
            let quiet = self.policy.idle_quiet.saturating_mul(1 << doubling).min(self.policy.idle_quiet_limit);
            let until = Instant::now() + quiet;
            entry.quiet_until = Some(entry.quiet_until.map_or(until, |current| current.max(until)));
        }
        entry.idle
    }

    fn challenge(&self, uid: u32) {
        if let Some(current) = lock(&self.running).as_ref() {
            if current.uid == uid {
                current.challenged.store(true, Ordering::Relaxed);
            } else {
                current.crowded_by.store(uid, Ordering::Relaxed);
            }
        }
    }

    fn crowd(&self, uid: u32) {
        if let Some(current) = lock(&self.running).as_ref() {
            current.crowded_by.store(uid, Ordering::Relaxed);
        }
    }

    fn beat(&self, uid: u32) -> Msg {
        let observers = lock(&self.observers);
        observers.current.as_ref().filter(|(asked, _)| *asked == uid).map_or_else(|| Msg::new("watch-idle"), |(_, under_way)| under_way.clone())
    }

    fn locked(left: Duration) -> Answer {
        Answer::Err(Msg::new("locked").num("failures", MAX_FAILURES).num("minutes", minutes(left)))
    }

    fn verify(&self, uid: u32, fingers: &[String], wait: Option<u16>, watch: &mut Watch<'_, D>, report: &mut dyn Reporter) -> Result<Outcome, BoxError> {
        let wait = wait.map_or(self.policy.verify_wait, |s| Duration::from_secs(u64::from(s)));
        let mut session = self.device.open(report)?;
        flows::verify(&self.policy, &mut session, &self.store, uid, fingers, wait, watch, report)
    }

    pub fn observe(&self, request: &Request, peer: u32, report: &mut dyn Reporter) -> Answer {
        let target = request.uid;
        if request.verb != Verb::Watch || request.acting.is_some() {
            return Answer::Err(Msg::new("bad-request"));
        }
        if !self.store.allows(target) {
            return Answer::Err(Msg::new("service-own-user-only"));
        }
        if peer != ROOT && peer != target {
            return Answer::Err(Msg::new("foreign-fingers"));
        }
        let Some(subscription) = self.subscribe(target) else { return Answer::Busy(Msg::new("watch-crowded")) };
        let mut silent = Duration::ZERO;
        loop {
            let notice = match subscription.next(OBSERVE_TICK) {
                Some(notice) => notice,
                None if !report.alive() => break,
                None => {
                    silent += OBSERVE_TICK;
                    if silent < OBSERVE_BEAT {
                        continue;
                    }
                    self.beat(target)
                }
            };
            silent = Duration::ZERO;
            if report.say(notice).is_err() {
                break;
            }
        }
        Answer::Ok(Msg::new("watch-closed"))
    }

    pub fn handle(&self, request: &Request, peer: u32, report: &mut dyn Reporter) -> Answer {
        self.handle_from(request, peer, None, report)
    }

    pub fn handle_from(&self, request: &Request, peer: u32, process: Option<u32>, report: &mut dyn Reporter) -> Answer {
        let direct = request.acting.is_none();
        let origin = Origin {
            process: process.filter(|_| direct).unwrap_or(0),
            program: request.program.as_deref().filter(|_| direct && peer == ROOT).unwrap_or(UNNAMED),
            route: if direct { "direct" } else { "bridge" },
        };
        let peer = match request.acting {
            None => peer,
            Some(acting) if Some(peer) == self.delegate => {
                if request.verb != Verb::List {
                    self.log(Msg::new("log-delegated").arg("delegate", peer).arg("peer", acting).arg("verb", request.verb.word()).arg("uid", request.uid));
                }
                acting
            }
            Some(_) => return Answer::Err(Msg::new("bad-request")),
        };
        self.dispatch(request, peer, &origin, report).unwrap_or_else(|e| match e.downcast_ref::<Fail>() {
            Some(Fail(expected)) => Answer::Err(expected.clone()),
            None => {
                self.log(Msg::new("log-request-failed").arg("detail", e));
                Answer::Err(Msg::new("service-failure"))
            }
        })
    }

    fn dispatch(&self, request: &Request, peer: u32, origin: &Origin<'_>, report: &mut dyn Reporter) -> Result<Answer, BoxError> {
        let target = request.uid;
        if !self.store.allows(target) {
            return Ok(Answer::Err(Msg::new("service-own-user-only")));
        }
        let own = peer == ROOT || peer == target;
        let privileged = peer == ROOT || (self.access == Access::Personal && peer == target);
        let finger = request.finger.as_deref();
        let busy = || Answer::Busy(Msg::new("busy"));
        match request.verb {
            Verb::List if own => {
                let names = self.store.names(target)?;
                Ok(Answer::Ok(if names.is_empty() { Msg::new("list-empty") } else { Msg::new("list").arg("fingers", names.join(", ")) }))
            }
            Verb::Delete if privileged => {
                let finger = finger.ok_or_else(|| Fail::new("bad-request"))?;
                self.store.delete(target, finger)?;
                self.log(Msg::new("log-delete").arg("uid", target).arg("finger", finger).arg("peer", peer));
                Ok(Answer::Ok(Msg::new("deleted").arg("finger", finger)))
            }
            Verb::Unlock if privileged => {
                self.lockouts.clear(&self.store, target)?;
                self.log(Msg::new("log-unlock").arg("uid", target).arg("peer", peer));
                Ok(Answer::Ok(Msg::new("unlocked")))
            }
            Verb::Enroll if privileged => {
                let finger = finger.ok_or_else(|| Fail::new("bad-request"))?;
                flows::enrollable(&self.policy, &self.store, target, finger)?;
                let Some(_sensor) = self.sensor() else {
                    self.crowd(target);
                    return Ok(busy());
                };
                let _announced = self.announce(target, Msg::new("watch-enroll").arg("route", origin.route).num("pid", origin.process));
                let mut session = self.device.open(report)?;
                let summary = flows::enroll(&self.policy, &mut session, &self.store, target, finger, report)?;
                self.log(Msg::new("log-enroll").arg("uid", target).arg("finger", finger).arg("peer", peer));
                Ok(Answer::Ok(summary))
            }
            Verb::Verify if own => {
                let supervised = peer == ROOT;
                if let Some(left) = self.locked_for(target) {
                    if supervised {
                        self.log(Msg::new("log-refused-locked").arg("uid", target).num("minutes", minutes(left)));
                    }
                    return Ok(Self::locked(left));
                }
                let fingers = match finger {
                    Some(name) if self.store.exists(target, name)? => vec![name.to_string()],
                    Some(name) => return Err(Fail::new("finger-unknown").arg("finger", name).into()),
                    None => self.store.names(target)?,
                };
                if fingers.is_empty() {
                    return Ok(Answer::Err(Msg::new("user-no-fingers")));
                }
                if let Some(refusal) = self.paused(target, supervised) {
                    return Ok(refusal);
                }
                let Some(_sensor) = self.sensor() else {
                    self.challenge(target);
                    return Ok(busy());
                };
                if let Some(refusal) = self.paused(target, supervised) {
                    return Ok(refusal);
                }
                let asked_by = if supervised { "root" } else { "self" };
                let notice = Msg::new("watch-verify").arg("by", asked_by).arg("route", origin.route).num("pid", origin.process).arg("program", origin.program);
                let _announced = self.announce(target, notice);
                let (challenged, crowded_by) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicU32::new(NOBODY)));
                *lock(&self.running) = Some(Running { uid: target, challenged: Arc::clone(&challenged), crowded_by: Arc::clone(&crowded_by) });
                let mut watch = Watch { service: self, uid: target, peer, compared: false };
                let mut interruptible = Interruptible { inner: report, challenged: &challenged, contended: false, touched: false };
                let started = Instant::now();
                let outcome = self.verify(target, &fingers, request.wait, &mut watch, &mut interruptible);
                *lock(&self.running) = None;
                let (contended, touched) = (interruptible.contended, interruptible.touched || watch.compared);
                let abandoned = contended || matches!(outcome, Ok(Outcome::NoTouch)) || !interruptible.inner.alive();
                if watch.compared {
                    if supervised {
                        self.attended(target);
                    }
                } else if supervised && abandoned && !touched {
                    let idle = self.unattended(target);
                    self.log(Msg::new("log-verify-idle").arg("uid", target).num("count", idle));
                }
                let kept_out = crowded_by.load(Ordering::Relaxed);
                if !touched && kept_out != NOBODY {
                    let held = started.elapsed();
                    self.bench(target, held);
                    self.log(Msg::new("log-benched").arg("uid", target).arg("by", kept_out).num("seconds", held.as_secs() + 1));
                }
                if contended {
                    self.hush(target, self.policy.contention_quiet);
                    self.log(Msg::new("log-contended").arg("uid", target).arg("peer", peer));
                    return Ok(Answer::Err(Msg::new("contended")));
                }
                Ok(match outcome? {
                    Outcome::Matched { finger, .. } => Answer::Ok(Msg::new("verified").arg("finger", finger)),
                    Outcome::NotMatched => Answer::No(Msg::new("not-verified")),
                    Outcome::NoTouch => Answer::Err(Msg::new("no-touch")),
                    Outcome::Locked => Self::locked(self.locked_for(target).unwrap_or(crate::lockout::LOCKOUT)),
                })
            }
            Verb::Watch => Ok(Answer::Err(Msg::new("watch-unsupported"))),
            Verb::Enroll | Verb::Delete | Verb::Unlock => Ok(Answer::Err(Msg::new("need-sudo"))),
            Verb::Verify | Verb::List => Ok(Answer::Err(Msg::new("foreign-fingers"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use elanfp_core::Sensor;

    use super::*;
    use crate::fake::{FAST, FakeSensor, PID, Recorder, TempDir, VID, policy};
    use crate::store::Layout;

    const ALICE: u32 = 1000;
    const BOB: u32 = 1001;

    struct Script(RefCell<Vec<FakeSensor>>);

    impl Device for Script {
        type Transport = FakeSensor;

        fn open(&self, report: &mut dyn Reporter) -> Result<Session<FakeSensor>, BoxError> {
            let sensor = self.0.borrow_mut().pop().unwrap_or_else(|| FakeSensor::with_touches(&[]));
            Session::open_with(Sensor::new(sensor), VID, PID, FAST, report)
        }
    }

    fn staged(dir: &TempDir, access: Access, layout: Layout, mut sensors: Vec<FakeSensor>) -> Service<Script> {
        let store = FileStore::new(dir.0.clone(), layout, papilla::vault::new_key().unwrap()).unwrap();
        sensors.reverse();
        Service::new(Script(RefCell::new(sensors)), store, policy(), access, Catalogs::new())
    }

    fn service(dir: &TempDir, access: Access, layout: Layout, sessions: &[&[u32]]) -> Service<Script> {
        staged(dir, access, layout, sessions.iter().map(|seeds| FakeSensor::with_touches(seeds)).collect())
    }

    fn logged(service: &Service<Script>, key: &str) -> usize {
        lock(&service.journal).iter().filter(|line| *line == key).count()
    }

    fn ask(service: &Service<Script>, verb: Verb, uid: u32, finger: Option<&str>, peer: u32) -> Answer {
        let request = Request::new(verb, uid).finger(finger.map(str::to_string));
        service.handle(&request, peer, &mut Recorder::default())
    }

    fn key(answer: &Answer) -> &str {
        crate::fake::renders_everywhere(answer.message());
        answer.message().key.as_ref()
    }

    #[test]
    fn only_root_enrolls_and_deletes_in_system_mode() {
        let dir = TempDir::new("svc-root");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7]]);
        assert_eq!(key(&ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ALICE)), "need-sudo");
        assert_eq!(key(&ask(&svc, Verb::Enroll, ALICE, Some("right-index"), BOB)), "need-sudo");
        assert!(svc.store().names(ALICE).unwrap().is_empty());
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        assert_eq!(key(&ask(&svc, Verb::Delete, ALICE, Some("right-index"), ALICE)), "need-sudo");
        assert_eq!(key(&ask(&svc, Verb::Delete, ALICE, Some("right-index"), BOB)), "need-sudo");
        assert_eq!(svc.store().names(ALICE).unwrap().len(), 1);
        assert!(matches!(ask(&svc, Verb::Delete, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        assert!(svc.store().names(ALICE).unwrap().is_empty());
    }

    #[test]
    fn users_reach_only_their_own_fingers() {
        let dir = TempDir::new("svc-own");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[7], &[7]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        assert_eq!(key(&ask(&svc, Verb::List, ALICE, None, BOB)), "foreign-fingers");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, BOB)), "foreign-fingers");
        assert_eq!(key(&ask(&svc, Verb::List, ALICE, None, ALICE)), "list");
        assert_eq!(key(&ask(&svc, Verb::List, BOB, None, BOB)), "list-empty");
        assert_eq!(key(&ask(&svc, Verb::Verify, BOB, None, BOB)), "user-no-fingers");
        assert!(matches!(ask(&svc, Verb::Verify, ALICE, None, ALICE), Answer::Ok(_)));
        assert!(matches!(ask(&svc, Verb::Verify, ALICE, None, ROOT), Answer::Ok(_)));
    }

    #[test]
    fn personal_service_serves_one_user_and_lets_them_manage_fingers() {
        let dir = TempDir::new("svc-personal");
        let svc = service(&dir, Access::Personal, Layout::Single(ALICE), &[&[7, 7, 7]]);
        assert_eq!(key(&ask(&svc, Verb::List, BOB, None, BOB)), "service-own-user-only");
        assert_eq!(key(&ask(&svc, Verb::List, ROOT, None, ROOT)), "service-own-user-only");
        assert_eq!(key(&ask(&svc, Verb::Enroll, ALICE, Some("right-index"), BOB)), "need-sudo");
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ALICE), Answer::Ok(_)));
        assert!(matches!(ask(&svc, Verb::Delete, ALICE, Some("right-index"), ALICE), Answer::Ok(_)));
    }

    #[test]
    fn five_failed_comparisons_lock_the_user_out_and_a_match_resets_the_count() {
        let dir = TempDir::new("svc-lock");
        let wrong: &[u32] = &[8, 9];
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], wrong, &[7], wrong, wrong, &[8, 7], &[7]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "not-verified");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "verified");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "not-verified");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "not-verified");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "locked");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "locked");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "locked");
        assert_eq!(svc.device.0.borrow().len(), 1);
        assert_eq!(key(&ask(&svc, Verb::Verify, BOB, None, BOB)), "user-no-fingers");
    }

    struct Quitter;

    impl Reporter for Quitter {
        fn say(&mut self, message: Msg) -> Result<(), BoxError> {
            if message.key == "verify-no-match" { Err(Fail::new("client-gone").into()) } else { Ok(()) }
        }
    }

    #[test]
    fn hanging_up_after_each_failure_does_not_dodge_the_lockout() {
        let dir = TempDir::new("svc-dodge");
        let mut sessions: Vec<&[u32]> = vec![&[7, 7, 7]];
        let wrong: &[u32] = &[8, 9];
        sessions.extend(std::iter::repeat_n(wrong, MAX_FAILURES as usize + 1));
        let svc = service(&dir, Access::System, Layout::PerUser, &sessions);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        for _ in 0..MAX_FAILURES {
            let answer = svc.handle(&Request::new(Verb::Verify, ALICE), ALICE, &mut Quitter);
            assert_eq!(key(&answer), "client-gone");
        }
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "locked");
        assert_eq!(svc.device.0.borrow().len(), 1);
    }

    #[test]
    fn lockout_survives_a_restart_and_a_damaged_record_fails_closed() {
        let dir = TempDir::new("svc-persist");
        let mut sessions: Vec<&[u32]> = vec![&[7, 7, 7]];
        let wrong: &[u32] = &[8, 9];
        sessions.extend(std::iter::repeat_n(wrong, 3));
        let svc = service(&dir, Access::System, Layout::PerUser, &sessions);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        for expected in ["not-verified", "not-verified", "locked"] {
            assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), expected);
        }
        assert_eq!(std::fs::read(dir.0.join("1000/lockout")).expect("lockout record"), b"5 1\n");
        assert_eq!((logged(&svc, "log-lockout"), logged(&svc, "log-verify-no-match")), (1, MAX_FAILURES as usize));
        let restart = |sessions: &[&[u32]]| {
            let store = FileStore::new(dir.0.clone(), Layout::PerUser, papilla::vault::new_key().unwrap()).unwrap();
            Service::new(
                Script(RefCell::new(sessions.iter().rev().map(|s| FakeSensor::with_touches(s)).collect())),
                store,
                policy(),
                Access::System,
                Catalogs::new(),
            )
        };
        match ask(&restart(&[&[7]]), Verb::Verify, ALICE, None, ALICE) {
            Answer::Err(msg) => assert!(svc.catalog(Some("en")).render(&msg).contains("~10 min"), "{msg:?}"),
            other => panic!("{other:?}"),
        }
        std::fs::write(dir.0.join("1000/lockout"), b"not a record").unwrap();
        assert_eq!(key(&ask(&restart(&[&[7]]), Verb::Verify, ALICE, None, ALICE)), "locked");
        std::fs::write(dir.0.join("1000/lockout"), b"0 3\n").unwrap();
        assert_ne!(key(&ask(&restart(&[&[7]]), Verb::Verify, ALICE, None, ALICE)), "locked");
    }

    #[test]
    fn an_administrator_can_lift_a_lockout_and_nobody_else() {
        let dir = TempDir::new("svc-unlock");
        let wrong: &[u32] = &[8, 9];
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], wrong, wrong, wrong, &[7]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        for expected in ["not-verified", "not-verified", "locked"] {
            assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), expected);
        }
        assert_eq!(key(&ask(&svc, Verb::Unlock, ALICE, None, ALICE)), "need-sudo");
        assert_eq!(key(&ask(&svc, Verb::Unlock, ALICE, None, BOB)), "need-sudo");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "locked");
        assert_eq!(key(&ask(&svc, Verb::Unlock, ALICE, None, ROOT)), "unlocked");
        assert!(!dir.0.join("1000/lockout").exists());
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "verified");
        assert_eq!(key(&ask(&svc, Verb::Unlock, BOB, None, ROOT)), "unlocked");
        assert_eq!(logged(&svc, "log-unlock"), 2);
    }

    #[test]
    fn only_a_failed_comparison_is_a_no() {
        let dir = TempDir::new("svc-kinds");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[8, 9], &[], &[7]]);
        assert!(matches!(ask(&svc, Verb::Verify, BOB, None, BOB), Answer::Err(_)));
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        assert!(matches!(ask(&svc, Verb::Verify, ALICE, None, ALICE), Answer::No(msg) if msg.key == "not-verified"));
        assert!(matches!(ask(&svc, Verb::Verify, ALICE, None, ALICE), Answer::Err(msg) if msg.key == "no-touch"));
        assert!(matches!(ask(&svc, Verb::Verify, ALICE, None, ALICE), Answer::Ok(msg) if msg.key == "verified"));
    }

    #[test]
    fn scores_and_touch_dynamics_reach_the_journal_only_when_asked_for() {
        let dir = TempDir::new("svc-detail");
        let quiet = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[8, 7]]);
        assert!(matches!(ask(&quiet, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        assert_eq!(key(&ask(&quiet, Verb::Verify, ALICE, None, ALICE)), "verified");
        assert_eq!((logged(&quiet, "log-verify-no-match"), logged(&quiet, "log-verify-match")), (1, 1));
        assert_eq!((logged(&quiet, "log-verify-score"), logged(&quiet, "log-touch-dynamics")), (0, 0));
        let other = TempDir::new("svc-detail-on");
        let chatty = service(&other, Access::System, Layout::PerUser, &[&[7, 7, 7], &[8, 7]]).detailed(true);
        assert!(matches!(ask(&chatty, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        assert_eq!(key(&ask(&chatty, Verb::Verify, ALICE, None, ALICE)), "verified");
        assert_eq!((logged(&chatty, "log-verify-score"), logged(&chatty, "log-touch-dynamics")), (2, 2));
    }

    #[test]
    fn an_unknown_finger_is_refused_without_touching_the_sensor() {
        let dir = TempDir::new("svc-unknown");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        let held = svc.sensor().expect("free sensor");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, Some("left-thumb"), ALICE)), "finger-unknown");
        assert_eq!(key(&ask(&svc, Verb::Verify, BOB, None, BOB)), "user-no-fingers");
        drop(held);
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, Some("left-thumb"), ALICE)), "finger-unknown");
        assert!(svc.device.0.borrow().is_empty());
    }

    #[test]
    fn untouched_privileged_requests_are_not_failures_but_soon_silence_the_reader() {
        let dir = TempDir::new("svc-notouch");
        let none: &[u32] = &[];
        let svc = patient(&dir, &[&[7, 7, 7], none, none, none, none, none, &[7]]);
        let quiet_left = || {
            let until = lock(&svc.calm).get(&ALICE).and_then(|calm| calm.quiet_until);
            until.map(|until| until.saturating_duration_since(Instant::now()).as_secs())
        };
        let pause_runs_out = || lock(&svc.calm).get_mut(&ALICE).unwrap().quiet_until = Some(Instant::now());
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        for _ in 0..policy().idle_requests {
            assert_eq!(quiet_left(), None);
            assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "no-touch");
        }
        assert_eq!(quiet_left(), Some(29));
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "quiet");
        assert_eq!(svc.device.0.borrow().len(), 3);
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "no-touch");
        pause_runs_out();
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "no-touch");
        assert_eq!(quiet_left(), Some(59));
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "quiet");
        pause_runs_out();
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "verified");
        assert_eq!(lock(&svc.calm).get(&ALICE).map(|calm| calm.idle), Some(0));
        assert_eq!((logged(&svc, "log-verify-idle"), logged(&svc, "log-refused-quiet")), (4, 2));
    }

    fn patient(dir: &TempDir, sessions: &[&[u32]]) -> Service<Script> {
        let mut slow = policy();
        slow.contention_quiet = Duration::from_secs(30);
        slow.idle_quiet = Duration::from_secs(30);
        slow.idle_quiet_limit = Duration::from_secs(600);
        let store = FileStore::new(dir.0.clone(), Layout::PerUser, papilla::vault::new_key().unwrap()).unwrap();
        let sensors = sessions.iter().rev().map(|seeds| FakeSensor::with_touches(seeds)).collect();
        Service::new(Script(RefCell::new(sensors)), store, slow, Access::System, Catalogs::new())
    }

    #[test]
    fn a_touch_in_the_users_own_request_does_not_reopen_the_reader_for_requests_from_root() {
        let dir = TempDir::new("svc-own-touch");
        let svc = patient(&dir, &[&[7, 7, 7], &[], &[9], &[7]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        let mut rival = Rival { service: &svc, target: ALICE, answer: None, stays: true };
        assert_eq!(key(&svc.handle(&Request::new(Verb::Verify, ALICE).wait(Some(30)), ROOT, &mut rival)), "contended");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "quiet");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "not-verified");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "quiet");
        assert_eq!(svc.device.0.borrow().len(), 1);
    }

    #[test]
    fn a_touch_in_the_users_own_request_does_not_forgive_untouched_requests_from_root() {
        let dir = TempDir::new("svc-own-touch-idle");
        let none: &[u32] = &[];
        let svc = patient(&dir, &[&[7, 7, 7], none, none, none, &[9], &[7]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        for _ in 0..policy().idle_requests {
            assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "no-touch");
        }
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "not-verified");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "quiet");
        assert_eq!(lock(&svc.calm).get(&ALICE).map(|calm| calm.idle), Some(policy().idle_requests));
        assert_eq!(svc.device.0.borrow().len(), 1);
    }

    #[test]
    fn an_untouched_wait_that_kept_someone_else_out_sits_out_for_as_long() {
        let dir = TempDir::new("svc-bench");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[5, 5, 5], &[], &[7], &[5]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        assert!(matches!(ask(&svc, Verb::Enroll, BOB, Some("right-index"), ROOT), Answer::Ok(_)));
        let mut kept_out = Rival { service: &svc, target: ALICE, answer: None, stays: true };
        let asked = Instant::now();
        let waited = svc.handle(&Request::new(Verb::Verify, BOB).wait(Some(1)), BOB, &mut kept_out);
        assert_eq!((key(&waited), kept_out.answer.as_ref().map(key)), ("no-touch", Some("busy")));
        let sits_out = |until: Instant| lock(&svc.calm).get_mut(&BOB).and_then(|calm| calm.benched_until.replace(until));
        let sat_out = sits_out(Instant::now() + Duration::from_secs(30)).expect("the holder sits out");
        assert!(sat_out >= asked + Duration::from_millis(1500), "{:?}", sat_out.saturating_duration_since(asked));
        assert!(sat_out.saturating_duration_since(Instant::now()) <= Duration::from_secs(3));
        assert_eq!(key(&ask(&svc, Verb::Verify, BOB, None, BOB)), "quiet");
        assert_eq!(key(&ask(&svc, Verb::Verify, BOB, None, ROOT)), "quiet");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "verified");
        sits_out(Instant::now());
        assert_eq!(key(&ask(&svc, Verb::Verify, BOB, None, BOB)), "verified");
        assert_eq!(logged(&svc, "log-benched"), 1);
    }

    #[test]
    fn a_finger_that_was_there_but_gave_no_usable_frame_is_not_absence() {
        let dir = TempDir::new("svc-wet");
        let wet = || {
            let mut sensor = FakeSensor::new();
            for seed in 0..policy().unusable_limit as u32 {
                sensor.press(crate::fake::partial_frame(seed));
            }
            sensor
        };
        let mut sensors = vec![FakeSensor::with_touches(&[7, 7, 7])];
        sensors.extend((0..policy().idle_requests + 2).map(|_| wet()));
        sensors.push(FakeSensor::with_touches(&[7]));
        let svc = staged(&dir, Access::System, Layout::PerUser, sensors);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        for _ in 0..policy().idle_requests + 2 {
            assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "no-touch");
        }
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "verified");
        assert_eq!(logged(&svc, "log-verify-idle"), 0);
    }

    #[test]
    fn a_waiting_lock_screen_is_never_silenced_for_waiting() {
        let dir = TempDir::new("svc-lockscreen");
        let none: &[u32] = &[];
        let mut sessions: Vec<&[u32]> = vec![&[7, 7, 7]];
        sessions.extend(std::iter::repeat_n(none, 8));
        sessions.push(&[7]);
        let svc = service(&dir, Access::System, Layout::PerUser, &sessions);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        for _ in 0..8 {
            assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "no-touch");
        }
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "verified");
    }

    const BRIDGE: u32 = 777;

    fn delegated(verb: Verb, uid: u32, finger: Option<&str>, acting: u32) -> Request {
        Request::new(verb, uid).finger(finger.map(str::to_string)).acting(Some(acting))
    }

    #[test]
    fn only_the_trusted_delegate_may_speak_for_someone_else() {
        let dir = TempDir::new("svc-delegate");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[7]]).trusting(Some(BRIDGE));
        let silent = &mut Recorder::default();
        for impostor in [ALICE, BOB, ROOT] {
            assert_eq!(key(&svc.handle(&delegated(Verb::List, ALICE, None, ROOT), impostor, silent)), "bad-request");
            assert_eq!(key(&svc.handle(&delegated(Verb::Enroll, ALICE, Some("right-index"), ROOT), impostor, silent)), "bad-request");
        }
        assert_eq!(key(&ask(&svc, Verb::List, ALICE, None, BRIDGE)), "foreign-fingers");
        assert_eq!(key(&ask(&svc, Verb::Enroll, ALICE, Some("right-index"), BRIDGE)), "need-sudo");
        assert_eq!(key(&svc.handle(&delegated(Verb::Enroll, ALICE, Some("right-index"), ALICE), BRIDGE, silent)), "need-sudo");
        assert!(matches!(svc.handle(&delegated(Verb::Enroll, ALICE, Some("right-index"), ROOT), BRIDGE, silent), Answer::Ok(_)));
        assert_eq!(key(&svc.handle(&delegated(Verb::List, ALICE, None, BOB), BRIDGE, silent)), "foreign-fingers");
        assert_eq!(key(&svc.handle(&delegated(Verb::Verify, ALICE, None, ALICE), BRIDGE, silent)), "verified");
        let unprivileged = service(&TempDir::new("svc-no-root-delegate"), Access::System, Layout::PerUser, &[]).trusting(Some(ROOT));
        assert!(unprivileged.delegate.is_none());
    }

    struct Rival<'a> {
        service: &'a Service<Script>,
        target: u32,
        answer: Option<Answer>,
        stays: bool,
    }

    impl Reporter for Rival<'_> {
        fn say(&mut self, _message: Msg) -> Result<(), BoxError> {
            Ok(())
        }

        fn alive(&mut self) -> bool {
            if self.answer.is_none() {
                self.answer = Some(self.service.handle(&Request::new(Verb::Verify, self.target), self.target, &mut Recorder::default()));
            }
            self.stays
        }
    }

    fn seen(subscription: &Subscription<'_, Script>) -> Vec<String> {
        std::iter::from_fn(|| subscription.next(Duration::from_millis(10))).map(|notice| notice.to_string()).collect()
    }

    #[test]
    fn the_owner_can_see_every_request_for_their_fingers_and_nobody_elses() {
        let dir = TempDir::new("svc-observe");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[7], &[7], &[5, 5, 5]]).trusting(Some(BRIDGE));
        let early = svc.subscribe(ALICE).unwrap();
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        assert_eq!(seen(&early), ["watch-enroll route=direct pid=0", "watch-over"]);
        drop(early);
        let (alice, bob) = (svc.subscribe(ALICE).unwrap(), svc.subscribe(BOB).unwrap());
        assert_eq!(key(&svc.handle_from(&Request::new(Verb::Verify, ALICE), ROOT, Some(4242), &mut Recorder::default())), "verified");
        assert_eq!(seen(&alice), ["watch-verify by=root route=direct pid=4242 program=?", "watch-over"]);
        assert_eq!(key(&svc.handle_from(&delegated(Verb::Verify, ALICE, None, ALICE), BRIDGE, Some(77), &mut Recorder::default())), "verified");
        assert_eq!(seen(&alice), ["watch-verify by=self route=bridge pid=0 program=?", "watch-over"]);
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, Some("left-thumb"), ROOT)), "finger-unknown");
        assert_eq!(key(&ask(&svc, Verb::Verify, BOB, None, ROOT)), "user-no-fingers");
        assert_eq!(key(&ask(&svc, Verb::List, ALICE, None, ALICE)), "list");
        assert!(seen(&alice).is_empty() && seen(&bob).is_empty());
        assert!(matches!(ask(&svc, Verb::Enroll, BOB, Some("right-index"), ROOT), Answer::Ok(_)));
        assert_eq!(seen(&bob), ["watch-enroll route=direct pid=0", "watch-over"]);
        assert!(seen(&alice).is_empty());
    }

    #[test]
    fn an_observer_who_arrives_late_is_told_about_the_request_under_way() {
        struct Latecomer<'a> {
            service: &'a Service<Script>,
            arrived: Option<Subscription<'a, Script>>,
        }

        impl Reporter for Latecomer<'_> {
            fn say(&mut self, _message: Msg) -> Result<(), BoxError> {
                Ok(())
            }

            fn alive(&mut self) -> bool {
                if self.arrived.is_none() {
                    self.arrived = self.service.subscribe(ALICE);
                }
                true
            }
        }

        let dir = TempDir::new("svc-latecomer");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        let mut latecomer = Latecomer { service: &svc, arrived: None };
        let asked = Request::new(Verb::Verify, ALICE).program(Some("sudo".into()));
        assert_eq!(key(&svc.handle_from(&asked, ROOT, Some(31), &mut latecomer)), "no-touch");
        assert_eq!(
            seen(latecomer.arrived.as_ref().expect("subscribed while the request waited")),
            ["watch-verify by=root route=direct pid=31 program=sudo", "watch-over"]
        );
        let afterwards = svc.subscribe(ALICE).unwrap();
        assert!(seen(&afterwards).is_empty());
    }

    #[test]
    fn the_name_of_the_asking_program_is_taken_only_from_root_itself() {
        let dir = TempDir::new("svc-program");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[7], &[7], &[7]]).trusting(Some(BRIDGE));
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        let alice = svc.subscribe(ALICE).unwrap();
        let named = |request: Request| request.program(Some("sudo".into()));
        assert_eq!(key(&svc.handle_from(&named(Request::new(Verb::Verify, ALICE)), ROOT, Some(9), &mut Recorder::default())), "verified");
        assert_eq!(key(&svc.handle_from(&named(Request::new(Verb::Verify, ALICE)), ALICE, Some(9), &mut Recorder::default())), "verified");
        assert_eq!(key(&svc.handle_from(&named(delegated(Verb::Verify, ALICE, None, ROOT)), BRIDGE, Some(9), &mut Recorder::default())), "verified");
        assert_eq!(
            seen(&alice),
            [
                "watch-verify by=root route=direct pid=9 program=sudo",
                "watch-over",
                "watch-verify by=self route=direct pid=9 program=?",
                "watch-over",
                "watch-verify by=root route=bridge pid=0 program=?",
                "watch-over"
            ]
        );
    }

    #[test]
    fn every_refusal_of_another_account_makes_an_untouched_holder_sit_out() {
        let dir = TempDir::new("svc-turns");
        let none: &[u32] = &[];
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[5, 5, 5], none, none, none, none, none, &[7], &[5]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        assert!(matches!(ask(&svc, Verb::Enroll, BOB, Some("right-index"), ROOT), Answer::Ok(_)));
        for round in 1..=5 {
            let mut waiting = Rival { service: &svc, target: BOB, answer: None, stays: true };
            let waited = svc.handle(&Request::new(Verb::Verify, ALICE), ALICE, &mut waiting);
            assert_eq!((key(&waited), waiting.answer.as_ref().map(key)), ("no-touch", Some("busy")), "round {round}");
            assert_eq!(logged(&svc, "log-benched"), round);
            let sitting_out = lock(&svc.calm).get_mut(&ALICE).and_then(|calm| calm.benched_until.take());
            assert!(sitting_out.is_some(), "round {round}");
        }
        let mut touching = Rival { service: &svc, target: BOB, answer: None, stays: true };
        assert_eq!(key(&svc.handle(&Request::new(Verb::Verify, ALICE), ALICE, &mut touching)), "verified");
        assert_eq!(touching.answer.as_ref().map(key), Some("busy"));
        assert_eq!(logged(&svc, "log-benched"), 5);
        assert_eq!(key(&ask(&svc, Verb::Verify, BOB, None, BOB)), "verified");
    }

    #[test]
    fn the_beat_repeats_the_request_under_way_and_is_idle_otherwise() {
        let dir = TempDir::new("svc-beat");
        let svc = service(&dir, Access::System, Layout::PerUser, &[]);
        assert_eq!(svc.beat(ALICE).to_string(), "watch-idle");
        let asked = svc.announce(ALICE, Msg::new("watch-verify").arg("by", "root"));
        assert_eq!((svc.beat(ALICE).to_string(), svc.beat(BOB).to_string()), ("watch-verify by=root".to_string(), "watch-idle".to_string()));
        drop(asked);
        assert_eq!(svc.beat(ALICE).to_string(), "watch-idle");
    }

    #[test]
    fn observers_are_few_and_leave_no_trace() {
        let dir = TempDir::new("svc-observers");
        let svc = service(&dir, Access::System, Layout::PerUser, &[]);
        let (first, second) = (svc.subscribe(ALICE).unwrap(), svc.subscribe(ALICE).unwrap());
        assert!(svc.subscribe(ALICE).is_none());
        assert!(svc.subscribe(BOB).is_some());
        drop(first);
        let third = svc.subscribe(ALICE).unwrap();
        let crowd: Vec<Subscription<'_, Script>> = (5000..5100).filter_map(|uid| svc.subscribe(uid)).collect();
        assert_eq!(crowd.len(), OBSERVERS - 2);
        for _ in 0..NOTICE_QUEUE * 3 {
            lock(&svc.observers).tell(ALICE, &Msg::new("watch-idle"));
        }
        assert_eq!((seen(&second).len(), seen(&third).len()), (NOTICE_QUEUE, NOTICE_QUEUE));
        drop((second, third, crowd));
        assert!(lock(&svc.observers).present.is_empty());
    }

    #[test]
    fn watching_is_for_the_owner_and_ends_with_the_client() {
        struct Gone;

        impl Reporter for Gone {
            fn say(&mut self, _message: Msg) -> Result<(), BoxError> {
                Ok(())
            }

            fn alive(&mut self) -> bool {
                false
            }
        }

        struct Onlooker<'a> {
            service: &'a Service<Script>,
            asked: bool,
            heard: Vec<String>,
        }

        impl Reporter for Onlooker<'_> {
            fn say(&mut self, message: Msg) -> Result<(), BoxError> {
                self.heard.push(message.key.to_string());
                Ok(())
            }

            fn alive(&mut self) -> bool {
                if !std::mem::replace(&mut self.asked, true) {
                    self.service.handle(&Request::new(Verb::Verify, ALICE), ROOT, &mut Recorder::default());
                }
                !self.heard.iter().any(|key| key == "watch-over")
            }
        }

        let dir = TempDir::new("svc-watch");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[7]]).trusting(Some(BRIDGE));
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        let watch = Request::new(Verb::Watch, ALICE);
        assert_eq!(key(&svc.observe(&watch, BOB, &mut Recorder::default())), "foreign-fingers");
        assert_eq!(key(&svc.observe(&watch.clone().acting(Some(ALICE)), BRIDGE, &mut Recorder::default())), "bad-request");
        assert_eq!(key(&svc.observe(&Request::new(Verb::List, ALICE), ALICE, &mut Recorder::default())), "bad-request");
        assert_eq!(key(&ask(&svc, Verb::Watch, ALICE, None, ALICE)), "watch-unsupported");
        let started = Instant::now();
        assert_eq!(key(&svc.observe(&watch, ALICE, &mut Gone)), "watch-closed");
        assert!(started.elapsed() < OBSERVE_TICK * 3, "{:?}", started.elapsed());
        let mut onlooker = Onlooker { service: &svc, asked: false, heard: Vec::new() };
        assert_eq!(key(&svc.observe(&watch, ROOT, &mut onlooker)), "watch-closed");
        assert_eq!(onlooker.heard, ["watch-verify", "watch-over"]);
        assert!(lock(&svc.observers).present.is_empty());
    }

    #[test]
    fn a_second_request_for_the_same_user_cancels_the_waiting_one() {
        let dir = TempDir::new("svc-rival");
        let svc = patient(&dir, &[&[7, 7, 7], &[], &[7]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        let mut rival = Rival { service: &svc, target: ALICE, answer: None, stays: true };
        let first = svc.handle(&Request::new(Verb::Verify, ALICE).wait(Some(30)), ROOT, &mut rival);
        assert_eq!((key(&first), rival.answer.as_ref().map(key)), ("contended", Some("busy")));
        assert!(matches!(first, Answer::Err(_)));
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "quiet");
        assert_eq!(svc.device.0.borrow().len(), 1);
        lock(&svc.calm).get_mut(&ALICE).unwrap().quiet_until = Some(Instant::now());
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "verified");
    }

    #[test]
    fn cancelling_a_privileged_request_again_and_again_does_not_reset_the_growing_pause() {
        let dir = TempDir::new("svc-self-rival");
        let none: &[u32] = &[];
        let mut sessions: Vec<&[u32]> = vec![&[7, 7, 7]];
        sessions.extend(std::iter::repeat_n(none, policy().idle_requests as usize + 1));
        let svc = service(&dir, Access::System, Layout::PerUser, &sessions);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        for round in 0..=policy().idle_requests {
            let mut rival = Rival { service: &svc, target: ALICE, answer: None, stays: true };
            assert_eq!(key(&svc.handle(&Request::new(Verb::Verify, ALICE).wait(Some(30)), ROOT, &mut rival)), "contended", "round {round}");
            let idle = lock(&svc.calm).get(&ALICE).map(|calm| calm.idle);
            assert_eq!(idle, Some(round + 1));
            std::thread::sleep(policy().contention_quiet.max(policy().idle_quiet) * 2 + Duration::from_millis(60));
        }
        assert_eq!(logged(&svc, "log-verify-idle"), policy().idle_requests as usize + 1);
        let quiet_left = lock(&svc.calm).get(&ALICE).and_then(|calm| calm.quiet_until).map(|until| until.saturating_duration_since(Instant::now()));
        assert!(quiet_left.is_some_and(|left| left.is_zero()), "{quiet_left:?}");
    }

    #[test]
    fn hanging_up_untouched_counts_like_waiting_in_vain() {
        let dir = TempDir::new("svc-hangup");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[], &[], &[]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        struct Leaver;
        impl Reporter for Leaver {
            fn say(&mut self, _message: Msg) -> Result<(), BoxError> {
                Ok(())
            }

            fn alive(&mut self) -> bool {
                false
            }
        }
        for _ in 0..policy().idle_requests {
            assert_eq!(key(&svc.handle(&Request::new(Verb::Verify, ALICE).wait(Some(30)), ROOT, &mut Leaver)), "client-gone");
        }
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ROOT)), "quiet");
        assert_eq!(svc.device.0.borrow().len(), 0);
    }

    #[test]
    fn retrying_right_after_hanging_up_is_not_a_collision() {
        let dir = TempDir::new("svc-retry");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[], &[7]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        let mut gone = Rival { service: &svc, target: ALICE, answer: None, stays: false };
        let first = svc.handle(&Request::new(Verb::Verify, ALICE).wait(Some(30)), ROOT, &mut gone);
        assert_eq!((key(&first), gone.answer.as_ref().map(key)), ("client-gone", Some("busy")));
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "verified");
    }

    #[test]
    fn another_user_waits_for_the_reader_without_cancelling_anyone() {
        let dir = TempDir::new("svc-neighbour");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7], &[11, 11, 11], &[]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        assert!(matches!(ask(&svc, Verb::Enroll, BOB, Some("right-index"), ROOT), Answer::Ok(_)));
        let mut rival = Rival { service: &svc, target: BOB, answer: None, stays: true };
        let first = svc.handle(&Request::new(Verb::Verify, ALICE), ALICE, &mut rival);
        assert_eq!((key(&first), rival.answer.as_ref().map(key)), ("no-touch", Some("busy")));
    }

    #[test]
    fn sensor_is_exclusive_while_listing_and_deleting_never_wait_for_it() {
        let dir = TempDir::new("svc-busy");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7]]);
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        let held = svc.sensor().expect("free sensor");
        assert_eq!(key(&ask(&svc, Verb::Verify, ALICE, None, ALICE)), "busy");
        assert_eq!(key(&ask(&svc, Verb::Enroll, ALICE, Some("left-index"), ROOT)), "busy");
        assert_eq!(key(&ask(&svc, Verb::List, ALICE, None, ALICE)), "list");
        assert!(matches!(ask(&svc, Verb::Delete, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        drop(held);
        assert_eq!(key(&ask(&svc, Verb::List, ALICE, None, ALICE)), "list-empty");
    }

    #[test]
    fn expected_failures_reach_the_client_and_unexpected_ones_stay_in_the_journal() {
        let dir = TempDir::new("svc-errors");
        let svc = service(&dir, Access::System, Layout::PerUser, &[&[7, 7, 7]]);
        assert_eq!(key(&ask(&svc, Verb::Delete, ALICE, Some("right-index"), ROOT)), "finger-unknown");
        assert!(matches!(ask(&svc, Verb::Enroll, ALICE, Some("right-index"), ROOT), Answer::Ok(_)));
        std::fs::write(dir.0.join("1000/right-index.papv"), b"garbage").unwrap();
        let answer = ask(&svc, Verb::Verify, ALICE, None, ALICE);
        assert_eq!(key(&answer), "template-unreadable");
        assert!(matches!(answer, Answer::Err(_)));
    }
}
