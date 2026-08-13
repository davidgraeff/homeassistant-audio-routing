# Harmonising the daemon API

A review of all **85 registered routes** against their two real consumers — the web UI
(`pipewire_audio_router/frontend/src/lib/api.ts`, 72 wrappers) and the Home Assistant
integration (`custom_components/pipewire_audio_router/api.py`) — looking for endpoints
that can be unified, merged or simplified.

The brief was explicit about the trade to accept: **the server may get more complex if
the consumers get simpler.** That is the right direction here, and this document argues
it from the consumers' own code rather than from taste — every proposal below is
motivated by a place where a caller is currently forced to know something the daemon
already knows.

**Status: §1–§5 are implemented** (2026-08-13), with no compatibility layer —
there are no external consumers yet and both in-repo ones were changed in the same step,
which is the policy this was done under: consistency over compatibility, no aliases, and
no documentation of moved endpoints. `docs/api-reference.md` describes the surface as it is
now; what each section below records is the *reasoning*, and where it says "proposal" read
"what was done". §7's list is what remains.

The route table went from **85 to 72** routes (77 after the harmonisation, then five spike harnesses deleted), the four status sockets became one, and
`{ok, message}` is gone from every response.

The ranking at the end is by *consumer win per unit of churn*; §7 lists what should be
left alone and why, because a harmonisation pass that touches everything is how a working
API gets broken for a tidier route table.

---

## 1. The finding that pays for the whole exercise: per-kind volume endpoints

Three endpoints do the same thing to three kinds of output, and **do not agree on a
scale**:

| Endpoint | Address | Scale | Applied by |
|---|---|---|---|
| `PUT /api/sendspin/volume` | `node_name` in body | `0–100` integer | sendspin protocol, in-band |
| `PUT /api/ap2/volume` | `node_name` in body | `0.0–1.0` | RTSP `SET_PARAMETER` |
| `PUT /api/pwsink/volume` | `node_name` in body | `0.0–1.0` **cubic** | the host's agent |

`mute` is the same three-way split. So is nothing else: the *timing* knob was already
unified onto `PUT /api/outputs/{node_name}/latency`, which dispatches on
`OutputKind::of(&node_name)` — the precedent this proposal follows exists in-tree.

**Both consumers therefore carry a dispatcher.** The web UI has a module for it whose
doc comment is a bug post-mortem ([`lib/outputs/level.ts`](../pipewire_audio_router/frontend/src/lib/outputs/level.ts)):

> That fallback is what made pw-sink volume and mute silently do nothing: a
> `pwsink-dev-*` name went to `PUT /api/sendspin/volume`, which happily *stored* it as an
> intent for a sendspin device that will never connect and answered `ok: true`. The click
> therefore looked accepted, and a moment later the pushed matrix frame […] put the old
> value back. That is exactly the "mute flips back on its own" symptom.

That bug is a *property of the API shape*: `set_sendspin_volume` takes any string, stores
it as a desired value, and answers `200 {ok: true}` — it cannot tell an out-of-kind name
from a device that is merely offline, because both are "not connected". The integration
dispatches by prefix too, in two places (`media_player.py`, once per output entity and
again per group entity), each branch converting the scale by hand:

```python
if self._is_ap2:      await client.async_set_ap2_volume(self.node_name, volume)
elif self._is_sendspin: await client.async_set_sendspin_volume(self.node_name, round(volume * 100))
elif self._is_pwsink:  await client.async_set_pwsink_volume(self.node_name, volume)
```

### Proposal

```
PUT /api/outputs/{node_name}/volume   { "volume": 0.42 }     # 0.0–1.0, one scale
PUT /api/outputs/{node_name}/mute     { "muted": true }
```

- **One scale on the wire: `0.0`–`1.0`**, matching HA's `volume_level` — so the
  integration stops converting entirely, and the UI (which works in whole percent behind
  its sliders) converts once, in one place, instead of differently per backend. The daemon
  owns the per-kind conversion; it already has `ap2_level()` and `host_level()` for exactly
  this in the alignment code.
