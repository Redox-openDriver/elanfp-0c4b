use std::cell::RefCell;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::sync::{RwLock, RwLockReadGuard, mpsc};
use std::time::{Duration, Instant};

use elanfp_core::Sensor;
use elanfp_engine::fake::{FAST_POLLING, FakeSensor, PID, TempDir, VID, policy};
use elanfp_engine::service::{Access, Device, Service};
use elanfp_engine::session::Session;
use elanfp_engine::store::{FileStore, Layout, new_key};
use elanfp_engine::{BoxError, Catalogs, Reporter};
use elanfp_redox::resource_path;
use elanfp_redox::scheme::{Claim, Daemon, RESERVED_FOR_ROOT, STALE_AFTER};

const ALICE: u32 = 1000;

struct Script(RefCell<Vec<Vec<u32>>>);

impl Device for Script {
    type Transport = FakeSensor;

    fn open(&self, report: &mut dyn Reporter) -> Result<Session<FakeSensor>, BoxError> {
        let seeds = self.0.borrow_mut().pop().unwrap_or_default();
        let mut sensor = FakeSensor::with_touches(&seeds);
        sensor.frames_mode = true;
        Session::open_with(Sensor::new(sensor), VID, PID, FAST_POLLING, report)
    }
}

static WHOSE: RwLock<()> = RwLock::new(());

struct Stand {
    scheme: &'static str,
    _dir: TempDir,
    _as_root: Option<RwLockReadGuard<'static, ()>>,
}

fn serve(scheme: &'static str, sessions: &[&[u32]]) -> TempDir {
    let dir = TempDir::new(scheme);
    let state = dir.0.clone();
    let script: Vec<Vec<u32>> = sessions.iter().rev().map(|s| s.to_vec()).collect();
    let (ready, started) = mpsc::channel();
    std::thread::spawn(move || {
        let store = FileStore::new(state, Layout::PerUser, new_key().unwrap()).unwrap();
        let service = Service::new(Script(RefCell::new(script)), store, policy(), Access::System, Catalogs::new());
        let mut daemon = Daemon::new(Claim::new(scheme).map_err(|e| e.to_string()).expect("scheme registration"), service);
        ready.send(()).unwrap();
        let error = daemon.serve().map_err(|e| e.to_string());
        panic!("scheme loop ended: {error:?}");
    });
    started.recv_timeout(Duration::from_secs(10)).expect("scheme did not start");
    dir
}

fn stand(scheme: &'static str, sessions: &[&[u32]]) -> Stand {
    let as_root = WHOSE.read().unwrap_or_else(|poisoned| poisoned.into_inner());
    Stand { scheme, _dir: serve(scheme, sessions), _as_root: Some(as_root) }
}

impl Stand {
    fn open(&self) -> std::io::Result<File> {
        OpenOptions::new().read(true).write(true).open(resource_path(self.scheme))
    }

    fn send(&self, line: &str) -> BufReader<File> {
        let mut file = self.open().expect("open");
        file.write_all(line.as_bytes()).expect("write");
        BufReader::new(file)
    }

    fn ask(&self, line: &str) -> (Vec<String>, String) {
        let mut lines: Vec<String> = self.send(line).lines().map(|l| l.expect("read")).collect();
        let last = lines.pop().expect("no reply");
        (lines, last)
    }

    fn ask_on(&self, mut file: File, line: &str) -> (Vec<String>, String) {
        file.write_all(line.as_bytes()).expect("write");
        let mut lines: Vec<String> = BufReader::new(file).lines().map(|l| l.expect("read")).collect();
        let last = lines.pop().expect("no reply");
        (lines, last)
    }
}

fn kind(reply: &str) -> &str {
    reply.split(' ').next().unwrap_or("")
}

