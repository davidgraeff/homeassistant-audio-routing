/**
 * YouTube Music cast receiver.
 *
 * Ties together:
 *   - `yt-cast-receiver` — DIAL discovery + the Lounge API, i.e. how the phone
 *     finds us and drives us. No Google Cast, no device certificates (a Cast
 *     receiver would need a Google-signed device certificate; DIAL + Lounge is
 *     YouTube's surviving pre-Cast path — see ../../../docs/ytmusic-receiver.md).
 *   - `MpvPlayer` (player.js) over `MpvClient` (mpv.js) — the actual playback,
 *     into the `ytm-out` PipeWire sink (`module-rtp-sink`), which transmits to
 *     the audio-router add-on over RTP.
 *
 * Everything is configured by environment variable so the systemd unit (on the
 * Pi) or the add-on's run.sh is the single place that describes an install.
 * Defaults suit the Pi.
 */

import os from 'os';
import { existsSync } from 'fs';
import YouTubeCastReceiver, { Constants } from 'yt-cast-receiver';
import path from 'path';
import startAdmin from './admin.js';
import MetadataReporter from './metadata.js';
import MpvClient from './mpv.js';
import MpvPlayer from './player.js';
import Resolver from './resolver.js';
import YtdlpDaemon from './ytdlp.js';
import { AlreadyRunningError } from './singleton.js';

/** Interfaces that must never be advertised over DIAL. */
const IGNORED_IFACE = /^(lo|docker|veth|br-|virbr|tailscale|zt|wg)/;

/**
 * Pick the LAN address to bind the DIAL server to.
 *
 * This is not cosmetic. An SSDP responder left to its own devices on a
 * multi-homed host answers on *every* interface with the same USN, and a sender
 * that keeps the wrong answer cannot fetch our device description and silently
 * drops us, with nothing logged on our side — measured on a box with a `docker0`,
 * which answered every M-SEARCH twice. Bind explicitly, and log what was chosen.
 */
function pickBindAddress(logger) {
  if (process.env.YTCR_BIND_ADDRESS) {
    return process.env.YTCR_BIND_ADDRESS;
  }
  const candidates = [];
  for (const [ name, addrs ] of Object.entries(os.networkInterfaces())) {
    if (IGNORED_IFACE.test(name)) {
      continue;
    }
    for (const a of addrs ?? []) {
      if (a.family === 'IPv4' && !a.internal) {
        candidates.push({ name, address: a.address });
      }
    }
  }
  if (candidates.length === 0) {
    logger.warn('[ytcr] no usable LAN interface found — letting the DIAL server bind everything');
    return undefined;
  }
  if (candidates.length > 1) {
    logger.warn(
      `[ytcr] multiple LAN interfaces (${candidates.map((c) => `${c.name}=${c.address}`).join(', ')}); ` +
      `using ${candidates[0].address}. Set YTCR_BIND_ADDRESS to override.`);
  }
  return candidates[0].address;
}

