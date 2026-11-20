use std::collections::{BTreeMap, HashMap, VecDeque};
use std::convert::Infallible;
use std::fmt::Display;
use std::time::{Duration, Instant};

use elanfp_core::Transport;
use elanfp_engine::service::{Answer, Device, ROOT, Service, needs_sensor};
use elanfp_engine::{BoxError, Catalog, Explain, Fail, Msg, Presence, Reporter, event};
use elanfp_ipc::{MAX_REQUEST, Reply, Request};
use event::{EventFlags, RawEventQueue};
use redox_scheme::scheme::{Op, SchemeResponse, SchemeState, SchemeSync, register_sync_scheme};
use redox_scheme::{CallerCtx, Id, OpenResult, RequestKind, Response, SignalBehavior, Socket};
use syscall::data::{Stat, StdFsCallMeta, TimeSpec};
use syscall::error::{EACCES, EAGAIN, EBADF, EBUSY, ECANCELED, EEXIST, EIO, EMFILE, ENODEV, ENOENT, EOPNOTSUPP, EWOULDBLOCK, Error, Result};
use syscall::flag::{EventFlags as Readiness, F_GETFL, F_SETFL, MODE_CHR, StdFsCallKind};
use syscall::schemev2::NewFdFlags;

use crate::RESOURCE;

pub const MAX_CLIENTS: usize = 32;
pub const MAX_CLIENTS_PER_USER: usize = 4;
pub const RESERVED_FOR_ROOT: usize = 8;
pub const MAX_BACKLOG: usize = 64 * 1024;
pub const MAX_PARKED: usize = 128;
pub const MAX_ORPHANS: usize = 16;
pub const STALE_AFTER: Duration = Duration::from_secs(10);
const ON_SOCKET: usize = 0;
const ON_TIMER: usize = 1;
const NANOS_IN_SECOND: u128 = 1_000_000_000;

enum Stage {
    Idle,
    Pending { request: Option<Request>, peer: u32 },
    Running,
    Done,
}

struct Client {
    owner: u32,
    touched: Instant,
    typed: Vec<u8>,
    stage: Stage,
    out: VecDeque<u8>,
}

impl Client {
    fn stale(&self) -> bool {
        matches!(self.stage, Stage::Idle | Stage::Done) && self.touched.elapsed() >= STALE_AFTER
    }
}

enum Handle {
    Root,
    Client(Client),
}

enum Job {
    Attended { id: usize, request: Option<Request>, peer: u32 },
    Orphan { request: Request, peer: u32 },
}

struct Desk {
    path: String,
    next: usize,
    handles: BTreeMap<usize, Handle>,
    orphans: VecDeque<(Request, u32)>,
    readable: Vec<usize>,
}

impl Desk {
    fn new(scheme: &str) -> Self {
        Desk { path: crate::resource_path(scheme), next: 0, handles: BTreeMap::new(), orphans: VecDeque::new(), readable: Vec::new() }
    }

    fn client(&mut self, id: usize) -> Result<&mut Client> {
        match self.handles.get_mut(&id) {
            Some(Handle::Client(client)) => Ok(client),
            _ => Err(Error::new(EBADF)),
        }
    }

    fn is_open(&self, id: usize) -> bool {
        matches!(self.handles.get(&id), Some(Handle::Client(_)))
    }

    fn clients(&self) -> impl Iterator<Item = (usize, &Client)> {
        self.handles.iter().filter_map(|(id, handle)| match handle {
            Handle::Client(client) => Some((*id, client)),
            Handle::Root => None,
        })
    }

    fn crowded(&self, uid: u32) -> Option<bool> {
        let (total, own) = self.clients().fold((0, 0), |(total, own), (_, client)| (total + 1, own + usize::from(client.owner == uid)));
        let (ceiling, allowance) = if uid == ROOT { (MAX_CLIENTS, RESERVED_FOR_ROOT) } else { (MAX_CLIENTS - RESERVED_FOR_ROOT, MAX_CLIENTS_PER_USER) };
        match (own >= allowance, total >= ceiling) {
            (true, _) => Some(true),
            (false, true) => Some(false),
            (false, false) => None,
        }
    }