#[test]
fn full_cycle_over_the_scheme() {
    let stand = stand("fingerprint.elanfp-test-cycle", &[&[7, 7, 7], &[7], &[8, 9]]);
    let (progress, done) = stand.ask("ENROLL 1000 right-index lang=en\n");
    assert_eq!(kind(&done), "OK", "{done} {progress:?}");
    assert!(progress.len() >= 3 && progress.iter().all(|line| line.starts_with("MSG ")), "{progress:?}");
    assert_eq!(stand.ask("LIST 1000 lang=en\n").1, "OK right-index");
    let (_, verified) = stand.ask("VERIFY 1000 lang=en\n");
    assert_eq!(kind(&verified), "OK", "{verified}");
    let (_, rejected) = stand.ask("VERIFY 1000 lang=en\n");
    assert_eq!(kind(&rejected), "NO", "{rejected}");
    assert_eq!(kind(&stand.ask("UNLOCK 1000 lang=en\n").1), "OK");
    assert_eq!(kind(&stand.ask("DELETE 1000 right-index lang=en\n").1), "OK");
    assert_eq!(kind(&stand.ask("VERIFY 1000 lang=en\n").1), "ERR");
}

#[test]
fn malformed_requests_get_an_error_reply() {
    let stand = stand("fingerprint.elanfp-test-garbage", &[]);
    for line in ["REBOOT 0\n", "VERIFY root\n", "VERIFY 1000 ../../etc/passwd\n", "LIST 1000\nLIST 1000\n", "\n", "ENROLL 1000\n"] {
        let (progress, last) = stand.ask(line);
        assert!(progress.is_empty() && kind(&last) == "ERR", "{line:?} → {last}");
    }
    let oversized = format!("VERIFY 1000 {}\n", "a".repeat(4096));
    assert_eq!(kind(&stand.ask(&oversized).1), "ERR");
    let mut file = stand.open().unwrap();
    file.write_all(&[0xff, 0xfe, b'\n']).unwrap();
    let mut reply = String::new();
    BufReader::new(&file).read_line(&mut reply).unwrap();
    assert_eq!(kind(&reply), "ERR");
    assert!(file.write_all(b"LIST 1000\n").is_err());
}

#[test]
fn unknown_paths_and_too_many_handles_are_refused() {
    let stand = stand("fingerprint.elanfp-test-limits", &[]);
    for path in ["", "other", "request/extra", "../request"] {
        let error = File::open(format!("/scheme/{}/{path}", stand.scheme)).err();
        assert!(error.is_some(), "{path:?} must not open");
    }
    let held: Vec<File> = (0..RESERVED_FOR_ROOT).map(|_| stand.open().expect("within the limit")).collect();
    assert!(stand.open().is_err());
    drop(held);
    assert!(stand.open().is_ok());
    let mut idle = stand.open().unwrap();
    let mut buf = [0u8; 16];
    assert_eq!(idle.read(&mut buf).unwrap(), 0);
}

const HELD: u32 = 60;
const PROMPT: Duration = Duration::from_secs(20);