- **An unknown or out-of-kind name is a `404`/`400`, not a stored intent.** This is the
  part that turns the class of bug above into an impossible one.
- The response should say *where the value went* — applied in-band, stored for reconnect,
  or refused — which is information both consumers currently reconstruct from prose.

**Server cost:** one dispatcher plus a scale conversion per kind (~80 lines).
**Consumer win:** `level.ts` disappears (~60 lines and a documented footgun); the
integration loses two prefix ladders, three client methods and all scale arithmetic; a
fourth output kind needs no consumer change at all.

**Done, without aliases.** `PUT /api/outputs/{node}/volume` and `/mute` dispatch on
`OutputKind::of(&node_name)` in `api/level.rs`; the three per-kind pairs are gone. The
consumer side came out as expected: `lib/outputs/level.ts` lost its dispatcher (99 → 58
lines, and what is left is the *read* side), and the integration's `media_player.py` lost
both prefix ladders and every scale conversion — `async_set_output_volume(node, volume)`
with HA's own 0.0–1.0 straight through. Its test that asserted `("sendspin-dev-bath", 40)`
now asserts `("sendspin-dev-bath", 0.4)`, which is the whole point in one line.

Two more endpoints joined them on the way, for the same reason:

* **`PUT /api/outputs/{node}/delay`** replaces `PUT /api/sendspin/delay` *and*
  `…/latency`. The polarity stays visible where it belongs — in the response and in
  `GET /api/outputs` — rather than in the URL: sendspin's knob is an *advance* and costs
  that speaker a reconnect, AP2's and pw-sink's are delays applied live.
