//! Host capability / weak-system assessment.
//!
//! Answers a single practical question: *is this machine strong enough for
//! realtime multi-room audio?* On small ARM boards (our baseline is a
//! Raspberry Pi 4 — 4× Cortex-A72 @1.5 GHz) we routinely see RT-audio
//! scheduling problems; on beefy x86 hosts we don't. This module reads a few
//! cheap `/proc` files plus the compiled-in target arch, probes whether the
//! process can actually obtain realtime scheduling, and boils it down to a
//! coarse `verdict` the diagnostics UI can surface.
//!
//! Everything here is best-effort: a missing/unreadable `/proc` file degrades
//! gracefully to a sane default rather than failing. The assessment is
//! effectively static for the lifetime of the process, but `assess()` is cheap
//! enough to call per request.

use serde::Serialize;

/// A snapshot of the host's audio-relevant capabilities plus a coarse verdict.
#[derive(Debug, Clone, Serialize)]
pub struct HostAssessment {
    /// Human-readable CPU model (e.g. "Cortex-A72" / Pi "Model" line, or the
    /// x86 "model name"). Falls back to the arch string if nothing is found.
    pub cpu_model: String,
    /// Number of logical CPUs as reported by the OS.
    pub cores: usize,
    /// Compiled-in target architecture (`std::env::consts::ARCH`), e.g.
    /// "aarch64", "x86_64", "arm".
    pub arch: String,
    /// Total system RAM in mebibytes (from `/proc/meminfo` `MemTotal`).
    pub mem_total_mb: u64,
    /// Whether the process can obtain realtime (`SCHED_FIFO`) scheduling — i.e.
    /// it holds `CAP_SYS_NICE`. Realtime audio relays depend on this.
    pub rt_available: bool,
    /// Coarse verdict: `"adequate"` | `"marginal"` | `"underpowered"`.
    pub verdict: String,
    /// Short human-readable explanation of the verdict.
    pub note: String,
}

/// Read the number of logical CPUs. Uses `std::thread::available_parallelism`
/// which reflects the OS-visible core count (and honours cgroup limits on
/// recent kernels); falls back to counting `processor` lines in
/// `/proc/cpuinfo`, then to 1.
fn read_cores() -> usize {
    if let Ok(n) = std::thread::available_parallelism() {
        return n.get();
    }
    if let Ok(txt) = std::fs::read_to_string("/proc/cpuinfo") {
        let n = txt.lines().filter(|l| l.starts_with("processor")).count();
        if n > 0 {
            return n;
        }
    }
    1
}

/// Best-effort CPU model name from `/proc/cpuinfo`.
///
/// x86 exposes `model name`; ARM boards typically expose only `Model` (the
/// board, e.g. "Raspberry Pi 4 Model B Rev 1.4") and/or `model name` /
/// `CPU part`. We prefer, in order: `model name`, `Model`, `Hardware`.
fn read_cpu_model(arch: &str) -> String {
    if let Ok(txt) = std::fs::read_to_string("/proc/cpuinfo") {
        for key in ["model name", "Model", "Hardware"] {
            for line in txt.lines() {
                if let Some((k, v)) = line.split_once(':') {
                    if k.trim() == key {
                        let v = v.trim();
                        if !v.is_empty() {
                            return v.to_string();
                        }
                    }
                }
            }
        }
    }
    // Nothing useful found — fall back to the arch so the field is never empty.
    format!("unknown ({arch})")
}

/// Total RAM in MiB from `/proc/meminfo` (`MemTotal:` is in kB). Returns 0 if
/// the file is unreadable or the field is missing.
fn read_mem_total_mb() -> u64 {
    if let Ok(txt) = std::fs::read_to_string("/proc/meminfo") {
        for line in txt.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                // Format: "MemTotal:       16311764 kB"
                if let Some(kb) = rest.split_whitespace().next() {
                    if let Ok(kb) = kb.parse::<u64>() {
                        return kb / 1024;
                    }
                }
            }
        }
    }
    0
}

/// CAP_SYS_NICE is capability bit 23. `/proc/self/status` exposes the effective
/// capability set as a 64-bit hex mask in the `CapEff:` line.
const CAP_SYS_NICE: u32 = 23;

/// Check `CAP_SYS_NICE` in the effective capability set via
/// `/proc/self/status` `CapEff:`. Returns `None` if it can't be determined.
fn cap_sys_nice_from_status() -> Option<bool> {
    let txt = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in txt.lines() {
        if let Some(rest) = line.strip_prefix("CapEff:") {
            let mask = u64::from_str_radix(rest.trim(), 16).ok()?;
            return Some((mask >> CAP_SYS_NICE) & 1 == 1);
        }
    }
    None
}

