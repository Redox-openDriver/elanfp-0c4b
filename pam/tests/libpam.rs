use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io::{BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PAM_SUCCESS: c_int = 0;
const PAM_AUTH_ERR: c_int = 7;
const PAM_AUTHINFO_UNAVAIL: c_int = 9;
const PAM_USER_UNKNOWN: c_int = 10;
const PAM_RHOST: c_int = 4;
const PAM_RUSER: c_int = 8;
const NOBODY: &str = "no-such-account-elanfp-test";
const MODULE: &str = "libpam_elanfp.so";

#[repr(C)]
struct Message {
    style: c_int,
    text: *const c_char,
}

#[repr(C)]
struct Response {
    text: *mut c_char,
    code: c_int,
}

#[repr(C)]
struct Conversation {
    call: extern "C" fn(c_int, *mut *const Message, *mut *mut Response, *mut c_void) -> c_int,
    data: *mut c_void,
}

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start_confdir(
        service: *const c_char,
        user: *const c_char,
        conversation: *const Conversation,
        confdir: *const c_char,
        handle: *mut *mut c_void,
    ) -> c_int;
    fn pam_set_item(handle: *mut c_void, item: c_int, value: *const c_void) -> c_int;
    fn pam_authenticate(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(handle: *mut c_void, status: c_int) -> c_int;
}

extern "C" fn listen(count: c_int, messages: *mut *const Message, responses: *mut *mut Response, data: *mut c_void) -> c_int {
    let heard = unsafe { &*(data as *const Mutex<Vec<String>>) };
    for index in 0..count.max(0) as usize {
        let message = unsafe { &**messages.add(index) };
        heard.lock().unwrap().push(unsafe { CStr::from_ptr(message.text) }.to_string_lossy().into_owned());
    }
    unsafe { *responses = libc::calloc(count.max(1) as usize, std::mem::size_of::<Response>()).cast() };
    PAM_SUCCESS
}

fn own_uid() -> u32 {
    unsafe { libc::getuid() }
}

fn own_name() -> String {
    let output = std::process::Command::new("id").arg("-un").output().unwrap();
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn module() -> PathBuf {
    let test = std::env::current_exe().unwrap();
    let units = test.ancestors().nth(3).and_then(|package| std::fs::read_dir(package).ok()).into_iter().flatten().flatten();
    let places = std::iter::once(test.with_file_name(MODULE)).chain(units.map(|unit| unit.path().join("out").join(MODULE)));
    let newest = places.filter_map(|path| Some((path.metadata().ok()?.modified().ok()?, path))).max();
    newest.unwrap_or_else(|| panic!("{MODULE} is not built near {}", test.display())).1
}

struct Stand {
    dir: PathBuf,
    requests: Arc<Mutex<Vec<String>>>,
    connections: Arc<AtomicUsize>,
}

impl Stand {
    fn start(script: &'static [u8], options: &str) -> Stand {
        static COUNT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!("elanfp-pam-test-{}-{}", std::process::id(), COUNT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let socket = dir.join("socket");
        let line = format!("auth required {} socket={} service_user={} console=any timeout=1 {options}\n", module().display(), socket.display(), own_name());
        std::fs::write(dir.join("elanfp"), line).unwrap();
        let listener = UnixListener::bind(&socket).unwrap();
        let (requests, connections) = (Arc::new(Mutex::new(Vec::new())), Arc::new(AtomicUsize::new(0)));
        let (seen, counted) = (Arc::clone(&requests), Arc::clone(&connections));
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                counted.fetch_add(1, Ordering::Relaxed);
                let mut request = String::new();
                if BufReader::new(&stream).read_line(&mut request).is_ok_and(|read| read > 0) {
                    seen.lock().unwrap().push(request.trim_end().to_string());
                    let _ = stream.write_all(script);
                }
            }
        });
        Stand { dir, requests, connections }
    }

    fn authenticate(&self, user: &str, items: &[(c_int, &str)]) -> (c_int, Vec<String>) {
        let heard = Mutex::new(Vec::new());
        let conversation = Conversation { call: listen, data: (&raw const heard).cast_mut().cast() };
        let (service, user, confdir) = (CString::new("elanfp").unwrap(), CString::new(user).unwrap(), CString::new(self.dir.to_str().unwrap()).unwrap());
        let mut handle: *mut c_void = std::ptr::null_mut();
        let started = unsafe { pam_start_confdir(service.as_ptr(), user.as_ptr(), &conversation, confdir.as_ptr(), &mut handle) };
        assert_eq!(started, PAM_SUCCESS);
        for (item, value) in items {
            let value = CString::new(*value).unwrap();
            assert_eq!(unsafe { pam_set_item(handle, *item, value.as_ptr().cast()) }, PAM_SUCCESS);
        }
        let verdict = unsafe { pam_authenticate(handle, 0) };
        unsafe { pam_end(handle, verdict) };
        (verdict, heard.into_inner().unwrap())
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Stand {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn outcome(script: &'static [u8]) -> (c_int, Vec<String>) {
    Stand::start(script, "").authenticate(&own_name(), &[])
}

#[test]
fn only_a_yes_from_the_service_is_success() {
    let stand = Stand::start(b"MSG place your finger\nOK welcome\n", "");
    assert_eq!(stand.authenticate(&own_name(), &[]), (PAM_SUCCESS, vec!["place your finger".to_string()]));
    let requests = stand.requests();
    assert!(requests.len() == 1 && requests[0].starts_with(&format!("VERIFY {} wait=1", own_uid())), "{requests:?}");
    assert!(requests[0].split(' ').any(|word| word == "svc=elanfp"), "{requests:?}");
    assert_eq!(outcome(b"MSG place your finger\nNO not recognised\n"), (PAM_AUTH_ERR, vec!["place your finger".to_string(), "not recognised".to_string()]));
    assert_eq!(
        outcome(b"MSG place your finger\nERR nobody touched\n"),
        (PAM_AUTHINFO_UNAVAIL, vec!["place your finger".to_string(), "nobody touched".to_string()])
    );
    assert_eq!(outcome(b"ERR locked for a while\n"), (PAM_AUTHINFO_UNAVAIL, Vec::new()));
    assert_eq!(outcome(b"NO not recognised\n"), (PAM_AUTH_ERR, Vec::new()));
    assert_eq!(outcome(b"BUSY later\n").0, PAM_AUTHINFO_UNAVAIL);
}

#[test]
fn a_service_that_misbehaves_is_never_success() {
    for script in [&b""[..], b"OK", b"WHAT\n", b"\xff\xfe\n", b"MSG a\nMSG b\n", b"MSG a\n\nOK\n", b"ok\n", b" OK\n"] {
        assert_eq!(outcome(script).0, PAM_AUTHINFO_UNAVAIL, "{script:?}");
    }
    let (verdict, heard) = outcome(b"MSG clear\x1b[2Jscreen\x07\nOK fine\n");
    assert_eq!(verdict, PAM_SUCCESS);
    assert!(heard.iter().all(|line| !line.contains(['\x1b', '\x07'])), "{heard:?}");
}

#[test]
fn the_request_must_come_from_the_person_it_is_about() {
    let stand = Stand::start(b"OK welcome\n", "");
    assert_eq!(stand.authenticate(&own_name(), &[(PAM_RHOST, "example.org")]).0, PAM_AUTHINFO_UNAVAIL);
    assert_eq!(stand.authenticate(&own_name(), &[(PAM_RUSER, NOBODY)]).0, PAM_AUTHINFO_UNAVAIL);
    assert_eq!(stand.authenticate(NOBODY, &[]).0, PAM_USER_UNKNOWN);
    if own_uid() != 0 {
        assert_eq!(stand.authenticate("root", &[]).0, PAM_AUTHINFO_UNAVAIL);
    }
    assert_eq!(stand.connections.load(Ordering::Relaxed), 0);
    assert_eq!(stand.authenticate(&own_name(), &[(PAM_RUSER, &own_name())]).0, PAM_SUCCESS);
    assert_eq!(stand.authenticate(&own_name(), &[(PAM_RUSER, "root")]).0, PAM_SUCCESS);
    assert_eq!(stand.authenticate(&own_name(), &[(PAM_RHOST, "")]).0, PAM_SUCCESS);
    assert_eq!(stand.requests().len(), 3);
}

#[test]
fn a_socket_held_by_somebody_else_is_not_trusted() {
    if own_uid() == 0 {
        return;
    }
    let stand = Stand::start(b"OK welcome\n", "service_user=root");
    assert_eq!(stand.authenticate(&own_name(), &[]).0, PAM_AUTHINFO_UNAVAIL);
    assert!(stand.requests().is_empty());
    let unknown = Stand::start(b"OK welcome\n", &format!("service_user={NOBODY}"));
    assert_eq!(unknown.authenticate(&own_name(), &[]).0, PAM_AUTHINFO_UNAVAIL);
    assert_eq!(unknown.connections.load(Ordering::Relaxed), 0);
}

#[test]
fn a_service_that_does_not_accept_cannot_hang_the_login() {
    let dir = std::env::temp_dir().join(format!("elanfp-pam-test-{}-stuck", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("socket");
    let listener = UnixListener::bind(&socket).unwrap();
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 2) }, 0);
    let mut held = Vec::new();
    let full = loop {
        match elanfp_ipc::connect(&socket, Duration::from_secs(1)) {
            Ok(stream) => held.push(stream),
            Err(error) => break error,
        }
        assert!(held.len() < 16, "the queue never filled");
    };
    assert_eq!(full.kind(), std::io::ErrorKind::TimedOut);
    let line = format!("auth required {} socket={} service_user={} console=any timeout=1\n", module().display(), socket.display(), own_name());
    std::fs::write(dir.join("elanfp"), line).unwrap();
    let stand = Stand { dir, requests: Arc::default(), connections: Arc::default() };
    let started = Instant::now();
    assert_eq!(stand.authenticate(&own_name(), &[]).0, PAM_AUTHINFO_UNAVAIL);
    assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
    drop(listener);
}
