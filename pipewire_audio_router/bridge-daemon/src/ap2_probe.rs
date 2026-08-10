//! Application-level reachability probe for an AirPlay-2 receiver.
//!
//! **Why not a bare `TcpStream::connect`.** A receiver whose AirTunes process has
//! stopped calling `accept()` still completes the TCP handshake *in our kernel*: its
//! SYN-ACK arrives, we ACK, `connect()` returns `Ok` — and the peer then quietly
//! re-transmits that same SYN-ACK for the next minute, because with its accept queue
//! full it dropped our ACK and never finished the handshake on its side. It ACKs no
//! data and answers no request. `connect()` cannot see any of this, so a
//! connect-only probe reports such a receiver as healthy forever.
//!
//! Observed on a Pioneer VSX-934 (2026-08-10): `:7000` "open" to every `connect()`,
//! zero bytes ever returned to any RTSP request (`GET /info`, `OPTIONS`,
//! `/server-info`, plain HTTP), while the same unit's web UI (`:8080`) and eISCP
//! control (`:60128`) answered in milliseconds and its mDNS `_airplay._tcp` record
//! stayed live and byte-identical to a working Yamaha's. Only a mains power cycle
//! cleared it. The output sat green in the UI the whole time.
//!
//! A `GET /info` round-trip is the smallest check that proves the *service* is
//! alive rather than just the kernel — and it is exactly what the real connect path
//! sends first, so it fails in the same place a real connect would. Any reply at all
//! counts as alive: even a refusal proves an AirTunes process is reading the socket.
//! Verified harmless against a receiver mid-session (a Yamaha WX-021 streaming from
//! this daemon answered six probes in ~4 ms each without a glitch) — receivers serve
//! `/info` on a second connection by design, which is how real senders discover them.

use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// What a probe learned about a receiver's RTSP endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ap2Reach {
    /// The AirTunes service answered — it is running and reading its socket.
    Alive,
    /// TCP "connected" but the service never answered. Either its accept queue is
    /// full (a wedged AirTunes: see the module docs) or the process is hung. The
    /// device is *on the network* — so this must not be treated as "gone", only as
    /// "not usable" (removing it would flap against its still-live mDNS record).
    RtspSilent,
    /// Not reachable at all — powered off, asleep, or off the network.
    Unreachable(String),
}

impl Ap2Reach {
    /// A user-facing explanation, or `None` when healthy.
    pub fn fault_message(&self) -> Option<String> {
        match self {
            Ap2Reach::Alive => None,
            Ap2Reach::RtspSilent => Some(
                "AirPlay port accepts connections but the receiver never answers \
                 — its AirPlay service is wedged. Power-cycle the device (a standby \
                 toggle is usually not enough)."
                    .to_string(),
            ),
            Ap2Reach::Unreachable(why) => Some(format!("Receiver not reachable: {why}")),
        }
    }
}

/// The one plaintext request we probe with. Mirrors the first request the real
/// connect path sends (`RtspRequest::get_info`), headers included, so a receiver
/// that rejects *us* rejects the probe the same way.
const PROBE_REQUEST: &[u8] = b"GET /info RTSP/1.0\r\n\
CSeq: 1\r\n\
Content-Type: application/x-apple-binary-plist\r\n\
User-Agent: AirPlay/745.83\r\n\
\r\n";

/// Probe `addr`'s RTSP endpoint. `connect_timeout` bounds the handshake,
/// `reply_timeout` the wait for the receiver's first byte.
pub async fn probe(addr: SocketAddr, connect_timeout: Duration, reply_timeout: Duration) -> Ap2Reach {
    let mut stream = match tokio::time::timeout(connect_timeout, TcpStream::connect(addr)).await {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Ap2Reach::Unreachable(e.to_string()),
        Err(_) => return Ap2Reach::Unreachable(format!("no TCP handshake within {connect_timeout:?}")),
    };
    // A half-finished handshake (the wedged case) usually still absorbs the write
    // into our send buffer, so a write error is not the signal — the missing *reply*
    // is. Treat a write failure as silence too: either way nothing came back.
    if tokio::time::timeout(reply_timeout, stream.write_all(PROBE_REQUEST)).await.is_err() {
        return Ap2Reach::RtspSilent;
    }
    let mut buf = [0u8; 64];
    match tokio::time::timeout(reply_timeout, stream.read(&mut buf)).await {
        // Any bytes ⇒ an AirTunes process is reading and answering.
        Ok(Ok(n)) if n > 0 => Ap2Reach::Alive,
        // Clean EOF or a read error: the service closed on us rather than serving.
        // Not "gone from the network" — same treatment as silence.
        Ok(_) => Ap2Reach::RtspSilent,
        Err(_) => Ap2Reach::RtspSilent,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[tokio::test]
    async fn nothing_listening_is_unreachable() {
        // Bind then drop, so the port is (almost certainly) free and refuses.
        let addr = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        assert!(matches!(probe(addr, ms(500), ms(500)).await, Ap2Reach::Unreachable(_)));
    }

    #[tokio::test]
    async fn a_listener_that_never_answers_is_rtsp_silent() {
        // The wedged-receiver shape we cannot detect with connect() alone: the
        // handshake completes, but nothing ever accepts/answers.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Deliberately never accept: the backlog completes the handshake for us.
        assert_eq!(probe(addr, ms(500), ms(300)).await, Ap2Reach::RtspSilent);
        drop(listener);
    }

    #[tokio::test]
    async fn accepted_but_silent_is_rtsp_silent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let h = tokio::spawn(async move {
            let (s, _) = listener.accept().await.unwrap();
            // Hold the socket open, read nothing, write nothing.
            tokio::time::sleep(ms(800)).await;
            drop(s);
        });
        assert_eq!(probe(addr, ms(500), ms(300)).await, Ap2Reach::RtspSilent);
        h.await.unwrap();
    }

    #[tokio::test]
    async fn any_reply_counts_as_alive() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let h = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 256];
            let _ = s.read(&mut buf).await;
            // Even a refusal proves the service is alive.
            let _ = s.write_all(b"RTSP/1.0 403 Forbidden\r\nCSeq: 1\r\n\r\n").await;
        });
        assert_eq!(probe(addr, ms(500), ms(1000)).await, Ap2Reach::Alive);
        h.await.unwrap();
    }

    #[test]
    fn only_faults_carry_a_message() {
        assert!(Ap2Reach::Alive.fault_message().is_none());
        assert!(Ap2Reach::RtspSilent.fault_message().unwrap().contains("Power-cycle"));
        assert!(Ap2Reach::Unreachable("boom".into()).fault_message().unwrap().contains("boom"));
    }
}