/// Best-effort realtime-scheduling probe: spawn a throwaway thread and try to
/// switch it to `SCHED_FIFO` at the lowest RT priority. Success means the
/// process really can obtain RT scheduling (it holds `CAP_SYS_NICE` and the
/// policy is permitted); we immediately restore the thread to `SCHED_OTHER` and
/// let it exit. This mirrors what the audio relay threads do at startup.
#[cfg(target_os = "linux")]
fn probe_rt_scheduling() -> bool {
    let handle = std::thread::spawn(|| -> bool {
        // SAFETY: sched_setscheduler on the current thread (pid 0) with a valid,
        // zero-initialised sched_param. No aliasing, no ownership transfer. The
        // thread is exclusively ours and exits right after.
        unsafe {
            let mut param: libc::sched_param = std::mem::zeroed();
            param.sched_priority = 1; // lowest valid SCHED_FIFO priority
            let ok = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) == 0;
            if ok {
                // Restore normal scheduling before the thread winds down.
                let mut normal: libc::sched_param = std::mem::zeroed();
                normal.sched_priority = 0;
                let _ = libc::sched_setscheduler(0, libc::SCHED_OTHER, &normal);
            }
            ok
        }
    });
    // If the probe thread panicked for any reason, treat RT as unavailable.
    handle.join().unwrap_or(false)
}

#[cfg(not(target_os = "linux"))]
fn probe_rt_scheduling() -> bool {
    false
}

/// Determine whether realtime scheduling is available, preferring the live
/// `sched_setscheduler` probe (the ground truth) and falling back to the
/// `CAP_SYS_NICE` capability bit if the probe is inconclusive.
fn detect_rt_available() -> bool {
    if probe_rt_scheduling() {
        return true;
    }
    cap_sys_nice_from_status().unwrap_or(false)
}

/// Verdict heuristic, calibrated against the Raspberry Pi 4 baseline
/// (4× Cortex-A72 @1.5 GHz — the weakest board we support for realtime
/// multi-room audio).
///
/// Rules (first match wins):
/// - **underpowered**: fewer than 4 cores, an unsupported architecture, or no
///   realtime scheduling. Any of these means RT multi-room audio will very
///   likely stutter or fail outright.
/// - **adequate**: a comfortably headroom-y host — x86_64 with ≥4 cores, or any
///   supported arch with ≥6 cores (a Pi 4 has exactly 4, so ≥6 clears it
///   decisively) — and RT scheduling available.
/// - **marginal**: everything in between — a Pi-4-class board (4–5 cores,
///   supported arch) with RT available. Works, but with little headroom.
fn compute_verdict(cores: usize, arch: &str, rt_available: bool) -> (String, String) {
    let arch_supported = matches!(arch, "aarch64" | "x86_64");

    if !rt_available {
        return (
            "underpowered".to_string(),
            "Realtime scheduling is unavailable (missing CAP_SYS_NICE); \
             multi-room audio relays cannot get RT priority and will be prone \
             to dropouts."
                .to_string(),
        );
    }
    if cores < 4 {
        return (
            "underpowered".to_string(),
            format!(
                "Only {cores} CPU core(s) — below the 4-core Raspberry Pi 4 \
                 baseline for realtime multi-room audio."
            ),
        );
    }
    if !arch_supported {
        return (
            "underpowered".to_string(),
            format!(
                "Unsupported CPU architecture '{arch}'; only aarch64 and x86_64 \
                 are validated for realtime multi-room audio."
            ),
        );
    }
    // From here: supported arch, >=4 cores, RT available.
    if arch == "x86_64" || cores >= 6 {
        return (
            "adequate".to_string(),
            format!(
                "{cores} cores on {arch} with realtime scheduling — comfortable \
                 headroom for realtime multi-room audio."
            ),
        );
    }
    (
        "marginal".to_string(),
        format!(
            "{cores} cores on {arch} with realtime scheduling — meets the \
             Raspberry Pi 4 baseline but with little headroom; heavy \
             multi-room use may occasionally struggle."
        ),
    )
}

/// Assess the current host. Cheap enough to call per request.
pub fn assess() -> HostAssessment {
    let arch = std::env::consts::ARCH.to_string();
    let cores = read_cores();
    let cpu_model = read_cpu_model(&arch);
    let mem_total_mb = read_mem_total_mb();
    let rt_available = detect_rt_available();
    let (verdict, note) = compute_verdict(cores, &arch, rt_available);

    HostAssessment { cpu_model, cores, arch, mem_total_mb, rt_available, verdict, note }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assess_returns_sane_values() {
        let a = assess();
        assert!(a.cores >= 1, "cores should be at least 1, got {}", a.cores);
        assert!(!a.verdict.is_empty(), "verdict should be non-empty");
        assert!(matches!(a.verdict.as_str(), "adequate" | "marginal" | "underpowered"), "unexpected verdict: {}", a.verdict);
        assert!(!a.note.is_empty(), "note should be non-empty");
        assert!(!a.arch.is_empty(), "arch should be non-empty");
        assert!(!a.cpu_model.is_empty(), "cpu_model should be non-empty");
    }

    #[test]
    fn verdict_underpowered_without_rt() {
        let (v, _) = compute_verdict(8, "x86_64", false);
        assert_eq!(v, "underpowered");
    }

    #[test]
    fn verdict_underpowered_few_cores() {
        let (v, _) = compute_verdict(2, "aarch64", true);
        assert_eq!(v, "underpowered");
    }

    #[test]
    fn verdict_marginal_pi4_class() {
        let (v, _) = compute_verdict(4, "aarch64", true);
        assert_eq!(v, "marginal");
    }

    #[test]
    fn verdict_adequate_x86() {
        let (v, _) = compute_verdict(4, "x86_64", true);
        assert_eq!(v, "adequate");
    }

    #[test]
    fn verdict_adequate_many_arm_cores() {
        let (v, _) = compute_verdict(8, "aarch64", true);
        assert_eq!(v, "adequate");
    }
}
