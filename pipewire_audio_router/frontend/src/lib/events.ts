/**
 * The one push socket: `GET /api/events`, with topics subscribed by message.
 *
 * There used to be four sockets — routing, the alignment session, the measurement run,
 * the equivalence experiment — and a browser allows **six** connections per host over
 * HTTP/1.1. The alignment wizard alone held three of them while the routing graph held a
 * fourth, so the REST calls those same pages make queued behind idle sockets. One socket
 * fixes that, and per-topic subscription is what makes it cheap: a page that leaves
 * unsubscribes, the daemon stops the work (metering and the profiler are armed by the
 * `meters` subscription, not by the connection), and the connection stays for the next
 * page.
 *
 * Consumers never see the socket. They call `onTopic(topic, handler)` and get an
 * unsubscribe function; this module owns the connection, the reconnect, the refcounting
 * and the one place that knows each frame's payload key.
 */
import { wsUrl } from './api';

/** Every topic the daemon offers (`events::Topic`). */
export type Topic =
  | 'matrix'
  | 'outputs'
  | 'discovered'
  | 'agents'
  | 'now_playing'
  | 'meters'
  | 'align'
  | 'measure'
  | 'equivalence';

/** How long before a dropped socket is retried. Matches what each of the four separate
 *  clients used, and every consumer still has its REST fallback for the gap. */
const RETRY_MS = 2000;

/**
 * Where each topic's payload lives inside its frame — the single place that knows the
 * wire shape.
 *
 * `matrix` is the exception and deliberately so: that frame historically *was* the
 * matrix, a bare `{sources, outputs, links}`, so `type` sits beside those fields rather
 * than nesting them.
 */
const PAYLOAD: Record<Topic, string | null> = {
  matrix: null,
  outputs: 'outputs',
  discovered: 'outputs',
  agents: 'agents',
  now_playing: 'sources',
  meters: 'nodes',
  align: 'state',
  measure: 'status',
  equivalence: 'status',
};

type Handler = (payload: unknown) => void;

/** Handlers per topic. The count is what decides whether the daemon is told. */
const handlers = new Map<Topic, Set<Handler>>();
/** Consumers watching the connection itself (for a "reconnecting…" hint). */
const connectionWatchers = new Set<(connected: boolean) => void>();

let socket: WebSocket | null = null;
let retry: ReturnType<typeof setTimeout> | null = null;
let connected = false;

function setConnected(next: boolean) {
  if (connected === next) return;
  connected = next;
  for (const w of connectionWatchers) w(next);
}

/** Is the socket up right now? */
export function eventsConnected(): boolean {
  return connected;
}

/** Watch the connection state. Returns an unsubscribe function. */
export function onConnection(watch: (connected: boolean) => void): () => void {
  connectionWatchers.add(watch);
  watch(connected);
  return () => connectionWatchers.delete(watch);
}

function send(op: 'subscribe' | 'unsubscribe', topics: Topic[]) {
  if (!socket || socket.readyState !== WebSocket.OPEN || topics.length === 0) return;
  socket.send(JSON.stringify({ op, topics }));
}

function open() {
  if (socket || handlers.size === 0) return;
  let sock: WebSocket;
  try {
    sock = new WebSocket(wsUrl('api/events'));
  } catch {
    scheduleRetry();
    return;
  }
  socket = sock;
  sock.onopen = () => {
    if (socket !== sock) return;
    setConnected(true);
    // Re-subscribe on every connect, not just the first: after a reconnect the daemon
    // knows nothing about what this client wants, and each subscription answers with
    // that topic's current state — which is what makes a reconnect self-healing.
    send('subscribe', [...handlers.keys()]);
  };
  sock.onmessage = (ev) => {
    let frame: Record<string, unknown>;
    try {
      frame = JSON.parse(String(ev.data)) as Record<string, unknown>;
    } catch {
      return; // malformed: ignore rather than tear the socket down
    }
    const type = frame.type as Topic | 'subscribed' | undefined;
    if (type === undefined) return;
    if (type === 'subscribed') {
      const unknown = frame.unknown as string[] | undefined;
      // A topic this daemon does not have would otherwise be an eternal silence, and
      // the daemon says so explicitly — so say it once, loudly, in the console.
      if (unknown?.length) console.warn(`/api/events rejected unknown topic(s): ${unknown.join(', ')}`);
      return;
    }
    const set = handlers.get(type);
    if (!set) return; // a topic we unsubscribed from, mid-flight
    const key = PAYLOAD[type];
    const payload = key === null ? frame : frame[key];
    for (const h of [...set]) h(payload);
  };
  sock.onclose = () => {
    if (socket !== sock) return;
    socket = null;
    setConnected(false);
    scheduleRetry();
  };
  // `onerror` is always followed by `onclose`, which is where the recovery lives.
  sock.onerror = () => {};
}

function scheduleRetry() {
  if (retry || handlers.size === 0) return;
  retry = setTimeout(() => {
    retry = null;
    open();
  }, RETRY_MS);
}

function closeIfIdle() {
  if (handlers.size > 0) return;
  if (retry) {
    clearTimeout(retry);
    retry = null;
  }
  const sock = socket;
  socket = null;
  setConnected(false);
  if (sock) {
    sock.onclose = null;
    sock.onerror = null;
    sock.close();
  }
}

/**
 * Receive one topic's frames. Returns the unsubscribe function — call it when the page
 * or component goes away, which is what stops the daemon doing that work.
 *
 * Subscribing sends the topic's **current state** straight back (the daemon treats a
 * subscription like a connect), so a consumer needs no separate initial fetch. The one
 * exception is `meters`, which has nothing to say until its next 250 ms tick.
 */
export function onTopic(topic: Topic, handler: Handler): () => void {
  let set = handlers.get(topic);
  const fresh = set === undefined;
  if (!set) {
    set = new Set();
    handlers.set(topic, set);
  }
  set.add(handler);
  if (socket === null) {
    open();
  } else if (fresh) {
    // The socket is already up and this is the first consumer of this topic: ask for
    // it. A second consumer of the same topic costs nothing — the daemon is already
    // sending it, and re-subscribing would only make it resend.
    send('subscribe', [topic]);
  }
  return () => {
    const live = handlers.get(topic);
    if (!live) return;
    live.delete(handler);
    if (live.size > 0) return;
    handlers.delete(topic);
    send('unsubscribe', [topic]);
    closeIfIdle();
  };
}
