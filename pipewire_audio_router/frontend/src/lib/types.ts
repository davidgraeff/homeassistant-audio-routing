// Shapes returned by the bridge-daemon REST API (see docs/api-reference.md).

export interface RoutingNode {
  /** Stable node name — the primary key for routing (survives reloads/churn). */
  node_name: string;
  display_name: string;
  /** In the live graph right now. `false` = offline (configured/previously
   * routed but currently absent) — shown grayed; routing kept and reapplied. */
  present: boolean;
  /** Outputs only: manually-configured (`true`) vs mDNS auto-discovered
   * (`false`) — drives the "auto" badge. Always `true` for sources. */
  configured: boolean;
  /** Live PipeWire object id when present (for volume calls); null offline. */
  node_id: number | null;
  /** Recent peak level 0.0–1.0 for the meter (sources, while the matrix is
   * open); 0 for outputs/unmetered. */
  peak: number;
}

export interface RoutingLink {
  source: string;
  output: string;
}

export interface RoutingMatrix {
  sources: RoutingNode[];
  outputs: RoutingNode[];
  /** Persisted routing intent, by stable name — the linked cells (may include
   * a link to a currently-offline endpoint, shown grayed). */
  links: RoutingLink[];
}

export type Encryption = 'none' | 'RSA' | 'auth_setup';

export interface OutputInfo {
  node_name: string;
  name: string;
  /** 'airplay' (RAOP) or 'sendspin' — for the Type column. */
  kind: 'airplay' | 'sendspin';
  /** In the live graph now. */
  present: boolean;
  /** Manual store entry (`true`) vs mDNS auto-discovered (`false`). */
  configured: boolean;
  /** Connection details — known only for configured AirPlay entries (else null). */
  ip: string | null;
  port: number | null;
  encryption: string | null;
}

export interface AddOutputRequest {
  name: string;
  ip: string;
  port?: number;
  encryption?: Encryption;
}

export interface AirplaySourceInfo {
  name: string | null;
  running: boolean;
  /** Producer jitter-buffer target in ms. Higher = fewer stutters, more latency. */
  latency_msec: number;
  /** Advertise the auth-setup encryption mode so encryption-requiring senders can connect. */
  auth_setup: boolean;
  /** Refuse a new sender while one is already streaming (anti-takeover). */
  prevent_takeover: boolean;
}

export interface AirplayClient {
  /** Stable identifier for forget calls: the name if known, else the IP. */
  key: string;
  /** Friendly device name once the sender advertised one; null if only seen by IP. */
  name: string | null;
  /** Most recent IP address this client connected from. */
  addr: string;
  /** Unix seconds this client was first ever seen. */
  first_seen: number;
  /** Unix seconds of the most recent connection. */
  last_connected: number;
  /** Streaming to the AirPlay source right now. */
  connected: boolean;
  /** Future sessions from this client are refused (enforced at RTSP SETUP). */
  banned: boolean;
  /** Takeover priority: a higher-priority sender bumps a lower-priority one. */
  priority: number;
}

export interface RtpSourceInfo {
  /** Whether the source is enabled in the store. */
  enabled: boolean;
  /** UDP port it listens on (the stored value, or the default when disabled). */
  port: number;
  /** Receiver-side jitter buffer target in ms (stored value, or default when
   *  disabled). Higher = more dropout tolerance on a weak link, more latency. */
  latency_msec: number;
  /** `source.ip`: `0.0.0.0` = unicast, or a multicast group so several
   *  receivers can share one firmware stream. */
  source_addr: string;
  /** `sess.ignore-ssrc`: `true` accepts any sender on the port, `false` locks
   *  onto the first SSRC and rejects the rest ("Only one client"). */
  ignore_ssrc: boolean;
  /** Whether the `bt-bridge-rtp` node is present in the live PipeWire graph. */
  loaded: boolean;
}

export interface MediaPlayerInfo {
  node_id: number;
  node_name: string;
  state: 'playing' | 'idle';
  volume: number | null;
}

export interface OpResponse {
  ok: boolean;
  message: string;
}

export interface VolumeResponse {
  volume: number | null;
  message: string | null;
}

export interface WyomingAnnounce {
  host: string;
  port?: number;
  text: string;
  voice?: string | null;
}
