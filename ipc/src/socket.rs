use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::{CallError, Reply, Request, converse};

pub const SYSTEM_SOCKET: &str = "/run/elanfp/socket";
const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

pub fn user_socket() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR").filter(|d| !d.is_empty()).map(|d| PathBuf::from(d).join("elanfp.socket"))
}

fn dial(path: &Path) -> std::io::Result<UnixStream> {
    let name = path.as_os_str().as_bytes();
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if name.is_empty() || name.len() >= address.sun_path.len() || name.contains(&0) {
        return Err(std::io::ErrorKind::InvalidInput.into());
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (slot, byte) in address.sun_path.iter_mut().zip(name) {
        *slot = *byte as libc::c_char;
    }
    let length = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + name.len() + 1) as libc::socklen_t;
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        stream.set_write_timeout(Some(left))?;
        if unsafe { libc::connect(fd, (&raw const address).cast(), length) } == 0 {
            return Ok(stream);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINTR) => {}
            Some(libc::EAGAIN) => return Err(std::io::ErrorKind::TimedOut.into()),
            _ => return Err(error),
        }
    }
}

pub fn harden_process() -> std::io::Result<()> {
    for (option, value) in [(libc::PR_SET_DUMPABLE, 0), (libc::PR_SET_NO_NEW_PRIVS, 1)] {
        if unsafe { libc::prctl(option, value as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    unsafe { libc::umask(0o077) };
    Ok(())
}

pub fn connect(path: &Path, step_timeout: Duration) -> std::io::Result<UnixStream> {
    let stream = dial(path)?;
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    stream.set_read_timeout(Some(step_timeout))?;
    Ok(stream)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub uid: u32,
    pub pid: u32,
}

pub fn peer_uid(stream: &UnixStream) -> std::io::Result<u32> {
    peer(stream).map(|peer| peer.uid)
}

pub fn peer(stream: &UnixStream) -> std::io::Result<Peer> {
    let mut cred = libc::ucred { pid: 0, uid: u32::MAX, gid: u32::MAX };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe { libc::getsockopt(stream.as_raw_fd(), libc::SOL_SOCKET, libc::SO_PEERCRED, (&raw mut cred).cast(), &mut len) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if len as usize != std::mem::size_of::<libc::ucred>() || cred.uid == u32::MAX {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    Ok(Peer { uid: cred.uid, pid: u32::try_from(cred.pid).unwrap_or(0) })
}

const ACCOUNT_BUFFER: usize = 16384;

pub fn uid_of(name: &str) -> Option<u32> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut buffer = vec![0 as libc::c_char; ACCOUNT_BUFFER];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwnam_r(name.as_ptr(), &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut found) };
    (rc == 0 && !found.is_null()).then_some(entry.pw_uid)
}

pub fn account_exists(uid: u32) -> bool {
    let mut buffer = vec![0 as libc::c_char; ACCOUNT_BUFFER];
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut found: *mut libc::passwd = std::ptr::null_mut();
    let rc = unsafe { libc::getpwuid_r(uid, &mut entry, buffer.as_mut_ptr(), buffer.len(), &mut found) };
    rc == 0 && !found.is_null()
}

pub fn call(path: &Path, request: &Request, step_timeout: Duration, on_message: impl FnMut(&str)) -> Result<Reply, CallError> {
    converse(connect(path, step_timeout)?, request, on_message)
}

pub struct Bounded {
    stream: UnixStream,
    step: Duration,
    until: Instant,
}

impl Bounded {
    pub fn new(stream: UnixStream, step: Duration, total: Duration) -> Self {
        Bounded { stream, step, until: Instant::now() + total }
    }
}

impl Read for Bounded {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.until.checked_duration_since(Instant::now()).filter(|left| !left.is_zero()).ok_or(std::io::ErrorKind::TimedOut)?;
        self.stream.set_read_timeout(Some(left.min(self.step)))?;
        self.stream.read(buf)
    }
}

impl Write for Bounded {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.stream.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.stream.flush()
    }
}

pub fn exchange(stream: UnixStream, request: &Request, step: Duration, total: Duration, on_message: impl FnMut(&str)) -> Result<Reply, CallError> {
    stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
    converse(Bounded::new(stream, step, total), request, on_message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MAX_REPLY_LINE, Verb};

    fn verify(uid: u32) -> Request {
        Request::new(Verb::Verify, uid)
    }

    #[test]
    fn call_streams_messages_and_survives_a_hostile_peer() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("elanfp-ipc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let scripts: [&[u8]; 5] = [b"MSG one\nMSG two\nOK done\n", b"MSG partial", b"\xff\xfe\n", b"", b"OK \n"];
        let server = std::thread::spawn(move || {
            for script in scripts {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0u8; 64];
                let _ = stream.read(&mut request).unwrap();
                stream.write_all(script).unwrap();
            }
            let (mut stream, _) = listener.accept().unwrap();
            let _ = stream.read(&mut [0u8; 64]).unwrap();
            let flood = vec![b'x'; MAX_REPLY_LINE * 3];
            let _ = stream.write_all(&flood);
            let (stream, _) = listener.accept().unwrap();
            std::thread::sleep(Duration::from_millis(300));
            drop(stream);
        });
        let step = Duration::from_millis(100);
        let mut seen = Vec::new();
        let reply = call(&path, &verify(1000), step, |m| seen.push(m.to_string())).unwrap();
        assert_eq!((reply, seen), (Reply::Ok("done".into()), vec!["one".to_string(), "two".to_string()]));
        assert!(matches!(call(&path, &verify(1000), step, |_| {}), Err(CallError::Garbled)));
        assert!(matches!(call(&path, &verify(1000), step, |_| {}), Err(CallError::Garbled)));
        assert!(matches!(call(&path, &verify(1000), step, |_| {}), Err(CallError::Closed)));
        assert_eq!(call(&path, &verify(1000), step, |_| {}).unwrap(), Reply::Ok(String::new()));
        assert!(matches!(call(&path, &verify(1000), step, |_| {}), Err(CallError::Garbled)));
        assert!(matches!(call(&path, &verify(1000), step, |_| {}), Err(CallError::Io(_))));
        assert!(matches!(call(&dir.join("absent"), &verify(1000), step, |_| {}), Err(CallError::Io(_))));
        server.join().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn connecting_gives_up_when_nobody_accepts() {
        use std::os::unix::net::UnixListener;
        let dir = std::env::temp_dir().join(format!("elanfp-ipc-backlog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 2) }, 0);
        let started = Instant::now();
        let mut held = Vec::new();
        let refused = loop {
            match connect(&path, Duration::from_secs(1)) {
                Ok(stream) => held.push(stream),
                Err(error) => break error,
            }
            assert!(held.len() < 16, "the backlog never filled");
        };
        assert_eq!(refused.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() >= CONNECT_TIMEOUT && started.elapsed() < CONNECT_TIMEOUT * 4, "{:?}", started.elapsed());
        assert_eq!(peer_uid(&held[0]).unwrap(), unsafe { libc::getuid() });
        assert!(matches!(connect(&dir.join("absent"), Duration::from_secs(1)), Err(error) if error.kind() == std::io::ErrorKind::NotFound));
        let long = dir.join("x".repeat(200));
        assert!(matches!(connect(&long, Duration::from_secs(1)), Err(error) if error.kind() == std::io::ErrorKind::InvalidInput));
        drop(listener);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn accounts_resolve_both_ways() {
        assert_eq!(uid_of("root"), Some(0));
        assert_eq!(uid_of("no-such-account-elanfp-test"), None);
        assert_eq!(uid_of("bad\0name"), None);
        assert!(account_exists(0) && account_exists(unsafe { libc::getuid() }) && !account_exists(u32::MAX - 7));
    }

    #[test]
    fn a_dripping_peer_cannot_stretch_an_exchange_past_its_total() {
        let (client, mut server) = UnixStream::pair().unwrap();
        let dripper = std::thread::spawn(move || {
            let _ = server.read(&mut [0u8; 64]);
            for _ in 0..40 {
                if server.write_all(b"MSG still here\n").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let started = Instant::now();
        let mut lines = 0;
        let outcome = exchange(client, &verify(1000), Duration::from_millis(200), Duration::from_millis(400), |_| lines += 1);
        assert!(matches!(outcome, Err(CallError::Io(_))), "{outcome:?}");
        assert!(started.elapsed() < Duration::from_millis(900), "{:?}", started.elapsed());
        assert!(lines >= 1);
        dripper.join().unwrap();
    }
}