* **`POST /api/outputs/{node}/resync`** replaces `POST /api/sendspin/clear` and
  `POST /api/ap2/resync`. One intent ("this output is being sent audio and is not
  rendering it — do the cheapest thing that fixes it"), three mechanisms, and a pw-sink
  host answers 400 because it genuinely has none.

---

## 2. `node_name` in the body where every neighbour puts it in the path

13 request bodies carry the `node_name` of the thing they act on, and four more carry a
client `key`. The same resource is addressed both ways depending on which endpoint you
use:

| In the path | In the body |
|---|---|
| `PUT /api/outputs/{node_name}/latency` | `PUT /api/sendspin/delay` `{node_name, delay_ms}` |
| `PUT /api/outputs/{node_name}/name` | `PUT /api/sendspin/volume` `{node_name, volume}` |
| `DELETE /api/align/measure/split/{node_name}` | `POST /api/align/measure/split` `{node_name, level}` |
| `DELETE /api/routing/entity/{node_name}` | `POST /api/sendspin/clear` `{node_name}` |
| | `POST /api/ap2/resync` `{node_name}` |
| | `POST /api/align/channel` `{node_name, channels}` |
| | `POST /api/align/measure/arrival` `{node_name, level}` |

The cost is small but constant: a consumer cannot build a generic "act on this output"
helper, URL-based logging and rate limiting cannot see the subject, and the same identity
is spelled `node_name`, `id`, `key`, `target`, `targets`, `members` and `outputs`
depending on where you are.

### Proposal

New paths put the subject in the path. Concretely: `/api/outputs/{node}/volume`,
`/mute`, `/delay`, `/resync`, `/clear`; `/api/align/members/{node}/channel`;
`/api/align/measure/split/{node}` for both write and clear (it already exists for
`DELETE`). Bodies then carry only *values*.

One case must **not** move: `POST /api/announce` and `POST /api/duck` take `targets: []`
*or* `announcement_group` — they are genuinely multi-subject, and a body is the right
place for a set.

---

## 3. One vocabulary for "which outputs"

The same concept has five names, and one request accepts two of them at once:

| Endpoint | Field | Means |
|---|---|---|
| `POST /api/announce`, `POST /api/duck` | `targets` | output node names |
| `POST /api/groups/music` | `members` | output node names |
| `POST /api/groups/announcement` | `targets` | output node names |
| `POST /api/align/start` | `outputs` **and** `sources` | outputs to hold, *or* a source set to resolve a group from |
| `POST /api/align/audible` | `audible` | output node names |

`AlignStartRequest` is the sharp one: two mutually exclusive fields, both optional, whose
interaction the doc comment has to explain at length. It is a historical seam — the panel
used to live on source cards — and the source-set form is now the legacy path.

### Proposal

Standardise on **`outputs`** for a set of output node names in every new body, and keep
`targets`/`members` as accepted aliases where they already ship.

`/api/align/start`'s `sources` form is **gone**: the web UI only ever called the `outputs`
form and the HA integration does not touch `/api/align` at all, so one of the two fields
had no consumer. With it goes the "either/or" the doc comment had to explain, and
`AlignManager::start` (the source-set entry point) with it.

---

## 4. Response shapes: three conventions, and one that reports failure as success

| Shape | Count | Notes |
|---|---|---|
| `(StatusCode, Json<OutputOpResponse{ok, message}>)` | 36 | the envelope, status *and* `ok` both carry success |
| `Result<Json<T>, (StatusCode, Json<Refusal>)>` | 11 | typed error, HTTP status carries success |
| bare `Json<T>` | ~25 | reads, plus a few writes that cannot fail |

Two concrete problems, not stylistic:

- **`200 {ok: false}` exists.** `PUT /api/outputs/{n}/latency` on a sendspin name returns
  `400`; `POST /api/sendspin/clear` on a disconnected device returns
  `(StatusCode::OK, ok: reached)` — a plain `200` whose body says it did not happen; and
  `PUT /api/pwsink/volume` with no agent returns `503 {ok:false}`. A consumer must
  therefore check *both* the status and the body on every call, and both consumers do —
  `run()` in the UI's `toast.ts` inspects `ok` after the fetch resolved, and the
  integration raises on either. The rule is unwritten and inconsistently applied.
- **`{ok, message}` is not machine-readable.** `message` is the only carrier of *why*, so
  a consumer that wants to react differently to "no agent connected" than to "unknown
  output" has to match prose. The alignment subsystem already has the better answer in
  `Refusal` (a `kind` plus a sentence); the older half of the API predates it.

### Done

One rule, written down in `api/error.rs` and in the reference's own "How a call answers":
**the status carries success; a failure body is `{kind, message}`; a success body is the
affected resource or just the message.** `OutputOpResponse`, `LinkOpResponse` and
`CreateLinkResponse` are deleted; `DuckResponse`, `SpikeStartResponse` and
`AgAnnounceResponse` kept their data and lost their `ok`.

`ErrorKind` is deliberately five values about the *caller's* situation, not the daemon's
internals — and the one that did the real work is **`conflict`**, which is what every
`200 {ok:false}` turned into: "the request is fine, the target is not in a state where it
applies." `ok_if(happened, message)` is that shape as a helper, because every
tell-a-device-something handler has it.

Where the line falls, and it is not always obvious: an **announcement's rejection stays a
200**. The arbiter decided; the request was carried out. `admission` (`playing` / `queued` /
`rejected`) is the machine-readable outcome, which is what the `ok` flag beside it had been
reaching for. Same for a duck release of a hold that was already gone — the caller wanted it
not ducking, and it is not ducking.

The alignment subsystem keeps its own richer `Refusal` vocabulary (`mic_lost`,
`estimator`, plus the member to blame), because those name states a user can act on. Same
envelope, so a consumer has one shape.

Consumers, which is where the win is: the web UI's `run()` no longer inspects a flag —
**a throw is the only failure** — and `OpResponse` is `{message}`; the integration got one
`_raise_for_error(resp, what)` helper that carries the daemon's sentence *and* keeps
`kind` on the exception, replacing seven hand-written `if not body.get("ok")` checks.

---

## 5. Four sockets that could be one, and two that must stay separate

| Socket | Pushes | Consumers |
|---|---|---|
| `/api/routing/ws` | multiplexed: `matrix`, `now_playing`, `meters`, `agents`, … | UI + integration |
| `/api/align/ws` | session state | UI |
| `/api/align/measure/ws` | run state | UI |
| `/api/align/equivalence/ws` | experiment state | UI |
| `/api/align/mic/ws` | **binary ingest, client → server** | UI |
| `/api/agent/ws` | the receiver-agent protocol | agents only |

`/api/routing/ws` is **already a topic-multiplexed event bus** — the integration's client
names the frame types it consumes and silently skips `meters` — so the pattern is
established and the three alignment sockets are the exception rather than the rule.

Each socket is not just a route: it is a reconnect/backoff/heartbeat implementation in
every consumer, and `align/status_ws.rs` exists because a second copy of that handling
"is how one of them ends up leaking a `watch` subscription".

### Done — and subscription is by *message*, not by URL

`GET /api/events`, with `{"op":"subscribe","topics":[…]}` / `unsubscribe` on the socket
itself. That was the sharpening this needed: a URL parameter fixes a client's topics for
the life of the connection, whereas a page that navigates away can now unsubscribe and the
daemon stops that work while the connection stays for the next page.

`meters` is where that pays twice: subscribing to it is what **arms** per-source peak
metering and the PipeWire profiler, and the last unsubscribe disarms them. The accounting
used to be tied to the socket's lifetime, so an alignment wizard paid for a 4 Hz meter tick
it never rendered.

Consumers: the web UI has one client (`lib/events.ts`, ~190 lines) owning the connection,
the reconnect and per-topic refcounting, and the four stores became four `onTopic(…)`
calls; the HA integration subscribes to exactly `matrix` and `now_playing`, so it is no
longer *sent* the 4 Hz meters frame it used to skip four times a second.

Two rules the daemon guarantees, both because the old sockets did: **subscribing sends
that topic's current state at once** (a subscription is the successor of a connect), and
**frames are deduplicated per topic**.

**Not merged:** `/api/align/mic/ws` (binary, client→server, one socket at a time, its own
hello/ready handshake) and `/api/agent/ws` (a device protocol with its own auth, not a UI
feed). Merging either buys nothing and complicates both.

---

## 6. The constraint that shapes migration: two consumers on different clocks

The add-on and the HA integration are **installed and updated separately**. A user can
run a new add-on with an old integration for weeks. So:

- every rename needs an **alias window**, and the aliases have to be listed with the
  release that may drop them;
*(Left as written, because it is the constraint that will apply the moment there is an
external consumer — it just did not apply to this pass.)*

- the daemon's `GET /api/status` already carries `version` — the add-on **build** string
  (`0.3.20260812230112`), which the integration reads for its service device. That is not a
  capability signal: deriving "does this build have `/api/outputs/{n}/volume`" from a
  timestamp means hard-coding dates in the integration. Add an explicit **`api_generation`**
  (or a feature list) beside it, so a consumer can pick the new path when offered and the
  old one otherwise — the one change that has to land *before* any rename;
- the web UI is served *by the daemon*, so it is always in lockstep and needs no window.
  Its share of the churn is free.

**Which draws a useful line through the surface.** The integration touches roughly the
outputs, sources, routing, now-playing, groups, duck, announce, agents, settings and status
families. It touches **none** of `/api/align/*` (verified: no match for `api/align` anywhere
under `custom_components/`). So the whole alignment subsystem — 24 routes, the four sockets
of §5, and the endpoints §2 and §3 would re-address — has exactly **one** consumer, and it
ships with the daemon. Those can be changed outright, with no alias window and no
generation flag. The compatibility work is confined to the dozen or so endpoints the
integration actually calls, which happens to include §1's volume family.

This is also the argument against a big-bang v2 prefix: two surfaces to maintain, and the
integration would have to speak both anyway.

---

## 7. Leave alone, deliberately

| Thing | Why |
|---|---|
| `/api/align/*`'s many small POSTs (`audible`, `volume`, `select`, `channel`, `still-here`) | They read as sprawl but they are *steps of one protocol* with different idle-timeout semantics — `still-here` postpones teardown, a status poll deliberately does not. A single `PATCH /api/align/session` would hide exactly that. (Full disclosure: `/api/align/channel` is mine, added yesterday, and a `PATCH` would have been the tidier choice — the timeout asymmetry is why it is not.) |
| `POST /api/outputs/{n}/adopt` / `ignore` / `unpair` | RPC-shaped, but they are a state machine with a side effect (pairing mints a token). `PUT …/state` would make the token minting invisible. |
| `/api/spike/*` | **Gone** (2026-08-13). They were development harnesses in a shipped route table, documented as "not a supported interface" — five rows a reader had to skip and five request types nobody validated. The recipe for the next experiment, including what exposing one over HTTP costs, is now the module doc of `bridge-daemon/src/spike/mod.rs`, which is all that is left of them. |
| `/api/links` vs `/api/routing/link` | Different layers (raw port link vs persisted intent). Worth renaming `/api/links` → `/api/pw/links` to stop it reading as the routing one. |
| `/health` | Plain text, no envelope, on purpose. |
| `/api/sendspin/delay`'s separateness | The *address* should join `/api/outputs/{n}/delay`, but the semantic difference must stay visible: it is an **advance**, not a delay, and writing it costs a device reconnect. Put that in the response and in `GET /api/outputs` (polarity + cost), not in the URL. |

---

## 8. Ranked

| # | Change | Consumer win | Churn | Risk |
|---|---|---|---|---|
| # | Change | Consumer win | State |
|---|---|---|---|
| 1 | `/api/outputs/{n}/volume` + `/mute` + `/delay` + `/resync`, one scale, dispatch server-side (§1) | high — deletes a dispatcher and a bug class from both consumers | **done** |
| 2 | One socket, topics by message (§5) | high — one reconnect implementation instead of four, and four fewer HTTP/1.1 connections | **done** |
| 3 | Subject in the path, values in the body (§2) | medium — a generic per-output call becomes expressible | **done** |
| 4 | `outputs` as the one field name; drop `align/start`'s `sources` (§3) | medium | **done** |
| 5 | One error convention (§4) | high — every call site stops double-checking status *and* body | **done** |
| 6 | `/api/groups/{tier}` instead of two parallel trees | low — the shapes differ more than they look (`members` vs `targets`, priority, duck) | open |
| 7 | API generation in `/api/status` (§6) | only matters once there is an external consumer | not needed yet |

**What remains is §7's list and §6's generation flag**, and neither is urgent: the
`/api/groups/{tier}` merge is low value (the two shapes differ more than they look), and the
generation flag only starts mattering when something outside this repo speaks to the daemon.
`/api/spike/*` is already dealt with: the harnesses are deleted, and `spike/mod.rs` keeps
the recipe for the next experiment rather than the experiments.

---

## 9. Method

Route table extracted from `bridge-daemon/src/api/mod.rs` (85 routes); request bodies and
response types from the 15 handler modules; consumers read in
`frontend/src/lib/api.ts`, `frontend/src/lib/outputs/level.ts`,
`custom_components/pipewire_audio_router/api.py` and `media_player.py`. Every
inconsistency claimed above is a quotation from one of those files, not an inference —
including the two that are documented bugs (§1's silent wrong write, §4's `200 {ok:
false}`).

Not examined: authentication (there is none beyond the agent's bearer token, and adding
it would change every proposal here), rate limiting, and pagination — no listing is large
enough to need it yet.
