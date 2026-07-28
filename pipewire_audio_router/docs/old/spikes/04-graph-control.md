# Spike 4 result: programmatic PipeWire graph control — REVISED after a deeper look across languages

Per PLAN.md Section 7 spike #4 and Section 4's bridge daemon design. This
spike ran in two rounds: the first concluded "shell out from Python, it's
fast enough"; a follow-up investigation (prompted by a good question —
does the choice of implementation language change this?) found a
genuinely better option. **The revised conclusion supersedes the
original one** — read the "Round 2" section for the current
recommendation.

## Round 1: Python, shelling out vs. native bindings

### Finding: no mature native Python binding to libpipewire's graph API exists

Checked the two realistic candidates:

- **`pipewire-python`** (PyPI, v0.2.3, installs cleanly) — its own
  docstring states outright: *"controls `pipewire` via terminal, creating
  shell commands and executing them as required."* Confirmed by reading
  `link.py`: every operation goes through `_execute_shell_command`,
  which shells out to `pw-link`/`pw-cli`/`pw-play`/`pw-record` and parses
  their text output into convenience dataclasses. Not a binding — the
  same approach this project already uses manually, in a nicer wrapper.
- **GObject-Introspection bindings** — no `gir1.2-pipewire*` package
  exists on Ubuntu 26.04. Unlike GStreamer, PipeWire's core graph API has
  no GI typelib, so no `python3-gi`-based binding is possible without
  writing one from scratch. `python3-libpulse` wraps the PulseAudio
  *compatibility* layer, not the native graph API this project needs.

The only way to get a genuine native Python binding would be hand-written
`ctypes`/`cffi` bindings against `libpipewire-0.3-dev` — real effort, no
package to build on.

### Is shelling out from Python fast enough? Measured, not assumed.

`tests/test_spike04_graph_control_latency.sh`, 20 iterations each, in the
real container:

| Operation | Avg latency |
|---|---|
| `pw-dump` (full graph state read) | **8ms** |
| `pw-link` create 2 ports + `pw-link -d` destroy 2 ports (one full route toggle) | **16ms** |

Both are far under any UI-responsiveness threshold (~100ms is where a
delay starts being perceptible). This measurement is still accurate and
still worth knowing — it's the baseline the Round 2 finding beats.

## Round 2: does the implementation language change the answer?

Prompted by the observation that the eventual bridge daemon is a
long-running web-API process anyway (not a short-lived script), so a
compiled language with a real dependency ecosystem could plausibly do
better than Python+subprocess. Investigated Rust, Go, and C++
specifically, empirically rather than by reputation.

### Rust: a real, official, complete binding exists — and it's dramatically faster

**`pipewire-rs`** (crates.io: `pipewire`, currently v0.10) is maintained
under `gitlab.freedesktop.org/pipewire` — i.e. upstream PipeWire's own
org, not a third-party project. Cloned it and read `examples/pw-mon.rs`
(a full reimplementation of the `pw-mon` CLI tool: `Node`, `Port`,
`Link`, `Device`, `Client`, `Module`, `Factory`, `Metadata` types, live
registry change events) and `examples/create-delete-remote-objects.rs`
(creates and destroys a real `Link` object via the same link-factory
mechanism `pw-link` itself uses). This is a complete graph-control API,
not a playback-only or read-only wrapper.

Built a proof-of-concept (`tests/spike04_rust_poc/`) adapting the
official example: connects to a real PipeWire core, discovers two test
nodes and the link factory via the registry, then creates and destroys a
link between them 20 times, timing each round trip natively (no
subprocess).

**It compiles cleanly against Ubuntu 26.04's own `libpipewire-0.3-dev`
(1.6.2) with zero patches** (`cargo add pipewire@0.10; cargo build
--release` — 15 seconds, no version pinning drama), and runs correctly
against a real running PipeWire+WirePlumber instance:

```
source_node_id=49 sink_node_id=54 link_factory=link-factory
out_port=51 in_port=53
PASS: native pipewire-rs create+destroy link round trip avg over 20 iterations: 0.07ms
```

**0.07ms, vs. 16ms for the shell-based version — roughly 230x faster.**
This isn't a fluke or measurement noise: it's the expected consequence of
architecture. `pw-link` (and every other CLI invocation) pays fork/exec
cost *and* opens a brand-new client connection to the PipeWire daemon,
negotiates, does one operation, and disconnects — every single time. The
native binding opens **one persistent connection** for the daemon's
entire lifetime and just sends protocol messages directly over the
already-established socket. For a long-running bridge daemon (exactly
what this project needs), that's the architecturally correct shape
regardless of the latency number — it also means the daemon can hold
**live registry state** via event listeners instead of re-running
`pw-dump` on demand, which is a real design simplification, not just a
speed win.

### Go: no viable existing binding for this use case

