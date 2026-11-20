use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::time::Duration;

use elanfp_ipc::{MAX_WAIT_S, MIN_WAIT_S, Reply, Request, SYSTEM_SOCKET, Verb, peer_uid, uid_of};

const PAM_SUCCESS: c_int = 0;
const PAM_AUTH_ERR: c_int = 7;
const PAM_AUTHINFO_UNAVAIL: c_int = 9;
const PAM_USER_UNKNOWN: c_int = 10;
const PAM_SILENT: c_int = 0x8000;
const PAM_SERVICE: c_int = 1;
const PAM_RHOST: c_int = 4;
const PAM_CONV: c_int = 5;
const PAM_RUSER: c_int = 8;
const PAM_TEXT_INFO: c_int = 4;

const ROOT: u32 = 0;
const DEFAULT_WAIT_S: u16 = 10;
const DEFAULT_SERVICE_USER: &str = "elanfp";
const STEP_MARGIN: Duration = Duration::from_secs(15);
const TOTAL: Duration = Duration::from_secs(120);
const CONSOLE_SEAT: &CStr = c"seat0";
const GREETER_CLASS: &CStr = c"greeter";
const LOGIND_LIBRARIES: [&CStr; 2] = [c"libsystemd.so.0", c"libelogind.so.0"];

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

type ConvFn = unsafe extern "C" fn(c_int, *mut *const PamMessage, *mut *mut PamResponse, *mut c_void) -> c_int;

#[repr(C)]
struct PamConv {
    conv: Option<ConvFn>,
    appdata_ptr: *mut c_void,
}

#[link(name = "pam")]
unsafe extern "C" {
    fn pam_get_user(pamh: *mut c_void, user: *mut *const c_char, prompt: *const c_char) -> c_int;
    fn pam_get_item(pamh: *const c_void, item_type: c_int, item: *mut *const c_void) -> c_int;
}

type PidGetSession = unsafe extern "C" fn(libc::pid_t, *mut *mut c_char) -> c_int;
type SessionIsRemote = unsafe extern "C" fn(*const c_char) -> c_int;
type SeatGetActive = unsafe extern "C" fn(*const c_char, *mut *mut c_char, *mut libc::uid_t) -> c_int;
type SessionGetClass = unsafe extern "C" fn(*const c_char, *mut *mut c_char) -> c_int;

struct Logind {
    library: *mut c_void,
    pid_get_session: PidGetSession,
    session_is_remote: SessionIsRemote,
    seat_get_active: SeatGetActive,
    session_get_class: SessionGetClass,
}

impl Logind {
    fn open(name: &CStr) -> Option<Logind> {
        let library = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if library.is_null() {
            return None;
        }
        let symbol = |name: &CStr| {
            let address = unsafe { libc::dlsym(library, name.as_ptr()) };
            (!address.is_null()).then_some(address)
        };
        let symbols = (symbol(c"sd_pid_get_session"), symbol(c"sd_session_is_remote"), symbol(c"sd_seat_get_active"), symbol(c"sd_session_get_class"));
        let (Some(pid_get_session), Some(session_is_remote), Some(seat_get_active), Some(session_get_class)) = symbols else {
            unsafe { libc::dlclose(library) };
            return None;
        };
        Some(unsafe {
            Logind {
                library,
                pid_get_session: std::mem::transmute::<*mut c_void, PidGetSession>(pid_get_session),
                session_is_remote: std::mem::transmute::<*mut c_void, SessionIsRemote>(session_is_remote),
                seat_get_active: std::mem::transmute::<*mut c_void, SeatGetActive>(seat_get_active),
                session_get_class: std::mem::transmute::<*mut c_void, SessionGetClass>(session_get_class),
            }
        })
    }

    fn load() -> Option<Logind> {
        LOGIND_LIBRARIES.iter().find_map(|name| Logind::open(name))
    }

    fn caller_is_remote(&self) -> bool {
        let mut session: *mut c_char = std::ptr::null_mut();
        if unsafe { (self.pid_get_session)(0, &mut session) } < 0 || session.is_null() {
            return false;
        }
        let session = Owned(session);
        unsafe { (self.session_is_remote)(session.0) != 0 }
    }

    fn console_belongs_to(&self, target: u32) -> bool {
        let mut session: *mut c_char = std::ptr::null_mut();
        let mut owner: libc::uid_t = libc::uid_t::MAX;
        if unsafe { (self.seat_get_active)(CONSOLE_SEAT.as_ptr(), &mut session, &mut owner) } < 0 || session.is_null() {
            return false;
        }
        let session = Owned(session);
        if unsafe { (self.session_is_remote)(session.0) } != 0 {
            return false;
        }
        if owner == target {
            return true;
        }
        let mut class: *mut c_char = std::ptr::null_mut();
        if unsafe { (self.session_get_class)(session.0, &mut class) } < 0 || class.is_null() {
            return false;
        }
        Owned(class).text() == GREETER_CLASS
    }
}