    fn reclaim(&mut self, uid: u32, own_only: bool) -> bool {
        let abandoned = self
            .clients()
            .filter(|(_, client)| client.stale() && if own_only { client.owner == uid } else { client.owner != ROOT })
            .min_by_key(|(_, client)| client.touched)
            .map(|(id, _)| id);
        abandoned.is_some_and(|id| self.handles.remove(&id).is_some())
    }

    fn push(&mut self, id: usize, bytes: &[u8]) -> bool {
        match self.client(id) {
            Ok(client) if client.out.len() + bytes.len() <= MAX_BACKLOG => {
                client.out.extend(bytes);
                self.readable.push(id);
                true
            }
            _ => false,
        }
    }

    fn finish(&mut self, id: usize, bytes: &[u8]) {
        if let Ok(client) = self.client(id) {
            if client.out.len() + bytes.len() > MAX_BACKLOG {
                let whole_line = client.out.iter().position(|&byte| byte == b'\n').map_or(0, |at| at + 1);
                client.out.truncate(whole_line);
            }
            client.out.extend(bytes);
            client.stage = Stage::Done;
            client.touched = Instant::now();
            self.readable.push(id);
        }
    }

    fn take_job(&mut self) -> Option<Job> {
        if let Some((request, peer)) = self.orphans.pop_front() {
            return Some(Job::Orphan { request, peer });
        }
        let (&id, client) = self.handles.iter_mut().find_map(|(id, handle)| match handle {
            Handle::Client(client) if matches!(client.stage, Stage::Pending { .. }) => Some((id, client)),
            _ => None,
        })?;
        match std::mem::replace(&mut client.stage, Stage::Running) {
            Stage::Pending { request, peer } => Some(Job::Attended { id, request, peer }),
            _ => None,
        }
    }
}

impl SchemeSync for Desk {
    fn scheme_root(&mut self) -> Result<usize> {
        let id = self.next;
        self.next += 1;
        self.handles.insert(id, Handle::Root);
        Ok(id)
    }

    fn openat(&mut self, dirfd: usize, path: &str, _flags: usize, _fcntl_flags: u32, ctx: &CallerCtx) -> Result<OpenResult> {
        if !matches!(self.handles.get(&dirfd), Some(Handle::Root)) {
            return Err(Error::new(EACCES));
        }
        if path.trim_matches('/') != RESOURCE {
            return Err(Error::new(ENOENT));
        }
        if let Some(own_only) = self.crowded(ctx.uid) {
            if !self.reclaim(ctx.uid, own_only) || self.crowded(ctx.uid).is_some() {
                return Err(Error::new(EMFILE));
            }
        }
        let id = self.next;
        self.next += 1;
        let client = Client { owner: ctx.uid, touched: Instant::now(), typed: Vec::new(), stage: Stage::Idle, out: VecDeque::new() };
        self.handles.insert(id, Handle::Client(client));
        Ok(OpenResult::ThisScheme { number: id, flags: NewFdFlags::empty() })
    }

    fn read(&mut self, id: usize, buf: &mut [u8], _offset: u64, _fcntl_flags: u32, _ctx: &CallerCtx) -> Result<usize> {
        let client = self.client(id)?;
        if client.out.is_empty() {
            return match client.stage {
                Stage::Idle | Stage::Done => Ok(0),
                Stage::Pending { .. } | Stage::Running => Err(Error::new(EWOULDBLOCK)),
            };
        }
        client.touched = Instant::now();
        let count = buf.len().min(client.out.len());
        for (slot, byte) in buf.iter_mut().zip(client.out.drain(..count)) {
            *slot = byte;
        }
        Ok(count)
    }

    fn write(&mut self, id: usize, buf: &[u8], _offset: u64, _fcntl_flags: u32, ctx: &CallerCtx) -> Result<usize> {
        let client = self.client(id)?;
        if ctx.uid != client.owner {
            return Err(Error::new(EACCES));
        }
        if !matches!(client.stage, Stage::Idle) {
            return Err(Error::new(EBUSY));
        }
        client.touched = Instant::now();
        if client.typed.len() + buf.len() > MAX_REQUEST {
            client.typed.clear();
            client.stage = Stage::Pending { request: None, peer: client.owner };
            return Ok(buf.len());
        }
        client.typed.extend_from_slice(buf);
        if client.typed.ends_with(b"\n") {
            let request = std::str::from_utf8(&client.typed).ok().and_then(Request::parse);
            client.typed.clear();
            client.stage = Stage::Pending { request, peer: client.owner };
        }
        Ok(buf.len())
    }

