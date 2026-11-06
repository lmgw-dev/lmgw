//! The socket options of this process's own descriptors, read with
//! `getsockopt`: what a test checks the gateway set on a socket it accepted.

/// One integer option of descriptor `fd`; `None` when it is no socket that
/// has it (or it closed meanwhile).
pub fn int_opt(fd: i32, level: libc::c_int, name: libc::c_int) -> Option<i64> {
    let mut v: libc::c_int = 0;
    let mut len = std::mem::size_of::<libc::c_int>() as libc::socklen_t;
    // SAFETY: `v` and `len` are valid for the call; a descriptor that is no
    // socket (or closed meanwhile) is an error.
    let r = unsafe { libc::getsockopt(fd, level, name, (&raw mut v).cast(), &raw mut len) };
    (r == 0).then_some(i64::from(v))
}

/// The TCP keep-alive options of one descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeepAlive {
    pub on: bool,
    pub idle_s: i64,
    pub interval_s: i64,
    pub user_timeout_ms: i64,
}

/// Those of every descriptor of this process that is a TCP socket.
pub fn keepalives() -> Vec<KeepAlive> {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|e| e.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .filter_map(|fd| {
            Some(KeepAlive {
                on: int_opt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE)? != 0,
                idle_s: int_opt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPIDLE)?,
                interval_s: int_opt(fd, libc::IPPROTO_TCP, libc::TCP_KEEPINTVL)?,
                user_timeout_ms: int_opt(fd, libc::IPPROTO_TCP, libc::TCP_USER_TIMEOUT)?,
            })
        })
        .collect()
}
