# AP1 receiver: PCM codec + auth-setup mode — implementation plan

Vendored `shairplay` (AirPlay 1 receive path) for the HA PipeWire audio-routing
add-on. Goal: broaden AP1 sender compatibility along two axes the crate did not
cover — **codec** (add raw PCM/L16 next to ALAC) and **encryption/auth** (add
the MFi `auth-setup` gate next to RSA), with per-connection auto-dispatch and
configurable mDNS advertising.

Status legend: ✅ done · 🔬 needs live validation · ⏸️ deferred to user.

---

## 0. Background: what already worked

The AP1 receive path already handled more than its mDNS advertising admitted
(verified by reading the source):

| Capability | State | Where |
|---|---|---|
| RSA key exchange (`a=rsaaeskey`) | decrypts | `handlers_ap1.rs::handle_announce` |
| FairPlay DRM (`POST /fp-setup` + `a=fpaeskey`) | decrypts | `handlers_ap1.rs::handle_fp_setup` |
| Unencrypted audio (all-zero key ⇒ passthrough) | works | `buffer.rs::queue` |
| Apple-Challenge RSA response | works | `rtsp.rs` middleware |
| Digest password auth | works | `rtsp.rs` middleware |
| Audio decode | **ALAC only** | `buffer.rs` (unconditional `alac.decode_frame`) |
| mDNS `cn`/`et` | **hardcoded** `cn=1`, `et=0` | `net/mdns.rs` |
| `POST /auth-setup` | **absent** | — |

So the work is: a PCM decode branch, rtpmap-driven per-connection dispatch,
configurable advertising, and an `/auth-setup` responder.

---

## 1. Phase-3 research findings (auth-setup) — CONCLUSIVE

Researched against the live system: PipeWire **1.6.7** (exact installed version)
and shairport-sync 4.3.7, cross-checked with the PipeWire source at tag 1.6.7.

### From `module-raop-sink.c` (sender)

- `raop.encryption.type` ∈ {`none` (default), `RSA`, `auth_setup`};
  `raop.audio.codec` **"Needs to be PCM"** (man page) and defaults to PCM.
- The **PCM** codec (`write_codec_pcm`) does **not** emit raw L16. It wraps
  samples in an **uncompressed-ALAC frame** (`Is-not-compressed=1` bit set),
  byte-swapped, and always announces `a=rtpmap:96 AppleLossless` +
  `a=fmtp:96 <frames> 0 16 40 10 14 2 255 0 0 <rate>`. ⇒ **The existing ALAC
  decoder already handles PipeWire's audio.** (This is why the add-on works today
  with `cn=1`/ALAC + `et=0`.)
- `auth_setup` flow: `POST /auth-setup` with a **fixed 33-byte body**
  (`0x01` + 32 constant bytes). The reply is **never parsed** — only the HTTP
  status is checked; non-200 ⇒ `pw_impl_module_schedule_destroy`; 200 ⇒ proceed
  to ANNOUNCE. In `auth_setup`/`none` modes **no AES encryption** is applied and
  the SDP carries **no `rsaaeskey`/`aesiv`** ⇒ audio arrives unencrypted.

### On-the-wire spike (PipeWire raop-sink `auth_setup` → shairport-sync)

`scratchpad/authsetup_spike.sh`, capture `authsetup.pcap`:

1. `OPTIONS` → `200 OK`.
2. `POST /auth-setup` `Content-Length: 33`, body **byte-for-byte** the source
   constant: `01 59 02 ed e9 0d 4e f2 bd 4c b6 8a 63 30 03 82 07 a9 4d bd 50 d8
   aa 46 5b 5d 8c 01 2a 0c 7e 1d 4e`.
3. shairport-sync 4.3.7 logs **"Unhandled POST /auth-setup"** and replies
   **`500 Internal Server Error`** → PipeWire immediately FIN/TEARDOWN.

**Conclusions:**
- To support PipeWire's `auth_setup` a receiver only has to answer `/auth-setup`
  with **`200 OK`**; the body is ignored. No Apple MFi certificate, no crypto,
  no `#![forbid(unsafe_code)]`/licensing conflict.
- shairport-sync (our reference) does **not** implement it — so this is a
  genuine capability gain, not a re-implementation.
- The audio-key concern from the original plan is **moot**: auth_setup audio is
  unencrypted, handled by the existing all-zero-key passthrough.

**Honest limitation:** a sender that *cryptographically validates* the
auth-setup reply (real Apple MFi gear) cannot be satisfied without Apple's
private material, which we will not embed. Such senders should use FairPlay
(`fp-setup`) or `none`/`RSA` instead. In practice they target speakers via
FairPlay, not generic receivers, so this does not affect the add-on's senders.

---

## 2. Design

**Two independent axes, negotiated per connection from the SDP — advertise the
supported set, dispatch on what actually arrives.**

- **Codec** (from `a=rtpmap`, decided once per ANNOUNCE):
  - `AppleLossless` → ALAC (uses `a=fmtp`; existing path). Also covers
    PipeWire's uncompressed-ALAC "PCM".
  - `L16/<rate>[/<ch>]` → raw PCM: big-endian interleaved S16 → f32, no ALAC.
- **Encryption/auth** — already auto-dispatched in `handle_announce`
  (rsaaeskey → fpaeskey → none). Add the `auth_setup` **gate** (`/auth-setup`
  → 200) which is orthogonal: it does not change the audio path (stays
  unencrypted passthrough).

Decryption stays codec-agnostic — it runs before decode and keys off
`aeskey == [0;16]` — so PCM composes with none/RSA/FairPlay for free.