impl Drop for Logind {
    fn drop(&mut self) {
        unsafe { libc::dlclose(self.library) };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    Logind,
    Any,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub socket: String,
    pub wait: u16,
    pub service_user: String,
    pub console: Console,
}

impl Default for Options {
    fn default() -> Self {
        Options { socket: SYSTEM_SOCKET.to_string(), wait: DEFAULT_WAIT_S, service_user: DEFAULT_SERVICE_USER.to_string(), console: Console::Logind }
    }
}

impl Options {
    pub fn parse<'a>(args: impl IntoIterator<Item = &'a str>) -> Self {
        let mut options = Options::default();
        for arg in args {
            match arg.split_once('=') {
                Some(("socket", path)) if path.starts_with('/') => options.socket = path.to_string(),
                Some(("timeout", seconds)) => {
                    if let Ok(seconds) = seconds.parse::<u16>() {
                        options.wait = seconds.clamp(MIN_WAIT_S, MAX_WAIT_S);
                    }
                }
                Some(("service_user", name)) if !name.is_empty() => options.service_user = name.to_string(),
                Some(("console", "any")) => options.console = Console::Any,
                Some(("console", "logind")) => options.console = Console::Logind,
                _ => {}
            }
        }
        options
    }
}

struct Pam {
    handle: *mut c_void,
    silent: bool,
}

impl Pam {
    fn item_string(&self, kind: c_int) -> Option<String> {
        let mut item: *const c_void = std::ptr::null();
        let rc = unsafe { pam_get_item(self.handle, kind, &mut item) };
        if rc != PAM_SUCCESS || item.is_null() {
            return None;
        }
        Some(unsafe { CStr::from_ptr(item.cast()) }.to_string_lossy().into_owned())
    }

    fn user(&self) -> Option<String> {
        let mut user: *const c_char = std::ptr::null();
        let rc = unsafe { pam_get_user(self.handle, &mut user, std::ptr::null()) };
        if rc != PAM_SUCCESS || user.is_null() {
            return None;
        }
        unsafe { CStr::from_ptr(user) }.to_str().ok().map(str::to_string)
    }

    fn info(&self, text: &str) {
        if self.silent {
            return;
        }
        let mut item: *const c_void = std::ptr::null();
        if unsafe { pam_get_item(self.handle, PAM_CONV, &mut item) } != PAM_SUCCESS || item.is_null() {
            return;
        }
        let conv = unsafe { &*(item as *const PamConv) };
        let Some(call) = conv.conv else { return };
        let Ok(text) = CString::new(text.replace('\0', " ")) else { return };
        let message = PamMessage { msg_style: PAM_TEXT_INFO, msg: text.as_ptr() };
        let mut list: *const PamMessage = &message;
        let mut response: *mut PamResponse = std::ptr::null_mut();
        unsafe {
            call(1, &mut list, &mut response, conv.appdata_ptr);
            if !response.is_null() {
                if !(*response).resp.is_null() {
                    libc::free((*response).resp.cast());
                }
                libc::free(response.cast());
            }
        }
    }
}

struct Owned(*mut c_char);

impl Owned {
    fn text(&self) -> &CStr {
        unsafe { CStr::from_ptr(self.0) }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        unsafe { libc::free(self.0.cast()) };
    }
}

fn at_the_console(console: Console, target: u32) -> bool {
    match (Logind::load(), console) {
        (Some(logind), _) if logind.caller_is_remote() => false,
        (Some(logind), Console::Logind) => logind.console_belongs_to(target),
        (None, Console::Logind) => false,
        (_, Console::Any) => true,
    }
}

fn asked_by_its_owner(invoker: u32, requester: Option<Option<u32>>, target: u32) -> bool {
    let own = |uid: u32| uid == ROOT || uid == target;
    own(invoker) && requester.is_none_or(|known| known.is_some_and(own))
}

