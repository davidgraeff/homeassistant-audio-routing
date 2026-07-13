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
}

export interface RtpSourceInfo {
  /** Whether the source is enabled in the store. */
  enabled: boolean;
  /** UDP port it listens on (the stored value, or the default when disabled). */
  port: number;
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