Checked the two real candidates on pkg.go.dev:

- **`bnema/purego-pipewire`** — uses `purego` (calls C shared libraries
  via `dlopen`/`dlsym`, no `cgo` needed — a genuinely nice property).
  But its own README says outright: *"This is early stage... the
  handwritten API is still focused on runtime lifecycle and float32
  playback."* No registry, node, port, or link API at all — it's for
  writing an audio-playback app, not a graph-routing controller.
- **`pipewire-monitor-go`** (and forks `xaionaro-go/pipewire-monitor-go`,
  `go.mwg.gamfam.au/go-pipewire-monitor`) — confirmed by reading its
  README: it's a wrapper around `pw-dump --monitor --no-colors`. Same
  category as Python's `pipewire-python` — a subprocess wrapper, and
  read-only (monitoring) at that, no create/destroy capability.

Building a real Go binding would mean hand-writing `cgo` bindings against
`libpipewire-0.3-dev` from scratch — comparable effort to C++ below, with
the added complication that `cgo` requires a real C cross-compiler for
the target architecture at build time, undermining one of Go's usual
selling points (trivial `GOARCH=arm64 go build` cross-compilation) for
this specific project's amd64-dev/arm64-target split.

### C++: full native access by definition — but none of Rust's ergonomics for free

PipeWire's own headers are plain C, directly usable from C++ with zero
wrapper (confirmed: a 6-line C++ program calling `pw_init()`/
`pw_get_library_version()` compiled and linked against
`libpipewire-0.3-dev` via plain `pkg-config` + `g++` in seconds, no
issues). This means C++ has access to the *exact same complete API*
`pipewire-rs` wraps — in principle at least as capable, since Rust's
binding is built on top of this same C API.

The catch: all of `pipewire-rs`'s ergonomics — `Node`/`Port`/`Link` as
owned Rust types with RAII cleanup, the builder-pattern listener API,
`do_roundtrip()`-style sync helpers — are hand-maintained work upstream
already did once. A C++ implementation starts from the raw C API: manual
callback registration via function pointers and `void*` user-data
casting, manual object lifetime tracking, no borrow-checker safety net
against use-after-free on a freed proxy. Fully capable, but the
implementation cost to reach parity with what Rust already provides
falls entirely on this project, not on an upstream maintainer.

## Revised conclusion for the bridge daemon (Section 4/5.5)

The honest tradeoff, not a foregone conclusion:

- **Python + subprocess** (the Round 1 answer): simplest to iterate on,
  zero new language for this project, 16ms is genuinely fine for a
  human clicking a button. Loses the "live registry state via event
  listeners" architectural simplification, and is ~230x slower per
  operation (irrelevant at v1's actual change frequency, but real).
- **Rust + `pipewire-rs`**: the technically strongest option found —
  official upstream binding, complete graph-control API, compiles
  cleanly against the exact production base image, dramatically faster,
  and Rust's HTTP/web ecosystem (`axum`, `actix-web`, `serde`, static-file
  serving) is mature enough to plausibly write the *entire* bridge daemon
  — PipeWire control, REST/WS API, and static UI hosting — as one
  compiled binary with a real dependency manager (`cargo`), matching what
  you described wanting. Cost: this project would be learning/using Rust
  for the whole daemon, not just the PipeWire layer, and `pipewire-rs`
  is still a `0.x` crate (pre-1.0, API can still shift between
  releases).
- **Go**: not a viable path for the PipeWire layer specifically — no
  existing binding does what's needed, and hand-rolling `cgo` bindings
  would cost real effort while also giving up Go's easy cross-compilation
  story. Not recommended unless there's a strong independent reason to
  prefer Go's ecosystem for the web-API half enough to absorb that cost.
- **C++**: fully capable, zero ecosystem-availability risk (it's *the*
  native language), but reaching parity with what Rust gets for free
  means re-implementing real chunks of what `pipewire-rs` already did.
  Reasonable if this project already had a C++ codebase to extend; not a
  reason to start one from scratch here.

**This is a real decision, not something to settle unilaterally in this
document** — it trades off implementation language for the whole bridge
daemon (not just the PipeWire-control sliver), which depends on factors
only you can weigh (which language you actually want to write and
maintain this in, going forward). Flagging it back rather than picking.

## Files added

- `tests/test_spike04_graph_control_latency.sh` — Round 1's shell-based
  latency measurement.
- `tests/spike04_rust_poc/` (`Dockerfile`, `main.rs`) +
  `tests/test_spike04_rust_native_binding.sh` — Round 2's native
  `pipewire-rs` proof-of-concept and its runner.

## Net effect on PLAN.md Section 7 risk table

Spike #4 now carries an open follow-up: which language the bridge daemon
is actually written in. Mechanically resolved either way (both paths
work); the choice is a project decision, flagged for you rather than
assumed.