fn verify_once_free(stand: &Stand) -> (String, Duration) {
    let started = Instant::now();
    loop {
        let (_, last) = stand.ask("VERIFY 1000 lang=en\n");
        if kind(&last) != "BUSY" || started.elapsed() > PROMPT {
            return (last, started.elapsed());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn sensor_is_exclusive_but_listing_and_deleting_still_answer() {
    let stand = stand("fingerprint.elanfp-test-busy", &[&[7, 7, 7], &[11, 11, 11], &[], &[7]]);
    assert_eq!(kind(&stand.ask("ENROLL 1000 right-index lang=en\n").1), "OK");
    assert_eq!(kind(&stand.ask("ENROLL 1000 left-index lang=en\n").1), "OK");
    let mut waiting = stand.send(&format!("VERIFY 1000 wait={HELD} lang=en\n"));
    let mut prompt = String::new();
    waiting.read_line(&mut prompt).unwrap();
    assert!(prompt.starts_with("MSG "), "{prompt}");
    let asked = Instant::now();
    assert_eq!(kind(&stand.ask("VERIFY 1001 lang=en\n").1), "ERR");
    assert_eq!(kind(&stand.ask("ENROLL 1000 left-thumb lang=en\n").1), "ERR");
    assert_eq!(stand.ask("LIST 1000 lang=en\n").1, "OK left-index, right-index");
    assert_eq!(kind(&stand.ask("DELETE 1000 left-index lang=en\n").1), "OK");
    assert_eq!(kind(&stand.ask("ENROLL 1000 left-thumb lang=en\n").1), "BUSY");
    assert_eq!(stand.ask("LIST 1000 lang=en\n").1, "OK right-index");
    assert!(asked.elapsed() < PROMPT, "answers must not wait for the sensor: {:?}", asked.elapsed());
    std::thread::sleep(Duration::from_secs(3));
    let held = asked.elapsed();
    drop(waiting);
    let (benched, _) = verify_once_free(&stand);
    assert!(kind(&benched) == "ERR" && benched.contains("switched off"), "{benched}");
    std::thread::sleep(held + Duration::from_secs(1));
    let (verified, _) = verify_once_free(&stand);
    assert_eq!(kind(&verified), "OK", "{verified}");
}

#[test]
fn a_second_request_for_the_same_user_cancels_the_waiting_one() {
    let stand = stand("fingerprint.elanfp-test-rival", &[&[7, 7, 7], &[]]);
    assert_eq!(kind(&stand.ask("ENROLL 1000 right-index lang=en\n").1), "OK");
    let mut waiting = stand.send(&format!("VERIFY 1000 wait={HELD} lang=en\n"));
    let mut prompt = String::new();
    waiting.read_line(&mut prompt).unwrap();
    let started = Instant::now();
    let (_, second) = stand.ask("VERIFY 1000 lang=en\n");
    let first = waiting.lines().map(|l| l.unwrap()).last().unwrap();
    assert!(started.elapsed() < PROMPT, "{:?}", started.elapsed());
    assert_eq!((kind(&first), kind(&second)), ("ERR", "BUSY"), "{first} / {second}");
    assert!(first.contains("another program"), "{first}");
    let (_, quiet) = stand.ask("VERIFY 1000 lang=en\n");
    assert!(kind(&quiet) == "ERR" && quiet.contains("switched off"), "{quiet}");
}

#[test]
fn closing_the_handle_releases_the_sensor() {
    let stand = stand("fingerprint.elanfp-test-gone", &[&[7, 7, 7], &[], &[7]]);
    assert_eq!(kind(&stand.ask("ENROLL 1000 right-index lang=en\n").1), "OK");
    let mut waiting = stand.send(&format!("VERIFY 1000 wait={HELD} lang=en\n"));
    let mut prompt = String::new();
    waiting.read_line(&mut prompt).unwrap();
    drop(waiting);
    let (verified, took) = verify_once_free(&stand);
    assert_eq!(kind(&verified), "OK", "{verified}");
    assert!(took < PROMPT, "{took:?}");
}

#[test]
fn a_request_nobody_answers_ends_by_itself() {
    let stand = stand("fingerprint.elanfp-test-timeout", &[&[7, 7, 7], &[]]);
    assert_eq!(kind(&stand.ask("ENROLL 1000 right-index lang=en\n").1), "OK");
    let (progress, last) = stand.ask("VERIFY 1000 wait=1 lang=en\n");
    assert_eq!(kind(&last), "ERR", "{last} {progress:?}");
    assert!(progress.iter().any(|line| line.starts_with("MSG ")), "{progress:?}");
}

#[test]
fn replies_follow_the_requested_language() {
    let stand = stand("fingerprint.elanfp-test-lang", &[]);
    let english = stand.ask("LIST 1000 lang=en\n").1;
    let russian = stand.ask("LIST 1000 lang=ru\n").1;
    assert!(kind(&english) == "OK" && kind(&russian) == "OK");
    assert_ne!(english, russian);
    let _ = ALICE;
    let unreadable = File::open(resource_path("fingerprint.elanfp-test-absent")).err().map(|e| e.kind());
    assert!(matches!(unreadable, Some(ErrorKind::NotFound) | Some(_)));
}

#[test]
fn a_request_may_arrive_in_pieces() {
    let stand = stand("fingerprint.elanfp-test-pieces", &[]);
    let mut file = stand.open().unwrap();
    file.write_all(b"LIST 10").unwrap();
    let mut early = [0u8; 16];
    assert_eq!(file.read(&mut early).unwrap(), 0);
    file.write_all(b"00 lang").unwrap();
    file.write_all(b"=en\n").unwrap();
    let mut reply = String::new();
    BufReader::new(&file).read_line(&mut reply).unwrap();
    assert_eq!(kind(&reply), "OK", "{reply}");
    let mut flood = stand.open().unwrap();
    let accepted = (0..40).take_while(|_| flood.write_all(b"0123456789").is_ok()).count();
    assert!(accepted > 20 && accepted < 40, "{accepted}");
    let mut refused = String::new();
    BufReader::new(&flood).read_line(&mut refused).unwrap();
    assert_eq!(kind(&refused), "ERR", "{refused}");
}

#[test]
fn a_request_sent_and_not_waited_for_is_still_carried_out() {
    let stand = stand("fingerprint.elanfp-test-orphan", &[&[7, 7, 7], &[11, 11, 11]]);
    assert_eq!(kind(&stand.ask("ENROLL 1000 right-index lang=en\n").1), "OK");
    let mut file = stand.open().unwrap();
    file.write_all(b"DELETE 1000 right-index\n").unwrap();
    drop(file);
    let started = Instant::now();
    while stand.ask("LIST 1000 lang=en\n").1.contains("right-index") {
        assert!(started.elapsed() < PROMPT, "the abandoned request was dropped");
        std::thread::sleep(Duration::from_millis(20));
    }
    let mut hasty = stand.open().unwrap();
    hasty.write_all(b"VERIFY 1000 wait=60\n").unwrap();
    drop(hasty);
    assert_eq!(kind(&stand.ask("ENROLL 1000 left-index lang=en\n").1), "OK");
}

#[test]
fn a_reply_that_is_not_ready_yet_wakes_a_waiting_poll() {
    use std::os::fd::AsRawFd;
    let stand = stand("fingerprint.elanfp-test-poll", &[&[7, 7, 7], &[]]);
    assert_eq!(kind(&stand.ask("ENROLL 1000 right-index lang=en\n").1), "OK");
    let mut file = stand.open().unwrap();
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0 && unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } >= 0);
    file.write_all(b"VERIFY 1000 wait=1 lang=en\n").unwrap();
    let (mut text, mut longest) = (String::new(), Duration::ZERO);
    while !text.ends_with('\n') || text.lines().all(|line| line.starts_with("MSG ")) {
        let mut waiting = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let started = Instant::now();
        assert!(unsafe { libc::poll(&mut waiting, 1, 10_000) } > 0, "nothing woke the poll: {text:?}");
        longest = longest.max(started.elapsed());
        let mut chunk = [0u8; 256];
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => text.push_str(std::str::from_utf8(&chunk[..count]).unwrap()),
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => panic!("{e}"),
        }
    }
    assert_eq!(kind(text.lines().last().unwrap_or("")), "ERR", "{text}");
    assert!(longest >= Duration::from_millis(300) && longest < Duration::from_secs(5), "{longest:?}");
}