    fn fcntl(&mut self, id: usize, cmd: usize, _arg: usize, _ctx: &CallerCtx) -> Result<usize> {
        self.client(id)?;
        match cmd {
            F_GETFL | F_SETFL => Ok(0),
            _ => Err(Error::new(EOPNOTSUPP)),
        }
    }

    fn fevent(&mut self, id: usize, _flags: Readiness, _ctx: &CallerCtx) -> Result<Readiness> {
        let client = self.client(id)?;
        let answered = !client.out.is_empty() || matches!(client.stage, Stage::Idle | Stage::Done);
        Ok(if answered { Readiness::EVENT_READ } else { Readiness::empty() })
    }

    fn fpath(&mut self, id: usize, buf: &mut [u8], _ctx: &CallerCtx) -> Result<usize> {
        self.client(id)?;
        let count = buf.len().min(self.path.len());
        buf[..count].copy_from_slice(&self.path.as_bytes()[..count]);
        Ok(count)
    }

    fn fstat(&mut self, id: usize, stat: &mut Stat, _ctx: &CallerCtx) -> Result<()> {
        self.client(id)?;
        *stat = Stat { st_mode: MODE_CHR | 0o666, ..Default::default() };
        Ok(())
    }

    fn on_close(&mut self, id: usize) {
        let Some(Handle::Client(client)) = self.handles.remove(&id) else { return };
        if let Stage::Pending { request: Some(request), peer } = client.stage {
            if !needs_sensor(request.verb) && self.orphans.len() < MAX_ORPHANS {
                self.orphans.push_back((request, peer));
            }
        }
    }
}

struct Hub {
    socket: Socket,
    state: SchemeState,
    desk: Desk,
    parked: HashMap<Id, (CallerCtx, Op)>,
    failed: Option<Error>,
    events: RawEventQueue,
    timer: libredox::Fd,
}

impl Hub {
    fn respond(&mut self, response: Response) -> Result<()> {
        match self.socket.write_response(response, SignalBehavior::Restart)? {
            true => Ok(()),
            false => Err(Error::new(EIO)),
        }
    }

    fn settle(&mut self, caller: CallerCtx, mut op: Op) -> Result<()> {
        let response = match op.handle_sync_dont_consume(&caller, &mut self.desk, &mut self.state) {
            SchemeResponse::Regular(Err(e)) if e.errno == EWOULDBLOCK && !op.is_explicitly_nonblock() => {
                if self.parked.len() < MAX_PARKED {
                    self.parked.insert(caller.id, (caller, op));
                    return Ok(());
                }
                Response::err(EAGAIN, op)
            }
            SchemeResponse::Regular(status) => Response::new(status, op),
            SchemeResponse::RegularAndNotifyOnDetach(status) => Response::new_notify_on_detach(status, op),
            SchemeResponse::Opened(opened) => Response::open_dup_like(opened, op),
        };
        self.respond(response)
    }

    fn deliver(&mut self) -> Result<()> {
        let waiting: Vec<Id> = self.parked.keys().copied().collect();
        for id in waiting {
            if let Some((caller, op)) = self.parked.remove(&id) {
                self.settle(caller, op)?;
            }
        }
        for id in std::mem::take(&mut self.desk.readable) {
            if self.desk.is_open(id) {
                self.respond(Response::post_fevent(id, Readiness::EVENT_READ.bits()))?;
            }
        }
        Ok(())
    }

    fn pump(&mut self) -> Result<()> {
        loop {
            let request = match self.socket.next_request(SignalBehavior::Restart) {
                Ok(Some(request)) => request,
                Ok(None) => return Err(Error::new(ENODEV)),
                Err(e) if e.errno == EAGAIN || e.errno == EWOULDBLOCK => break,
                Err(e) => return Err(e),
            };
            match request.kind() {
                RequestKind::Call(call) => {
                    let caller = call.caller();
                    match call.op() {
                        Ok(op) => self.settle(caller, op)?,
                        Err(call) => self.respond(Response::err(EOPNOTSUPP, call))?,
                    }
                }
                RequestKind::Cancellation(cancelled) => {
                    if let Some((_, op)) = self.parked.remove(&cancelled.id) {
                        self.respond(Response::err(ECANCELED, op))?;
                    }
                }
                RequestKind::OnClose { id } => {
                    self.state.on_close(id);
                    self.desk.on_close(id);
                }
                RequestKind::SendFd(request) => self.respond(Response::err(EOPNOTSUPP, request))?,
                RequestKind::RecvFd(request) => self.respond(Response::err(EOPNOTSUPP, request))?,
                RequestKind::MsyncMsg | RequestKind::MunmapMsg | RequestKind::MmapMsg | RequestKind::OnDetach { .. } => {}
            }
        }
        self.deliver()
    }

