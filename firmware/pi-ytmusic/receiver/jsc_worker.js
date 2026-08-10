/**
 * Resident JS-challenge worker.
 *
 * WHY THIS EXISTS
 * ---------------
 * yt-dlp solves YouTube's `n`/`sig` challenges by running the ejs solver in a fresh
 * `node` per resolve, feeding it the solver bundle *and* the player JS on stdin
 * (`jsc/_builtin/node.py`). Measured on the add-on host (aarch64, nice 19), that is
 * **15.15 s per resolve**, split:
 *
 *     bare node startup                      0.90 s
 *     preprocessing the 2.8 MB player       11.76 s   <- meriyah parse + astring regen
 *     parsing the 3.8 MB stdin payload etc.  2.48 s
 *     actually solving the challenges       ~0 ms     <- three cost the same as one
 *
 * So the cost is *parsing*, paid again on every track, and none of it is the solve.
 * Keeping the process alive is what removes it — not a warm JIT, which has nothing to
 * warm up: each challenge transform runs once.
 *
 * WHAT IS CACHED — AND WHAT DELIBERATELY IS NOT
 * ---------------------------------------------
 * The ejs core does `preprocessPlayer(code)` (the ~12 s part) and then
 * `Function('_result', preprocessed)(obj)` to obtain `{n, sig}` solver closures (see
 * `getFromPrepared`/`main` in `yt.solver.core.js`). This worker caches the **preprocessed
 * source** and rebuilds the **closures on every solve**.
 *
 * Caching the closures was tried and is wrong: reproduced on hardware, closures built
 * 4 minutes earlier yielded URLs that resolved and then answered 403, and killing only
 * this process fixed it while rebuilding the daemon's `YoutubeDL` did not. The ejs core
 * rebuilds per call for a reason.
 *
 * The preprocessed player is also handed back to Python for its disk cache, so a worker
 * restart costs a `Function()` compile rather than another 12 s preprocess.
 *
 * PROTOCOL (newline-delimited JSON on stdin/stdout, ids correlated by the caller)
 * -----------------------------------------------------------------------------
 *     -> {"op":"init","lib":"<js>","core":"<js>"}
 *     <- {"op":"init","ok":true}
 *     -> {"op":"solve","id":1,"player_id":"854a788e-main","requests":[...]}
 *     <- {"id":1,"need":"code"}                          // nothing cached for it yet
 *     -> {"op":"solve","id":1,...,"preprocessed":"<js>"}  // or "player":"<js>"
 *     <- {"id":1,"responses":[...],"preprocessed":"<js>"?,"cached":false}
 *
 * The source is passed to `node -e`, never imported, so the process can run under
 * node's permission model with no filesystem access at all — the same sandbox
 * yt-dlp gives the player JS it evaluates.
 */

'use strict';

/**
 * playerId -> preprocessed player source.
 *
 * The *source* is what is worth keeping: producing it costs ~12 s (meriyah parsing 2.8 MB
 * of player JS). The solver closures built from it are NOT kept — see solve().
 */
const sources = new Map();
/** playerIds whose closures could not be built, so the public entry point is used. */
const useEntryPoint = new Set();
/**
 * The ejs entry point, evaluated once.
 *
 * NOT named `jsc`: the core script declares `var jsc` in global scope, and a top-level
 * `let jsc` here is a global *lexical* binding that `var` may not shadow — node fails
 * the whole init with "Identifier 'jsc' has already been declared". Found by running
 * it; the name is load-bearing.
 */
let entry = null;

/** Bound so one stale player version cannot grow the process without limit. */
const MAX_PLAYERS = 4;

function send(obj) {
  process.stdout.write(`${JSON.stringify(obj)}\n`);
}

function note(message) {
  // This process's stderr is the daemon's stderr. Wear the daemon's tag, because this
  // worker *is* part of the daemon — `ytdlp.js` marks anything untagged as
  // `[ytdlpd:raw]`, which should stay reserved for text nobody claims.
  process.stderr.write(`[ytdlpd] [jsc-worker] ${message}\n`);
}

function init(msg) {
  // Composed exactly as node.py does it: the lib script declares `lib`, its exports
  // are spread onto the global object, then the core script declares `jsc`.
  // Indirect eval, so those `var`s land in global scope rather than in this function.
  const geval = eval;
  geval(msg.lib);
  Object.assign(globalThis, globalThis.lib);
  geval(msg.core);
  entry = globalThis.jsc;
  if (typeof entry !== 'function') {
    throw new Error('the core script did not define jsc()');
  }
  send({ op: 'init', ok: true });
}

/**
 * Turn a preprocessed player into `{n, sig}` closures.
 *
 * This is `getFromPrepared` from the ejs core, which is inside its IIFE and not
 * exported — so it is reproduced here, and treated as a contract that may break: a
 * failure is caught by the caller, which then falls back to the public `jsc()` entry
 * point for that player. Slower, but never wrong.
 */
