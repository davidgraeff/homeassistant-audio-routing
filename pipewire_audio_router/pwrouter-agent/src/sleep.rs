//! logind's suspend/shutdown signal, and the delay inhibitor that buys time to answer
//! it (docs/receiver-agent.md §9.5).
//!
//! The problem this solves is that a suspending machine says nothing. Its socket is
//! never closed — the kernel freezes mid-connection — so the add-on believes the host is
//! connected and streaming until TCP gives up retransmitting, which is minutes. For all
//! of them the Outputs page shows a healthy output, the group keeps a session advertised
//! and fans audio into it, the rebuild watchdog keeps asking a socket nobody reads, and
//! Home Assistant offers a volume slider for a machine that is asleep.
//!
//! `org.freedesktop.login1` broadcasts exactly the two signals needed, on the **system**
//! bus:
//!
//! * `PrepareForSleep(true)` before suspend/hibernate, `(false)` after resume;
//! * `PrepareForShutdown(true)` before poweroff/reboot.
//!
//! ## The inhibitor is what makes the signal useful
//!
//! logind waits for *delay* inhibitors that already exist when it starts the sequence —
//! taking one after the signal arrives is too late by construction. So the agent holds
//! one continuously and **drops it as the answer** to `PrepareForSleep(true)`: that is
//! the handshake, and dropping the fd is how you say "done, go ahead". A new one is
//! taken after resume. The budget is `InhibitDelayMaxUSec` (5 s by default, verified on
//! a live host), against which this spends one WebSocket frame and a module unload.
//!
//! Everything here is optional in the same way the tray is: no system bus, no logind, or
//! a refused inhibitor each degrade to the behaviour that existed before — the daemon
//! notices the dead socket eventually — and none of them can fail the agent.

use tokio::sync::mpsc;
use zbus::zvariant::OwnedFd;

const LOGIN1: &str = "org.freedesktop.login1";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";
const MANAGER: &str = "org.freedesktop.login1.Manager";

/// What logind told us about this machine's power state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SleepEvent {
    /// About to suspend, hibernate, power off or reboot. The client has until the
    /// inhibitor is released — which happens the moment it returns — to say goodbye.
    Suspending { shutdown: bool },
    /// Back from suspend. Not sent after a shutdown, for the obvious reason.
    Resumed,
}

/// Depth of the logind → client channel. Two is already generous: these arrive at human
/// pace, in strict alternation, and a missed one is not something a deeper queue would
/// fix.
pub const EVENT_DEPTH: usize = 2;

/// Watches logind and reports [`SleepEvent`]s. Returns `false` when this host has
/// nothing to watch (no system bus, no logind), so the caller can say so once.
///
/// The returned task lives for the process; it holds the delay inhibitor between
/// events.
pub async fn spawn(tx: mpsc::Sender<SleepEvent>) -> bool {
    let conn = match zbus::Connection::system().await {
        Ok(conn) => conn,
        Err(e) => {
            tracing::debug!("no system bus ({e}); suspend will not be announced to the add-on");
            return false;
        }
    };
    let proxy = match zbus::Proxy::new(&conn, LOGIN1, LOGIN1_PATH, MANAGER).await {
        Ok(proxy) => proxy,
        Err(e) => {
            tracing::debug!("no logind manager ({e}); suspend will not be announced to the add-on");
            return false;
        }
    };
    // Both streams are opened *before* the first inhibitor is taken, so a suspend that
    // starts in the same millisecond is still seen.
    let sleep = match proxy.receive_signal("PrepareForSleep").await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::debug!(
                "cannot subscribe to PrepareForSleep ({e}); suspend will not be announced"
            );
            return false;
        }
    };
    let shutdown = match proxy.receive_signal("PrepareForShutdown").await {
        Ok(stream) => stream,
        Err(e) => {
            tracing::debug!(
                "cannot subscribe to PrepareForShutdown ({e}); shutdown will not be announced"
            );
            return false;
        }
    };

    tokio::spawn(async move {
        use futures_util::StreamExt as _;
        // Held across the *whole* task, released only while a suspend is in flight.
        // `_conn` and `_proxy` ride along because dropping the connection would take
        // the subscriptions with it.
        let mut inhibitor = inhibit(&proxy).await;
        let mut sleep = sleep;
        let mut shutdown = shutdown;
        loop {
            let (starting, is_shutdown) = tokio::select! {
                Some(msg) = sleep.next() => (msg.body().deserialize::<bool>().unwrap_or(false), false),
                Some(msg) = shutdown.next() => (msg.body().deserialize::<bool>().unwrap_or(false), true),
                else => return, // logind went away; nothing left to watch.
            };
            if starting {
                tracing::info!(
                    "this machine is {}; telling the add-on before it goes",
                    if is_shutdown {
                        "shutting down"
                    } else {
                        "suspending"
                    }
                );
                // Send *then* release: the client end has to have had its turn before
                // logind is allowed to continue. A full channel means the client is not
                // draining, which no amount of waiting here would fix.
                if tx
                    .send(SleepEvent::Suspending {
                        shutdown: is_shutdown,
                    })
                    .await
                    .is_err()
                {
                    return; // client gone
                }
                // Releasing the fd is the "done" answer. Deliberately after the send,
                // and unconditionally — holding it on an error path would delay every
                // suspend on this machine by the full 5 s budget.
                drop(inhibitor.take());
            } else {
                tracing::info!("this machine is back from suspend");
                let _ = tx.send(SleepEvent::Resumed).await;
                // Re-arm for the next cycle. A refusal here is not fatal: the signal
                // still arrives, we just no longer get to hold the machine while
                // answering it.
                inhibitor = inhibit(&proxy).await;
            }
        }
    });
    true
}

/// Takes a `delay` inhibitor for sleep and shutdown, returning the fd that *is* the
/// inhibitor — dropping it releases.
///
/// `delay`, never `block`: this agent has no business preventing a suspend, only
/// finishing a sentence before it. A `block` inhibitor is the kind that makes a laptop
/// refuse to sleep when you close the lid, which is never what an audio helper should
/// do.
async fn inhibit(proxy: &zbus::Proxy<'_>) -> Option<OwnedFd> {
    match proxy
        .call_method(
            "Inhibit",
            &(
                "sleep:shutdown",
                "pwrouter-agent",
                "telling the audio router this host is going away",
                "delay",
            ),
        )
        .await
    {
        Ok(reply) => {
            match reply.body().deserialize::<OwnedFd>() {
                Ok(fd) => Some(fd),
                Err(e) => {
                    tracing::debug!("logind's inhibitor reply was not an fd ({e}); announcing without the delay");
                    None
                }
            }
        }
        Err(e) => {
            // Allowed for a local user by default (polkit), so this normally means a
            // locked-down or containerised session.
            tracing::debug!("could not take a delay inhibitor ({e}); announcing without it");
            None
        }
    }
}