function buildMpvArgs() {
  const audioDevice = process.env.YTCR_AUDIO_DEVICE || 'pipewire/ytm-out';
  const args = [
    '--idle=yes',
    '--no-video',
    // NOT `--no-terminal`: that disables mpv's messages entirely, so yt-dlp /
    // ytdl_hook failures vanish and mpv.js's stderr forwarding receives nothing —
    // a resolution failure then looks identical to a hang. Keep messages, drop only
    // terminal *input* handling (there is no tty under systemd anyway).
    '--input-terminal=no',
    '--msg-level=all=warn',
    // Pin the output to the `ytm-out` RTP sink. Without this WirePlumber picks a
    // device, and on this box the wrong pick means the audio never leaves the Pi.
    `--audio-device=${audioDevice}`,
    // Keep the device open across playlist items so the RTP sink is not torn
    // down between songs. Deliberately NOT an option that streams silence
    // forever: `ytm-out` transmits whenever it has a client, so that would put
    // ~1.5 Mbit/s of silence on the radio around the clock.
    '--gapless-audio=yes',
    // Prefer the native 48 kHz Opus stream: `ytm-out` is 48 kHz and so is the
    // router graph, so this keeps the whole path resample-free.
    '--ytdl-format=bestaudio/best',
    '--volume=100',
    // Output-buffer slack, up from mpv's 0.2 s default. This is the knob that
    // absorbs a *scheduling* hiccup: on this box a resolve spike (or anything else
    // that briefly preempts mpv) has under 200 ms to be serviced before the sink
    // runs dry, and a dry sink is an audible gap. One second costs one second of
    // added latency, which this design already accepts — the phone's progress bar
    // leading the sound is documented as expected.
    '--audio-buffer=1',
    // Say it rather than rely on `auto` classifying a bare googlevideo URL as a
    // network stream — with `resolver.js` prefetching, that is what mpv is handed.
    '--cache=yes',
    // Bound the demuxer cache. NOT a readahead limit: `--cache-secs` defaults to
    // 3600000 and overrides `--demuxer-readahead-secs` whenever the cache is on, so
    // mpv reads as far ahead as the network allows either way — this only caps how
    // much RAM that may occupy. mpv's own default is 150 MiB, which on a 512 MB box
    // that is already swapping (measured: 708k pages out) would let one long track
    // push the player into swap. 32 MiB is ~30 min of Opus, so it never binds in
    // practice.
    '--demuxer-max-bytes=32MiB',
  ];
  if (process.env.YTCR_YTDL_PATH) {
    args.push(`--script-opts=ytdl_hook-ytdl_path=${process.env.YTCR_YTDL_PATH}`);
  }

  // Options forwarded to yt-dlp itself. Each goes through
  // `--ytdl-raw-options-append` rather than `--ytdl-raw-options=`, because the
  // latter REPLACES the whole key/value list — a second occurrence would silently
  // drop the first.
  const rawOptions = [];

  // YouTube makes *authenticated* requests solve an `n` signature challenge,
  // which yt-dlp can only do with an external JavaScript runtime. Without one it
  // finds NO formats at all and every track fails — while anonymous playback keeps
  // working, which makes this easy to misdiagnose.
  //
  // yt-dlp enables only `deno` by default, and on this Pi (armv7l) none of the
  // obvious choices work: deno and bun have no 32-bit ARM builds, and Raspbian's
  // node 20 is reported `(unsupported)` by yt-dlp's provider. Debian's `quickjs`
  // does — verified with cookies on the hardware — but it has no JIT, so the setup
  // script installs a private Node 22 and the unit passes `node:<path>` (~22 s vs
  // ~90 s per authenticated resolve). The add-on passes plain `node`. `quickjs` is
  // the fallback, and hence this default.
  rawOptions.push(`js-runtimes=${process.env.YTCR_JS_RUNTIME || 'quickjs'}`);

  // Same persistent yt-dlp cache the resolver uses (resolver.js `#attempts`). mpv's
  // ytdl_hook is only the fallback path now, but when it runs it should not have to
  // re-fetch the challenge-solver scripts, and its default cache location in the
  // add-on's container is not persistent.
  if (process.env.YTCR_YTDLP_CACHE_DIR) {
    rawOptions.push(`cache-dir=${process.env.YTCR_YTDLP_CACHE_DIR}`);
  }

  // NOTE: the remote cipher server (YTCR_CIPHER_URL) is deliberately NOT passed to
  // mpv. yt-dlp picks one JS-challenge provider by preference and the plugin
  // registers none, so any builtin runtime — which mpv needs as its last-resort
  // path — outranks it and the option would be inert. Remote solving happens in
  // resolver.js, which offers the providers one at a time; this path stays local
  // on purpose (see resolver.js #attempts).

  // Cookies: needed for Premium/ad-free and, increasingly, to get a stream
  // at all. Read *per track*, so a freshly provisioned jar takes effect on the
  // next song with no restart. Only added when the file exists, so a missing jar
  // degrades to anonymous resolution instead of failing everything.
  const cookies = process.env.YTCR_COOKIES;
  if (cookies) {
    if (existsSync(cookies)) {
      rawOptions.push(`cookies=${cookies}`);
    }
    else {
      // Deliberately loud: this is the difference between "plays" and "every
      // track fails", and it is invisible otherwise.
      console.warn(`[ytcr] YTCR_COOKIES=${cookies} does not exist — resolving anonymously. `
        + 'Provision it with firmware/pi-ytmusic/push_cookies.py');
    }
  }
  for (const opt of rawOptions) {
    args.push(`--ytdl-raw-options-append=${opt}`);
  }
  if (process.env.YTCR_MPV_EXTRA_ARGS) {
    args.push(...process.env.YTCR_MPV_EXTRA_ARGS.split(' ').filter(Boolean));
  }
  return args;
}

