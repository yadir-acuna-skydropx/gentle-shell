//! Peer-credential check for the unix socket (S34 security): only the same user
//! may attach. The 0700 dir + 0600 socket already keep others out; this is a
//! defense-in-depth check of the connecting peer's uid via SO_PEERCRED.

use std::io;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;

/// Our own effective uid.
pub fn self_uid() -> u32 {
    // getuid never fails.
    unsafe { libc::getuid() }
}

/// Reads the connected peer's uid from the socket. Linux and Android expose it
/// via `SO_PEERCRED` + `ucred`; macOS and the BSDs expose it via `getpeereid`.
/// Both return the peer's effective uid, so the same-user check is identical on
/// every platform.
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let fd = stream.as_raw_fd();
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: getsockopt fills `cred` with `len` bytes; both out-params are valid.
    let rc = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(cred.uid)
}

/// macOS / BSD variant: `getpeereid` writes the connected peer's effective uid
/// and gid; we authorize on the uid exactly like the Linux path above.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn peer_uid(stream: &UnixStream) -> io::Result<u32> {
    let fd = stream.as_raw_fd();
    let mut uid: libc::uid_t = 0;
    let mut gid: libc::gid_t = 0;
    // SAFETY: getpeereid writes the peer's effective uid/gid into the out-params.
    let rc = unsafe { libc::getpeereid(fd, &mut uid, &mut gid) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    let _ = gid;
    Ok(uid)
}

/// Decides whether a peer with `peer` uid may attach given our `own` uid.
/// Root (uid 0) is intentionally NOT special-cased: only an exact match passes.
pub fn is_authorized(peer: u32, own: u32) -> bool {
    peer == own
}

/// Convenience: true when `stream`'s peer is the same user as us.
pub fn authorize(stream: &UnixStream) -> bool {
    match peer_uid(stream) {
        Ok(uid) => is_authorized(uid, self_uid()),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    #[test]
    fn only_the_same_uid_is_authorized() {
        assert!(is_authorized(1000, 1000));
        assert!(!is_authorized(1000, 1001));
        // Root is not special-cased.
        assert!(!is_authorized(0, 1000));
        assert!(!is_authorized(1000, 0));
    }

    #[test]
    fn socketpair_peer_uid_matches_our_own() {
        // A socketpair's two ends share our uid, so the peer check must pass.
        let (a, _b) = UnixStream::pair().unwrap();
        assert_eq!(peer_uid(&a).unwrap(), self_uid());
        assert!(authorize(&a));
    }
}
