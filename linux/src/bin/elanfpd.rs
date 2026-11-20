use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use elanfp_engine::service::{Access, Answer, Device, ROOT, Service};
use elanfp_engine::session::Session;
use elanfp_engine::store::ALGORITHM;
use elanfp_engine::{BoxError, Catalog, Catalogs, Fail, Msg, POLICY, Presence, Reporter, as_msg, catalog, event};
use elanfp_ipc::{MAX_REQUEST, Reply, Request, Verb};
use elanfp_linux::usb::NusbTransport;
use elanfp_linux::users::Peer;
use elanfp_linux::vigil::Vigil;
use elanfp_linux::{keys, usb, users, vigil};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_CONNECTIONS: usize = 32;
const MAX_CONNECTIONS_PER_USER: usize = 4;
const RESERVED_FOR_ROOT: usize = 8;
const RESERVED_FOR_DELEGATE: usize = 4;
const REFUSAL_TIMEOUT: Duration = Duration::from_millis(200);
const ACCEPT_PAUSE: Duration = Duration::from_millis(50);
const DELEGATE_USER: &str = "elanfp-fprint";
const DELEGATE_VARIABLE: &str = "ELANFP_DELEGATE_USER";
const DIAGNOSTICS_VARIABLE: &str = "ELANFP_DIAGNOSTICS";

struct Wire<'a> {
    stream: &'a UnixStream,
    catalog: &'a Catalog,
    keys: bool,
    vigil: Option<Vigil>,
}

impl Wire<'_> {
    fn slept(&self) -> bool {
        self.vigil.as_ref().is_some_and(Vigil::slept)
    }

    fn ended_by_sleep(&self) -> bool {
        self.vigil.as_ref().is_some_and(Vigil::ended)
    }

    fn send(&self, text: String) -> Result<(), BoxError> {
        let mut stream = self.stream;
        stream.write_all(Reply::Message(text).encode().as_bytes()).map_err(|_| Fail::new("client-gone"))?;
        Ok(())
    }
}

impl Reporter for Wire<'_> {
    fn say(&mut self, message: Msg) -> Result<(), BoxError> {
        self.send(if self.keys { event(&message).encode() } else { self.catalog.render(&message) })
    }

    fn alive(&mut self) -> bool {
        users::peer_alive(self.stream) && !self.slept()
    }

    fn presence(&mut self, state: Presence) {
        if self.keys {
            let _ = self.send(state.key().to_string());
        }
    }
}

struct Usb;

impl Device for Usb {
    type Transport = NusbTransport;

    fn open(&self, report: &mut dyn Reporter) -> Result<Session<NusbTransport>, BoxError> {
        let opened = usb::open()?;
        Session::open(opened.sensor, opened.vendor_id, opened.product_id, report)
    }
}

struct Shared {
    service: Service<Usb>,
    delegate: Option<u32>,
    connections: Mutex<HashMap<u32, usize>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Slot {
    shared: Arc<Shared>,
    uid: u32,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut connections = lock(&self.shared.connections);
        if let Some(count) = connections.get_mut(&self.uid) {
            *count -= 1;
            if *count == 0 {
                connections.remove(&self.uid);
            }
        }
    }
}

impl Shared {
    fn admit(self: &Arc<Self>, uid: u32) -> Option<Slot> {
        let mut connections = lock(&self.connections);
        let reserved = |who: u32| who == ROOT || Some(who) == self.delegate;
        let allowance = match uid {
            ROOT => RESERVED_FOR_ROOT,
            uid if Some(uid) == self.delegate => RESERVED_FOR_DELEGATE,
            _ => MAX_CONNECTIONS_PER_USER,
        };
        let own = connections.get(&uid).copied().unwrap_or(0);
        let shared: usize = connections.iter().filter(|(who, _)| !reserved(**who)).map(|(_, count)| *count).sum();
        if own >= allowance || (!reserved(uid) && shared >= MAX_CONNECTIONS - RESERVED_FOR_ROOT - RESERVED_FOR_DELEGATE) {
            return None;
        }
        *connections.entry(uid).or_insert(0) += 1;
        Some(Slot { shared: Arc::clone(self), uid })
    }

    fn turn_away(&self, stream: &UnixStream, answer: Answer) {
        if stream.set_write_timeout(Some(REFUSAL_TIMEOUT)).is_ok() {
            let mut stream = stream;
            let _ = stream.write_all(answer.reply(self.service.catalog(None)).encode().as_bytes());
        }
    }