/**
 * Where the resolver daemon's Python lives.
 *
 * Derived from `YTCR_YTDL_PATH` rather than configured: both deployments install
 * yt-dlp into a venv (`/opt/ytdlp` in the add-on, `~/.local/share/pi-ytmusic-venv`
 * on the Pi), and the interpreter that can `import yt_dlp` is by definition the one
 * next to that venv's `yt-dlp` — not whatever `python3` is first on PATH, which in
 * the add-on's container is the system Python with no yt-dlp in it.
 */
function daemonPython() {
  if (process.env.YTCR_YTDLP_PYTHON) {
    return process.env.YTCR_YTDLP_PYTHON;
  }
  if (!process.env.YTCR_YTDL_PATH) {
    return null;
  }
  return path.join(path.dirname(process.env.YTCR_YTDL_PATH), 'python3');
}

/**
 * The `node` binary the resident JS-challenge worker should run on, or null.
 *
 * Taken from `YTCR_JS_RUNTIME`, which is what yt-dlp is told to solve challenges with:
 * `node` in the add-on, `node:/home/…/node22/bin/node` on the Pi (Raspbian's node 20 is
 * rejected by yt-dlp, hence the private tarball). The worker is node-specific — it
 * speaks node's stdin/permission flags — so a box configured for `quickjs` gets no
 * worker and keeps yt-dlp's own path.
 */
function jscResidentNode() {
  if (process.env.YTCR_JSC_RESIDENT === '0') {
    return null;
  }
  const runtime = process.env.YTCR_JS_RUNTIME || 'quickjs';
  if (runtime !== 'node' && !runtime.startsWith('node:')) {
    return null;
  }
  return runtime.startsWith('node:') ? runtime.slice('node:'.length) : 'node';
}

/**
 * The MpvClient, visible to the top-level error handler. An mpv spawned before a
 * later startup step failed has to be killed on the way out, or it stays attached
 * to `ytm-out` — mixing into the RTP stream with nothing left to control it, and
 * `Restart=always` then stacks another one on top of it every few seconds.
 */
let mpvClient = null;
/** Likewise the resolver daemon: a child that outlives a failed startup. */
let ytdlpDaemon = null;

