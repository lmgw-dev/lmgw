//! TCP keep-alive on the device listener's connections (device link design
//! §4.3, changes of 2026-10-24): a peer that vanishes without a FIN or a
//! reset (a phone that lost its Wi-Fi) is noticed by the kernel's probes,
//! not when TCP's retransmissions give up many minutes later, so a chat
//! stream it opened stops its model call as at a hang-up.
//!
//! The values come from `chat_feed_keepalive_s`, the interval a client is
//! already told to derive its own dead-link timeout from. Every accepted
//! connection gets `SO_KEEPALIVE` with the first probe after that many idle
//! seconds and one more every that many seconds, at the kernel's probe count
//! (`TCP_KEEPCNT`, 9 by default). Read at each accept, so a change applies
//! to connections accepted after it.
//!
//! `TCP_USER_TIMEOUT` is not set at the accept: since Linux 5.11 a zero
//! receive window that lasts longer than it ends the connection though the
//! peer acknowledges every probe, so a healthy reader that stopped reading
//! for a while — a phone's player whose buffer is full on a long speech
//! download, an app frozen in the background with a large answer pending —
//! would be cut after it. It is set only for as long as a streamed chat
//! answer runs on the connection ([`chat_stream`], at twice the keep-alive):
//! chat completions, `/v1/messages`, a streamed `/v1/responses` and the
//! legacy completions stream, whose reader keeps reading as the model
//! writes. Never on audio, speech or transfer downloads. A device feed sets
//! its own stream's keep-alive and `TCP_USER_TIMEOUT` the same way
//! (`web::chat_feed`, §7.1). Each stream's values go when it ends, back to
//! the accept-time ones ([`crate::server::TimersOverride`]).

use std::time::Duration;

use crate::server::{Conn, TimersOverride};

/// The keep-alive timers of one device listener connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timers {
    /// Idle time before the first probe, and the time between probes.
    pub every: Duration,
}

impl Timers {
    /// The timers `settings` ask for now.
    pub fn of(settings: &crate::config::Settings) -> Self {
        Self {
            every: Duration::from_secs(u64::from(settings.chat_feed_keepalive_s.max(1))),
        }
    }

    /// The `TCP_USER_TIMEOUT` a streamed chat answer sets while it runs:
    /// twice the keep-alive.
    pub fn chat_user_timeout(&self) -> Duration {
        self.every * 2
    }

    /// What the listener's start line says of them.
    pub fn describe(&self) -> String {
        format!(
            "TCP keep-alive after {} s idle and every {} s at the kernel's probe count; \
             TCP_USER_TIMEOUT {} s only while a streamed chat answer runs (chat completions, \
             messages, responses, legacy completions), never on audio, speech or transfer \
             downloads (from chat_feed_keepalive_s)",
            self.every.as_secs(),
            self.every.as_secs(),
            self.chat_user_timeout().as_secs()
        )
    }

    /// Set the keep-alive on `socket`. A failure is the caller's to log:
    /// the connection serves on without it.
    pub fn apply(&self, socket: socket2::SockRef<'_>) -> std::io::Result<()> {
        socket.set_tcp_keepalive(
            &socket2::TcpKeepalive::new()
                .with_time(self.every)
                .with_interval(self.every),
        )
    }
}

/// `TCP_USER_TIMEOUT` at [`Timers::chat_user_timeout`] on the device
/// listener connection `conn` a streamed chat answer is starting on, for as
/// long as the returned guard lives (module doc): data the vanished peer
/// never acknowledges ends the connection then, and the stream with it.
/// `None` for any other connection (the plain listener's carries no socket
/// handle), and when it could not be set, which is logged: the stream runs
/// on without it.
pub(crate) fn chat_stream(
    conn: Option<&Conn>,
    settings: &crate::config::Settings,
) -> Option<TimersOverride> {
    let socket = conn?.socket.as_ref()?;
    let timeout = Timers::of(settings).chat_user_timeout();
    socket
        .override_timers(None, Some(timeout))
        .inspect_err(|e| tracing::warn!("chat stream: TCP_USER_TIMEOUT not set: {e}"))
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings maximum is the kernel's: the longest keep-alive a feed
    /// may be told sets as a socket timer, and one second more is what Linux
    /// refuses (EINVAL), which would leave a stream without timers.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_longest_keepalive_the_settings_allow_is_one_the_kernel_accepts() {
        use lmgw_api_types::chat_feed::MAX_KEEPALIVE_S;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let sock = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let at = |s: u64| Timers {
            every: Duration::from_secs(s),
        };
        at(u64::from(MAX_KEEPALIVE_S))
            .apply(socket2::SockRef::from(&sock))
            .expect("the settings maximum is accepted");
        assert!(
            at(u64::from(MAX_KEEPALIVE_S) + 1)
                .apply(socket2::SockRef::from(&sock))
                .is_err(),
            "MAX_KEEPALIVE_S is the largest the kernel accepts; raise it only with the kernel"
        );
    }

    #[test]
    fn the_timers_follow_the_feed_keepalive() {
        let s = crate::config::Settings {
            chat_feed_keepalive_s: 15,
            ..Default::default()
        };
        let t = Timers::of(&s);
        assert_eq!(t.every, Duration::from_secs(15));
        assert_eq!(t.chat_user_timeout(), Duration::from_secs(30));
        assert!(
            t.describe().starts_with(
                "TCP keep-alive after 15 s idle and every 15 s at the kernel's probe count; \
                 TCP_USER_TIMEOUT 30 s only while a streamed chat answer runs"
            ),
            "{}",
            t.describe()
        );
    }
}