### mDNS advertising gotcha (carried from prior interop findings)

PipeWire's `raop-discover` picks the **first** listed `cn` value but the
**highest** `et` value, and its RSA path is broken in 1.6.7. So the advertised
sets are made configurable with **safe defaults preserving current behaviour**
(`cn="1"`, `et="0"`); auto-dispatch still accepts anything a sender sends. The
add-on can opt into advertising PCM/auth-setup without changing the default.

---

## 3. Phase 1 — PCM (L16) codec  ✅

- `buffer.rs`: introduce `StreamCodec` (`Alac(AlacConfig)` | `Pcm { channels,
  sample_rate }`), chosen from `rtpmap` in `RaopBuffer::new`. `queue()` branches:
  ALAC → existing decode; PCM → decrypt (shared) then big-endian S16 → f32.
  Per-entry buffer sized to a packet-max bound (PCM frame length is variable).
- `rtp.rs`: derive `AudioFormat` (channels/sample_rate) from the chosen
  `StreamCodec` instead of assuming `AlacConfig`; resampler wiring unchanged.
- Note: PipeWire 1.6.7 never sends true L16 (see §1); this covers spec-compliant
  L16 senders (e.g. owntone/`cliraop`, other tools) and future-proofs the add-on.

## 4. Phase 2 — configurable `cn`/`et` advertising  ✅

- `RaopServerBuilder::advertise_codecs(&[Codec])` /
  `advertise_encryption(&[Encryption])`, plumbed via `RaopShared` into
  `AirPlayServiceInfo::new`. Defaults reproduce today's `cn="1"`, `et="0"`.
- `net/mdns.rs`: emit the configured, ordered sets (ordering semantics per §2).
- ANNOUNCE dispatch already automatic — no handler change for RSA/none/FairPlay.

## 5. Phase 3 — `POST /auth-setup` responder  ✅ (per research)

- Add route `POST /auth-setup → handlers_ap1::handle_auth_setup`.
- Handler: validate the request is `0x01` + 32 bytes (33 total), reply `200 OK`.
  Body: our own generated 32-byte X25519 public key (plausible shape, contains
  no Apple material; PipeWire ignores it). Empty body would also work for
  PipeWire — the 32-byte form is closer to protocol for lenient validators.
- Advertise `auth_setup` in `et` only when the consumer opts in (Phase 2),
  because of the PipeWire highest-`et`-wins behaviour.

---

## 6. Testing — ✅ done

- **Unit/integration (`cargo test`, default + `--features ap2`, all green):**
  - `L16/44100/2` and `L16/44100` (mono) rtpmap → PCM path; big-endian-S16→f32
    correctness; `AppleLossless`/bare-payload → ALAC (unchanged).
  - `/auth-setup` route: the exact 33-byte PipeWire body → `200 OK`.
  - `advertise_codecs`/`advertise_encryption` → correct ordered `cn`/`et` TXT
    (defaults `0,1` / `0`; e.g. `[None,AuthSetup,Rsa]` → `et="0,4,1"`).
  - Fixed a latent break: the vendored `cn="1"` patch had silently broken the
    AP2 `cn="0,1"` advertisement + its test; AP2 now uses its own `RAOP_AP2_CN`.
- **Live end-to-end (PipeWire 1.6.7 raop-sink → this receiver, headless):**
  - `raop.encryption.type=auth_setup`: full lifecycle completes and audio flows
    — `audio_init codec=Pcm 44100/2`, `total_samples=66978 peak=0.117` (real
    signal). This is the case that aborted at `500` against shairport-sync (§1).
  - `raop.encryption.type=none`: identical result (regression pass).
  - `raop-discover` auto-creates a sink from our mDNS advert (`cn=0,1`,`et=0`),
    confirming the new default does not wedge discovery.
  - True-L16 PCM path: covered by unit tests; PipeWire 1.6.7 never emits real
    L16 (its "PCM" is uncompressed-ALAC), so no live L16 sender was available.

---

## 7. Deferred to user / open decisions  ⏸️

1. **`et` advertising default stays `"0"` (none).** PipeWire's `raop-discover`
   selects the **highest** `et`, and PipeWire's RSA is broken in 1.6.x, so
   advertising `auth_setup` (`et=4`) by default could steer discover-configured
   senders onto auth_setup unnecessarily. The receiver *handles* auth_setup when
   a sender asks for it; the add-on can advertise it via
   `advertise_encryption([…, AuthSetup])` if a specific sender needs discovery to
   pick it. **Decide per target install.** (`cn` default was changed to `"0,1"` —
   see the behaviour-change note below; that one is validated and low-risk.)
2. **auth-setup reply body**: 32-byte ephemeral X25519 pubkey chosen as a
   no-Apple-material default (PipeWire ignores the body). If a specific
   non-PipeWire sender is required and cryptographically validates it, capture
   its exchange and revisit — cannot be resolved without that sender in hand.
3. **Final audio validation on the real HA box / target senders** — the live
   spike here used the developer machine's PipeWire; confirm on the appliance.

## 8. Behaviour change for the add-on

The AP1 `cn` default changed from the vendored `"1"` (ALAC-only, a workaround
for the missing PCM decoder) back to the upstream `"0,1"` (PCM+ALAC). This is
what `airplay_source.rs` now advertises, since it uses builder defaults. It is
**validated** (discover round-trip + direct streaming both work) and is arguably
safer for PipeWire, which can only *encode* PCM. To pin the old value:
`RaopServer::builder().advertise_codecs([Ap1Codec::Alac])`.