async function main() {
  const socketPath = process.env.YTCR_MPV_SOCKET
    || `${process.env.XDG_RUNTIME_DIR || '/tmp'}/ytmusic-mpv.sock`;

  // Two DISTINCT names on purpose: per the library, `name` is what a sender shows
  // when it found us over DIAL and `screenName` when it found us through manual
  // pairing ("Link with TV code"). Keeping them different means the picker itself
  // tells you which path worked — a free diagnostic.
  const deviceName = process.env.YTCR_DEVICE_NAME || `Musik (${os.hostname()})`;
  const screenName = process.env.YTCR_SCREEN_NAME || `${deviceName} [code]`;

  const dialPort = Number(process.env.YTCR_DIAL_PORT || 8099);
  const audioDevice = process.env.YTCR_AUDIO_DEVICE || 'pipewire/ytm-out';
  const bindAddress = pickBindAddress(console);

  const mpv = new MpvClient({
    binary: process.env.YTCR_MPV_BINARY || 'mpv',
    args: buildMpvArgs(),
    socketPath,
    logger: console, // upgraded to the receiver's logger before start()
  });
  mpvClient = mpv;

  // Now-playing reporting to the add-on (metadata.js). Off unless the add-on host
  // is configured: this role is otherwise a pure RTP sender that talks to nobody,
  // and it must stay able to run that way.
  // Where the audio-router add-on's API lives. Also what the admin page talks to,
  // so it is no longer only about metadata — `YTCR_REPORT_METADATA=false` turns the
  // *reporting* off while leaving the host known (the add-on's `report_metadata`
  // option). Unset means on, which is what the Pi's unit relies on.
  const addonHost = process.env.YTCR_ADDON_HOST;
  const metadata = addonHost && process.env.YTCR_REPORT_METADATA !== 'false'
    ? new MetadataReporter({
      host: addonHost,
      apiPort: Number(process.env.YTCR_ADDON_API_PORT || 8099),
      rtpPort: Number(process.env.YTCR_RTP_PORT || 46001),
      logger: console,
    })
    : null;
  // Pre-resolve upcoming tracks (resolver.js). Uses the same yt-dlp, runtime and
  // cookies as mpv's ytdl_hook, so a prefetched URL is exactly what mpv would have
  // resolved — it just happens while the previous track is still playing.
  const resolver = new Resolver({
    ytdlp: process.env.YTCR_YTDL_PATH,
    cookies: process.env.YTCR_COOKIES && existsSync(process.env.YTCR_COOKIES)
      ? process.env.YTCR_COOKIES : null,
    jsRuntime: process.env.YTCR_JS_RUNTIME || 'quickjs',
    format: process.env.YTCR_FORMAT || 'bestaudio/best',
    cipherUrl: process.env.YTCR_CIPHER_URL || null,
    cipherTimeout: Number(process.env.YTCR_CIPHER_TIMEOUT || 8),
    potUrl: process.env.YTCR_POT_URL || null,
    // `YTCR_CLIENT_LADDER=none` disables the extra client attempts; a comma-separated
    // list replaces them (valid names: web, web_safari, web_embedded, web_music,
    // web_creator, android, android_vr, ios, mweb, tv, tv_downgraded, tv_simply).
    ...(process.env.YTCR_CLIENT_LADDER
      ? { clientLadder: process.env.YTCR_CLIENT_LADDER === 'none'
        ? [] : process.env.YTCR_CLIENT_LADDER.split(',').map((c) => c.trim()).filter(Boolean) }
      : {}),
    cacheDir: process.env.YTCR_YTDLP_CACHE_DIR || null,
  });

  // The long-lived yt-dlp (ytdlp.js). Optional in both directions: no yt-dlp path
  // means no resolver at all, and a daemon that fails to start leaves the resolver
  // spawning per track — the behaviour before it existed. Nothing here may abort
  // startup, because a slow resolver is a worse outcome than an undiscoverable
  // receiver.
  const python = daemonPython();
  if (resolver.enabled && python && process.env.YTCR_YTDLP_DAEMON !== '0') {
    ytdlpDaemon = new YtdlpDaemon({
      python,
      socketPath: process.env.YTCR_YTDLP_SOCKET
        || `${process.env.XDG_RUNTIME_DIR || '/tmp'}/ytmusic-ytdlp.sock`,
      modes: resolver.daemonModes,
      prewarm: process.env.YTCR_YTDLP_PREWARM !== '0',
      jscResident: jscResidentNode(),
      jscPreprocessedCache: process.env.YTCR_YTDLP_PREPROCESSED_CACHE !== '0',
      logger: console, // upgraded to the receiver's logger below
    });
  }

  const player = new MpvPlayer(mpv, metadata, resolver);

  const receiver = new YouTubeCastReceiver(player, {
    dial: {
      port: dialPort,
      ...(bindAddress ? { bindToAddresses: [ bindAddress ] } : {}),
    },
    device: { name: deviceName, screenName },
    logLevel: process.env.YTCR_LOG_LEVEL || 'info',
  });
  mpv.setLogger(receiver.logger);
  resolver.setLogger?.(receiver.logger);
  ytdlpDaemon?.setLogger(receiver.logger);
  metadata?.attach(mpv);

  /** The phone currently driving us, for the admin page to show. Kept from the
   *  events rather than read out of the library, because these are the same two
   *  moments the log already reports. */
  let connectedSender = null;

  receiver.on('senderConnect', (sender) => {
    // Fresh session state for the first track. It expires in minutes (see
    // MAX_WARM_AGE_S), and the first play after a connect is the resolve least able to
    // absorb a retry — the user is waiting on it with nothing prefetched.
    ytdlpDaemon?.refresh('sender connected');
    const isYtMusic = sender.client?.key === Constants.CLIENTS.YTMUSIC.key;
    connectedSender = {
      name: sender.name,
      client: sender.client?.name ?? null,
      yt_music: isYtMusic,
      since: Date.now(),
    };
    receiver.logger.info(
      `[ytcr] sender connected: ${sender.name}` +
      `${sender.client?.name ? ` (${sender.client.name})` : ''}` +
      `${isYtMusic ? ' — YouTube Music' : ''}`);
  });
  receiver.on('senderDisconnect', (sender) => {
    connectedSender = null;
    receiver.logger.info(`[ytcr] sender disconnected: ${sender.name}`);
  });
  receiver.on('error', (error) => {
    receiver.logger.error('[ytcr] receiver error:', error);
  });

  await mpv.start();
  if (ytdlpDaemon) {
    // Started before the receiver so its prewarm resolve overlaps with becoming
    // discoverable. If a cast arrives while the prewarm is still running, the first
    // track waits behind it on the mode's lock rather than starting a second cold
    // extraction beside it — measured: 2.0 s wall for a 1.1 s resolve that queued.
    try {
      await ytdlpDaemon.start();
      resolver.setDaemon(ytdlpDaemon);
    }
    catch (e) {
      receiver.logger.warn(
        `[ytcr] resolver daemon did not start (${e.message}) — resolving with one `
        + 'yt-dlp per track');
      ytdlpDaemon = null;
    }
  }
  await receiver.start();

  // The admin page (admin.js): the RTP source on the router and the cookie jar,
  // which are the two things outside the phone's app that decide whether a cast is
  // audible. Only when a port is set — in the add-on that is ingress, so Home
  // Assistant has authenticated the visitor; on the Pi it is opt-in, because this
  // page writes a Google credential and nothing there asks who is asking.
  const admin = startAdmin({
    port: Number(process.env.YTCR_ADMIN_PORT) || null,
    bind: process.env.YTCR_ADMIN_BIND || undefined,
    device: {
      name: deviceName,
      screenName,
      dialPort,
      bindAddress,
      // `pipewire/ytm-out` is an mpv device spec; the node name is the part the
      // PipeWire graph knows it by.
      sinkName: audioDevice.replace(/^pipewire\//, ''),
    },
    router: {
      host: addonHost ?? '127.0.0.1',
      apiPort: Number(process.env.YTCR_ADDON_API_PORT || 8099),
    },
    rtpPort: Number(process.env.YTCR_RTP_PORT || 46001),
    jar: {
      file: process.env.YTCR_COOKIES,
      // The jar helper is pure stdlib, so any python3 will do — unlike the resolver
      // daemon, which needs the venv's interpreter to import yt_dlp.
      python: process.env.YTCR_ADMIN_PYTHON || 'python3',
      ytdlp: process.env.YTCR_YTDL_PATH,
      jsRuntime: process.env.YTCR_JS_RUNTIME || 'quickjs',
      probeUrl: process.env.YTCR_PROBE_URL || null,
    },
    probes: {
      mpv,
      resolverDaemon: () => !!ytdlpDaemon,
      sender: () => connectedSender,
    },
    logger: receiver.logger,
  });

  receiver.logger.info(
    `[ytcr] ready — DIAL name "${deviceName}" on ${bindAddress ?? 'all interfaces'}:${dialPort}, ` +
    `audio -> ${audioDevice}` +
    (metadata ? `, metadata -> ${addonHost}` : ', metadata reporting off') +
    (ytdlpDaemon ? ', resolver: long-lived yt-dlp' : ', resolver: yt-dlp per track'));

  let stopping = false;
  const shutdown = async (signal) => {
    if (stopping) {
      return;
    }
    stopping = true;
    receiver.logger.info(`[ytcr] ${signal} — shutting down`);
    // First, so a page mid-refresh cannot hold the process open past its answer.
    admin?.close();
    try {
      await receiver.stop();
    }
    catch (e) {
      receiver.logger.warn(`[ytcr] receiver stop failed: ${e.message}`);
    }
    // Leave the add-on with a cleared entry rather than a track that stopped
    // playing when this service did.
    metadata?.stopped();
    metadata?.close();
    await mpv.stop();
    await ytdlpDaemon?.stop();
    process.exit(0);
  };
  process.on('SIGTERM', () => void shutdown('SIGTERM'));
  process.on('SIGINT', () => void shutdown('SIGINT'));
}

main().catch(async (e) => {
  if (e instanceof AlreadyRunningError) {
    // An operator mistake (a second service, or a dev harness against the live
    // one), not a crash — so no stack trace. `Restart=always` will keep retrying
    // every RestartSec, which is both cheap (the lock fails before mpv is
    // spawned) and the behaviour you want: the service recovers by itself as
    // soon as whatever held the lock exits.
    console.error(`[ytcr] not starting: ${e.message}`);
  }
  else {
    console.error('[ytcr] fatal:', e);
  }
  try {
    await mpvClient?.stop();
    await ytdlpDaemon?.stop();
  }
  catch { /* best effort on the way out */ }
  process.exit(1);
});
