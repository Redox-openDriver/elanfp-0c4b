use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;

pub use elanfp_ipc::{Peer, account_exists, harden_process, peer, peer_uid, uid_of};

pub fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

pub fn effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

pub fn peer_alive(stream: &UnixStream) -> bool {
    let mut poll = libc::pollfd { fd: stream.as_raw_fd(), events: libc::POLLRDHUP, revents: 0 };
    let ready = unsafe { libc::poll(&mut poll, 1, 0) };
    ready == 0 || (ready > 0 && poll.revents & (libc::POLLRDHUP | libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_credentials_and_liveness_follow_the_socket() {
        let (a, b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a).unwrap(), current_uid());
        assert!(peer_alive(&a));
        drop(b);
        assert!(!peer_alive(&a));
    }

    #[test]
    fn pending_bytes_neither_look_like_a_hangup_nor_hide_one() {
        use std::io::Write;
        let (a, mut b) = UnixStream::pair().unwrap();
        b.write_all(b"x").unwrap();
        assert!(peer_alive(&a));
        assert!(peer_alive(&a));
        drop(b);
        assert!(!peer_alive(&a));
        let (a, mut b) = UnixStream::pair().unwrap();
        b.write_all(b"unread").unwrap();
        b.shutdown(std::net::Shutdown::Write).unwrap();
        assert!(!peer_alive(&a));
    }
}