    fn request(stream: &UnixStream) -> Option<Request> {
        let deadline = Instant::now() + REQUEST_TIMEOUT;
        stream.set_write_timeout(Some(REQUEST_TIMEOUT)).ok()?;
        let mut raw = Vec::with_capacity(MAX_REQUEST);
        let mut stream = stream;
        while !raw.ends_with(b"\n") {
            let left = deadline.checked_duration_since(Instant::now()).filter(|left| !left.is_zero())?;
            stream.set_read_timeout(Some(left)).ok()?;
            let mut chunk = [0u8; MAX_REQUEST];
            let room = MAX_REQUEST + 1 - raw.len();
            match stream.read(&mut chunk[..room.min(MAX_REQUEST)]) {
                Ok(0) => return None,
                Ok(count) => raw.extend_from_slice(&chunk[..count]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
            if raw.len() > MAX_REQUEST {
                return None;
            }
        }
        Request::parse(std::str::from_utf8(&raw).ok()?)
    }

    fn serve(&self, stream: &UnixStream, peer: Peer) {
        if !users::account_exists(peer.uid) {
            return self.turn_away(stream, Answer::Err(Msg::new("peer-unknown")));
        }
        let request = Self::request(stream);
        let catalog = self.service.catalog(request.as_ref().and_then(|r| r.lang.as_deref()));
        let keys = request.as_ref().is_some_and(|r| r.keys);
        let watching = request.as_ref().is_some_and(|request| request.verb == Verb::Watch);
        let mut wire = Wire { stream, catalog, keys, vigil: (!watching).then(Vigil::begin) };
        let answer = match &request {
            None => Answer::Err(Msg::new("bad-request")),
            Some(request) if watching => self.service.observe(request, peer.uid, &mut wire),
            Some(request) => self.service.handle_from(request, peer.uid, Some(peer.pid), &mut wire),
        };
        let answer = match answer {
            Answer::Err(_) if wire.ended_by_sleep() => Answer::Err(Msg::new("sleep-interrupted")),
            other => other,
        };
        let reply = if keys { answer.reply_keys() } else { answer.reply(catalog) };
        let mut stream = stream;
        let _ = stream.write_all(reply.encode().as_bytes());
    }
}

fn listen(path: &Path, mode: u32) -> Result<UnixListener, BoxError> {
    if path.exists() {
        if UnixStream::connect(path).is_ok() {
            return Err(Fail::new("already-running").arg("socket", path.display()).into());
        }
        std::fs::remove_file(path)?;
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    Ok(listener)
}

fn run() -> Result<(), BoxError> {
    let user_mode = match std::env::args().nth(1).as_deref() {
        None => false,
        Some("--user") => true,
        Some(_) => return Err(Fail::new("daemon-usage").into()),
    };
    if !user_mode && users::effective_uid() == ROOT {
        return Err(Fail::new("daemon-root").into());
    }
    let (store, path, mode) = if user_mode {
        let path = elanfp_ipc::user_socket().ok_or_else(|| Fail::new("runtime-dir-unset"))?;
        (keys::user_store()?, path, 0o600)
    } else {
        let dir = std::env::var_os("RUNTIME_DIRECTORY").map_or_else(|| PathBuf::from("/run/elanfp"), PathBuf::from);
        (keys::system_store()?, dir.join("socket"), 0o666)
    };
    let listener = listen(&path, mode)?;
    let access = if user_mode { Access::Personal } else { Access::System };
    let delegate = if user_mode { None } else { users::uid_of(&std::env::var(DELEGATE_VARIABLE).unwrap_or_else(|_| DELEGATE_USER.to_string())) };
    let delegate = delegate.filter(|uid| *uid != ROOT);
    let detailed = match std::env::var(DIAGNOSTICS_VARIABLE).ok().filter(|value| !value.is_empty()).as_deref() {
        None | Some("off") => false,
        Some("on") => true,
        Some(value) => return Err(Fail::new("bad-setting").arg("name", DIAGNOSTICS_VARIABLE).arg("value", value).into()),
    };
    let service = Service::new(Usb, store, POLICY, access, Catalogs::new()).trusting(delegate).detailed(detailed);
    let shared = Arc::new(Shared { service, delegate, connections: Mutex::new(HashMap::new()) });
    shared
        .service
        .log(Msg::new("log-started").arg("mode", if user_mode { "user" } else { "system" }).arg("socket", path.display()).arg("algorithm", ALGORITHM));
    shared.service.log(Msg::new("log-sleep-watch").arg("by", if vigil::suspends().is_some() { "counter" } else { "clock" }));
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            std::thread::sleep(ACCEPT_PAUSE);
            continue;
        };
        let Ok(peer) = users::peer(&stream) else { continue };
        let Some(slot) = shared.admit(peer.uid) else {
            shared.turn_away(&stream, Answer::Busy(Msg::new("busy")));
            continue;
        };
        let worker = std::thread::Builder::new().name("elanfp-request".into()).spawn(move || slot.shared.serve(&stream, peer));
        if let Err(e) = worker {
            shared.service.log(Msg::new("log-request-failed").arg("detail", e));
        }
    }
    Ok(())
}

fn main() {
    if let Err(e) = users::harden_process().map_err(|e| Fail::new("process-unprotected").arg("detail", e).into()).and_then(|()| run()) {
        let journal = catalog();
        eprintln!("{}", journal.render(&Msg::new("fatal").arg("text", journal.render(&as_msg(e.as_ref(), "error-internal")))));
        std::process::exit(1);
    }
}