    fn finish(&mut self, id: usize, answer: &Answer, catalog: &Catalog, keys: bool) {
        let reply = if keys { answer.reply_keys() } else { answer.reply(catalog) };
        self.desk.finish(id, reply.encode().as_bytes());
    }

    fn answer_while_busy<D: Device>(&mut self, service: &Service<D>) -> Result<()>
    where
        <D::Transport as Transport>::Error: Explain,
    {
        while let Some(job) = self.desk.take_job() {
            match job {
                Job::Orphan { request, peer } => drop(service.handle(&request, peer, &mut Unattended)),
                Job::Attended { id, request, peer } => {
                    let catalog = service.catalog(request.as_ref().and_then(|r| r.lang.as_deref()));
                    let answer = match &request {
                        None => Answer::Err(Msg::new("bad-request")),
                        Some(request) => service.handle(request, peer, &mut Unattended),
                    };
                    self.finish(id, &answer, catalog, request.as_ref().is_some_and(|r| r.keys));
                }
            }
        }
        self.deliver()
    }

    fn arm(&self, lasting: Duration) -> Result<()> {
        let mut at = TimeSpec::default();
        self.timer.read(&mut at).map_err(|e| Error::new(e.errno()))?;
        let nanos = u128::try_from(at.tv_nsec).unwrap_or(0) + lasting.as_nanos();
        at.tv_sec += i64::try_from(nanos / NANOS_IN_SECOND).unwrap_or(i64::MAX);
        at.tv_nsec = i32::try_from(nanos % NANOS_IN_SECOND).unwrap_or(0);
        self.timer.write(&at).map_err(|e| Error::new(e.errno()))?;
        Ok(())
    }
}

struct Unattended;

impl Reporter for Unattended {
    fn say(&mut self, _message: Msg) -> std::result::Result<(), BoxError> {
        Err(Fail::new("client-gone").into())
    }

    fn alive(&mut self) -> bool {
        false
    }

    fn rest(&mut self, _lasting: Duration) {}
}

struct Wire<'a, D: Device> {
    hub: &'a mut Hub,
    service: &'a Service<D>,
    id: usize,
    catalog: &'a Catalog,
    keys: bool,
}

impl<D: Device> Wire<'_, D>
where
    <D::Transport as Transport>::Error: Explain,
{
    fn pump(&mut self) {
        if self.hub.failed.is_none() {
            self.hub.failed = self.hub.pump().and_then(|()| self.hub.answer_while_busy(self.service)).err();
        }
    }
}

impl<D: Device> Reporter for Wire<'_, D>
where
    <D::Transport as Transport>::Error: Explain,
{
    fn say(&mut self, message: Msg) -> std::result::Result<(), BoxError> {
        let text = if self.keys { event(&message).encode() } else { self.catalog.render(&message) };
        let line = Reply::Message(text).encode();
        let queued = self.hub.desk.push(self.id, line.as_bytes());
        self.pump();
        match queued && self.hub.failed.is_none() {
            true => Ok(()),
            false => Err(Fail::new("client-gone").into()),
        }
    }

    fn alive(&mut self) -> bool {
        self.pump();
        self.hub.failed.is_none() && self.hub.desk.is_open(self.id)
    }

    fn presence(&mut self, state: Presence) {
        if self.keys {
            self.hub.desk.push(self.id, Reply::Message(state.key().to_string()).encode().as_bytes());
        }
    }

    fn rest(&mut self, lasting: Duration) {
        let until = Instant::now() + lasting;
        let mut armed = false;
        loop {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() || self.hub.failed.is_some() {
                return;
            }
            if !armed {
                if let Err(e) = self.hub.arm(left) {
                    self.hub.failed = Some(e);
                    return;
                }
                armed = true;
            }
            match self.hub.events.next_event() {
                Ok(woken) if woken.user_data == ON_TIMER => {}
                Ok(_) => self.pump(),
                Err(e) => {
                    self.hub.failed = Some(Error::new(e.errno()));
                    return;
                }
            }
        }
    }
}