fn authenticate(pam: &Pam, options: &Options) -> c_int {
    if pam.item_string(PAM_RHOST).is_some_and(|host| !host.is_empty()) {
        return PAM_AUTHINFO_UNAVAIL;
    }
    let Some(uid) = pam.user().and_then(|name| uid_of(&name)) else {
        return PAM_USER_UNKNOWN;
    };
    let requester = pam.item_string(PAM_RUSER).filter(|name| !name.is_empty()).map(|name| uid_of(&name));
    if !asked_by_its_owner(unsafe { libc::getuid() }, requester, uid) {
        return PAM_AUTHINFO_UNAVAIL;
    }
    if !at_the_console(options.console, uid) {
        return PAM_AUTHINFO_UNAVAIL;
    }
    let Some(service_uid) = uid_of(&options.service_user) else {
        return PAM_AUTHINFO_UNAVAIL;
    };
    let step = Duration::from_secs(u64::from(options.wait)) + STEP_MARGIN;
    let Ok(stream) = elanfp_ipc::connect(Path::new(&options.socket), step) else {
        return PAM_AUTHINFO_UNAVAIL;
    };
    if !peer_uid(&stream).is_ok_and(|peer| peer == service_uid || peer == ROOT) {
        return PAM_AUTHINFO_UNAVAIL;
    }
    let program = pam.item_string(PAM_SERVICE).filter(|name| elanfp_ipc::valid_name(name));
    let request = Request::new(Verb::Verify, uid).wait(Some(options.wait)).lang(elanfp_ipc::environment_language()).program(program);
    let mut interacted = false;
    let reply = elanfp_ipc::exchange(stream, &request, step, TOTAL, |text| {
        interacted = true;
        pam.info(text);
    });
    let (text, verdict) = match reply {
        Ok(Reply::Ok(_)) => return PAM_SUCCESS,
        Ok(Reply::No(text)) => (text, PAM_AUTH_ERR),
        Ok(Reply::Err(text)) => (text, PAM_AUTHINFO_UNAVAIL),
        Ok(Reply::Busy(_) | Reply::Message(_)) | Err(_) => return PAM_AUTHINFO_UNAVAIL,
    };
    if interacted {
        pam.info(&text);
    }
    verdict
}

fn arguments(argc: c_int, argv: *const *const c_char) -> Vec<String> {
    if argv.is_null() {
        return Vec::new();
    }
    (0..argc.max(0) as usize)
        .filter_map(|i| {
            let arg = unsafe { *argv.add(i) };
            (!arg.is_null()).then(|| unsafe { CStr::from_ptr(arg) }.to_string_lossy().into_owned())
        })
        .collect()
}

#[unsafe(no_mangle)]
pub extern "C" fn pam_sm_authenticate(pamh: *mut c_void, flags: c_int, argc: c_int, argv: *const *const c_char) -> c_int {
    std::panic::catch_unwind(|| {
        let pam = Pam { handle: pamh, silent: flags & PAM_SILENT != 0 };
        let args = arguments(argc, argv);
        authenticate(&pam, &Options::parse(args.iter().map(String::as_str)))
    })
    .unwrap_or(PAM_AUTHINFO_UNAVAIL)
}

#[unsafe(no_mangle)]
pub extern "C" fn pam_sm_setcred(_pamh: *mut c_void, _flags: c_int, _argc: c_int, _argv: *const *const c_char) -> c_int {
    PAM_SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_have_safe_defaults_and_bounds() {
        assert_eq!(Options::parse([]), Options { socket: SYSTEM_SOCKET.into(), wait: 10, service_user: "elanfp".into(), console: Console::Logind });
        let o = Options::parse(["timeout=5", "socket=/run/other/socket", "service_user=svc", "console=any", "debug", "unknown=1"]);
        assert_eq!(o, Options { socket: "/run/other/socket".into(), wait: 5, service_user: "svc".into(), console: Console::Any });
        assert_eq!(Options::parse(["console=any", "console=logind"]).console, Console::Logind);
        assert_eq!(Options::parse(["console=", "console=none", "console=ANY"]).console, Console::Logind);
        assert_eq!(Options::parse(["timeout=0"]).wait, MIN_WAIT_S);
        assert_eq!(Options::parse(["timeout=9999"]).wait, MAX_WAIT_S);
        assert_eq!(Options::parse(["timeout=abc", "socket=relative/path", "service_user="]), Options::default());
    }

    #[test]
    fn missing_login_manager_library_fails_closed_unless_the_admin_opted_out() {
        assert!(Logind::open(c"libelanfp-no-such-library.so.0").is_none());
        assert!(Logind::open(c"libc.so.6").is_none());
        if let Some(logind) = Logind::load() {
            assert!(!logind.console_belongs_to(u32::MAX - 1));
        }
    }

    #[test]
    fn a_fingerprint_is_asked_only_by_its_owner_or_by_root() {
        const USER: u32 = 1000;
        const OTHER: u32 = 1001;
        assert!(asked_by_its_owner(USER, None, USER));
        assert!(asked_by_its_owner(USER, Some(Some(USER)), USER));
        assert!(asked_by_its_owner(ROOT, None, USER));
        assert!(asked_by_its_owner(ROOT, Some(Some(ROOT)), USER));
        assert!(asked_by_its_owner(ROOT, Some(Some(USER)), USER));
        assert!(!asked_by_its_owner(OTHER, None, USER));
        assert!(!asked_by_its_owner(OTHER, Some(Some(USER)), USER));
        assert!(!asked_by_its_owner(USER, Some(Some(OTHER)), USER));
        assert!(!asked_by_its_owner(ROOT, Some(Some(OTHER)), USER));
        assert!(!asked_by_its_owner(ROOT, Some(None), USER));
        assert!(!asked_by_its_owner(USER, None, ROOT));
        assert!(asked_by_its_owner(ROOT, None, ROOT));
    }
}