function buildSolvers(preprocessed) {
  const result = { n: null, sig: null };
  Function('_result', preprocessed)(result);
  if (typeof result.n !== 'function' && typeof result.sig !== 'function') {
    throw new Error('no solver functions in the preprocessed player');
  }
  return result;
}

/** Apply cached closures to one request, in the shape the ejs core returns. */
function solveWith(cached, request) {
  const solver = cached[request.type];
  if (typeof solver !== 'function') {
    return { type: 'error', error: `no ${request.type} solver for this player` };
  }
  try {
    return {
      type: 'result',
      data: Object.fromEntries(request.challenges.map((c) => [ c, solver(c) ])),
    };
  }
  catch (e) {
    return { type: 'error', error: `${e && e.message ? e.message : e}` };
  }
}

function remember(playerId, value, store) {
  store.set(playerId, value);
  // Insertion-ordered, so the oldest player version goes first.
  while (store.size > MAX_PLAYERS) {
    store.delete(store.keys().next().value);
  }
}

function solve(msg) {
  const { id, player_id: playerId, requests } = msg;

  // Not cached: the closures. Reproduced on the add-on (2026-08-10) — a worker whose
  // closures were built 4 minutes earlier produced URLs that resolved and then answered
  // **403**, while killing *only* this process (leaving the daemon and its YoutubeDL
  // untouched) restored working URLs immediately. So something the solvers close over
  // does not survive the passage of time, and the ejs core is right to rebuild them from
  // the preprocessed player on every `jsc()` call (`getFromPrepared` inside `main`).
  //
  // Keeping the *source* keeps nearly all of the win regardless: the ~12 s preprocess is
  // paid once per player version, and a rebuild is a `Function()` compile, which V8 does
  // lazily — measured at 13 ms on x86_64.
  if (sources.has(playerId) && !useEntryPoint.has(playerId)) {
    const started = process.hrtime.bigint();
    let cached;
    try {
      cached = buildSolvers(sources.get(playerId));
    }
    catch (e) {
      note(`rebuilding solvers for ${playerId} failed (${e.message}) — using the ejs entry point`);
      useEntryPoint.add(playerId);
      cached = null;
    }
    if (cached) {
      const buildMs = Number(process.hrtime.bigint() - started) / 1e6;
      send({ id, responses: requests.map((r) => solveWith(cached, r)),
        source: 'rebuilt', build_ms: Math.round(buildMs) });
      return;
    }
  }

  if (useEntryPoint.has(playerId)) {
    const out = entry({
      type: 'preprocessed', preprocessed_player: sources.get(playerId), requests,
    });
    if (out.type === 'error') {
      send({ id, error: `${out.error}` });
      return;
    }
    send({ id, responses: out.responses, source: 'ejs-entry' });
    return;
  }

  if (msg.preprocessed) {
    remember(playerId, msg.preprocessed, sources);
    solve({ ...msg, preprocessed: null });
    return;
  }

  if (msg.player) {
    // The expensive path, through the public entry point: preprocess and solve in one go,
    // then keep the preprocessed player for every later rebuild and for Python's disk
    // cache.
    const out = entry({
      type: 'player', player: msg.player, requests, output_preprocessed: true,
    });
    if (out.type === 'error') {
      send({ id, error: `${out.error}` });
      return;
    }
    const preprocessed = out.preprocessed_player || null;
    if (preprocessed) {
      remember(playerId, preprocessed, sources);
    }
    send({ id, responses: out.responses, preprocessed, source: 'player' });
    return;
  }

  // Nothing held for this player: ask, rather than make Python read a few MB of JS off
  // disk (or download it) that may not be needed.
  send({ id, need: 'code' });
}

function handle(line) {
  let msg;
  try {
    msg = JSON.parse(line);
  }
  catch (e) {
    note(`unparseable request: ${e.message}`);
    return;
  }
  try {
    if (msg.op === 'init') {
      init(msg);
    }
    else if (msg.op === 'solve') {
      solve(msg);
    }
    else if (msg.op === 'ping') {
      send({ id: msg.id, ok: true, players: [ ...sources.keys() ] });
    }
    else {
      send({ id: msg.id, error: `unknown op ${msg.op}` });
    }
  }
  catch (e) {
    // Never die on one bad request: the daemon would respawn us and every cached
    // player would be re-preprocessed at 11.8 s each.
    send({ id: msg.id, error: `${e && e.stack ? e.stack.split('\n')[0] : e}` });
  }
}

let buffer = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  buffer += chunk;
  let nl;
  while ((nl = buffer.indexOf('\n')) !== -1) {
    const line = buffer.slice(0, nl);
    buffer = buffer.slice(nl + 1);
    if (line.trim()) {
      handle(line);
    }
  }
});
process.stdin.on('end', () => process.exit(0));