fn scheme_fail<E: Display>(scheme: &str) -> impl Fn(E) -> Fail + '_ {
    move |e| Fail::new("scheme-error").arg("scheme", scheme).arg("detail", e)
}

fn bind(scheme: &str) -> Result<(Socket, Desk)> {
    let socket = Socket::nonblock()?;
    let mut desk = Desk::new(scheme);
    register_sync_scheme(&socket, scheme, &mut desk)?;
    Ok((socket, desk))
}

fn abandoned(scheme: &str) -> bool {
    matches!(std::fs::File::open(crate::resource_path(scheme)), Err(e) if e.raw_os_error() == Some(ENODEV))
}

fn release(scheme: &str) -> Result<()> {
    let mut name = format!("/scheme/{scheme}").into_bytes();
    let namespace = libredox::call::getns().map_err(|e| Error::new(e.errno()))?;
    syscall::std_fs_call(namespace, &mut name, &StdFsCallMeta::new(StdFsCallKind::Unlinkat, 0, 0)).map(|_| ())
}

pub struct Claim {
    socket: Socket,
    desk: Desk,
    events: RawEventQueue,
    timer: libredox::Fd,
}

impl Claim {
    pub fn new(scheme: &str) -> std::result::Result<Claim, BoxError> {
        let bound = match bind(scheme) {
            Err(e) if e.errno == EEXIST && abandoned(scheme) => release(scheme).and_then(|()| bind(scheme)),
            other => other,
        };
        let (socket, desk) = match bound {
            Err(e) if e.errno == EEXIST => return Err(Fail::new("already-running").arg("socket", crate::resource_path(scheme)).into()),
            other => other.map_err(scheme_fail(scheme))?,
        };
        let events = RawEventQueue::new().map_err(scheme_fail(scheme))?;
        events.subscribe(socket.inner().raw(), ON_SOCKET, EventFlags::READ).map_err(scheme_fail(scheme))?;
        let clock = format!("/scheme/time/{}", libredox::flag::CLOCK_MONOTONIC);
        let timer = libredox::Fd::open(&clock, libredox::flag::O_NONBLOCK, 0).map_err(scheme_fail(scheme))?;
        events.subscribe(timer.raw(), ON_TIMER, EventFlags::READ).map_err(scheme_fail(scheme))?;
        Ok(Claim { socket, desk, events, timer })
    }
}

pub struct Daemon<D: Device> {
    hub: Hub,
    service: Service<D>,
}

impl<D: Device> Daemon<D>
where
    <D::Transport as Transport>::Error: Explain,
{
    pub fn new(claim: Claim, service: Service<D>) -> Self {
        let Claim { socket, desk, events, timer } = claim;
        Daemon { hub: Hub { socket, state: SchemeState::new(), desk, parked: HashMap::new(), failed: None, events, timer }, service }
    }

    pub fn service(&self) -> &Service<D> {
        &self.service
    }

    fn run(&mut self, job: Job) {
        match job {
            Job::Orphan { request, peer } => drop(self.service.handle(&request, peer, &mut Unattended)),
            Job::Attended { id, request, peer } => {
                let catalog = self.service.catalog(request.as_ref().and_then(|r| r.lang.as_deref()));
                let keys = request.as_ref().is_some_and(|r| r.keys);
                let answer = match &request {
                    None => Answer::Err(Msg::new("bad-request")),
                    Some(request) => self.service.handle(request, peer, &mut Wire { hub: &mut self.hub, service: &self.service, id, catalog, keys }),
                };
                self.hub.finish(id, &answer, catalog, keys);
            }
        }
    }

    pub fn serve(&mut self) -> std::result::Result<Infallible, BoxError> {
        loop {
            self.hub.pump()?;
            while let Some(job) = self.hub.desk.take_job() {
                self.run(job);
                if let Some(e) = self.hub.failed.take() {
                    return Err(e.into());
                }
                self.hub.pump()?;
            }
            self.hub.events.next_event()?;
        }
    }
}
