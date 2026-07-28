//! Phase 0 spike for docs/multi-source-inputs-plan.md: can two AirPlay (RAOP)
//! receivers run **concurrently in one process** on distinct RTSP ports with
//! distinct mDNS names? This is the only real unknown for AirPlay-multi; if it
//! passes, the plan's per-instance port allocator is viable, and if it ever
//! fails we can cap AirPlay sources at 0..1.
//!
//! Run:  cargo run --example multi_raop_spike
//! Optionally, in another shell: avahi-browse -rt _raop._tcp
//! (should list SpikeRaopA and SpikeRaopB while this runs).

use shairplay::{AudioFormat, AudioHandler, AudioSession, RaopServer};
use std::sync::Arc;

/// Minimal no-op handler — the spike only tests bind + advertise, not audio.
struct Noop;
impl AudioHandler for Noop {
    fn audio_init(&self, _format: AudioFormat) -> Box<dyn AudioSession> {
        Box::new(NoopSession)
    }
}
struct NoopSession;
impl AudioSession for NoopSession {
    fn audio_process(&mut self, _samples: &[f32]) {}
}

#[tokio::main]
async fn main() {
    let instances = [("SpikeRaopA", 5000u16), ("SpikeRaopB", 5001u16)];
    let mut servers = Vec::new();
    let mut started = 0;

    for (i, (name, port)) in instances.iter().enumerate() {
        // Distinct locally-administered hwaddr per instance (drives the mDNS
        // instance id + AirPlay deviceid — must differ so both advertise).
        let hwaddr = vec![0x02, 0x00, 0x00, 0x00, 0x00, i as u8];
        let handler: Arc<dyn AudioHandler> = Arc::new(Noop);
        match RaopServer::builder().name(*name).hwaddr(hwaddr).port(*port).build(handler) {
            Ok(mut server) => match server.start().await {
                Ok(()) => {
                    println!("OK   started '{name}' on RTSP :{port}");
                    servers.push(server);
                    started += 1;
                }
                Err(e) => println!("FAIL '{name}' on :{port} start error: {e}"),
            },
            Err(e) => println!("FAIL '{name}' build error: {e}"),
        }
    }

    println!("--- {started}/2 receivers up; holding 3s (browse _raop._tcp to confirm mDNS) ---");
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    for mut server in servers {
        server.stop().await;
    }

    if started == 2 {
        println!("RESULT: PASS — two concurrent RAOP receivers on distinct ports/names");
    } else {
        println!("RESULT: FAIL — only {started}/2 started");
        std::process::exit(1);
    }
}