#[test]
fn handles_nobody_uses_give_way_when_slots_run_out() {
    let stand = stand("fingerprint.elanfp-test-stale", &[]);
    let mut held: Vec<File> = (0..RESERVED_FOR_ROOT).map(|_| stand.open().expect("within the limit")).collect();
    assert!(stand.open().is_err());
    std::thread::sleep(STALE_AFTER + Duration::from_millis(300));
    let fresh = stand.open().expect("an unused handle gives way");
    assert!(held[0].write_all(b"LIST 1000\n").is_err());
    assert_eq!(kind(&stand.ask_on(fresh, "LIST 1000 lang=en\n").1), "OK");
    assert!(held[1].write_all(b"LIST 1000 lang=en\n").is_ok());
}

#[test]
fn a_request_belongs_to_whoever_opened_the_handle() {
    const NAME: &str = "fingerprint.elanfp-test-opener";
    let alone = WHOSE.write().unwrap_or_else(|poisoned| poisoned.into_inner());
    let stand = Stand { scheme: NAME, _dir: serve(NAME, &[]), _as_root: None };
    assert_eq!(unsafe { libc::seteuid(ALICE) }, 0);
    let opened = stand.open();
    assert_eq!(unsafe { libc::seteuid(0) }, 0);
    drop(alone);
    let mut file = opened.expect("open as a user");
    let refused = file.write_all(b"ENROLL 1000 flipped\n").expect_err("a handle opened by a user must not carry root's request");
    assert_eq!(refused.raw_os_error(), Some(libc::EACCES), "{refused}");
    assert!(stand.ask("LIST 1000 lang=en\n").1.starts_with("OK no"));
}

#[test]
fn a_live_service_keeps_its_name() {
    let stand = stand("fingerprint.elanfp-test-live", &[]);
    assert!(Claim::new(stand.scheme).is_err());
    assert!(stand.ask("LIST 1000 lang=en\n").1.starts_with("OK"));
}
