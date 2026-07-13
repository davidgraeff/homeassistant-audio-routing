// Shapes returned by the bridge-daemon REST API (see docs/api-reference.md).

export interface RoutingNode {
  /** Live PipeWire object id — used for link/unlink and volume calls. Changes
   * when a node's module reloads. */
  node_id: number;
  /** Stable PipeWire node name (survives a module reload). Used as the list
   * key so a reload doesn't tear down the row/column (and its volume slider). */
  node_name: string;
  display_name: string;
}

export interface RoutingMatrix {
  sources: RoutingNode[];
  outputs: RoutingNode[];
  /** `[source_node_id, output_node_id]` pairs currently linked. */
  links: [number, number][];
}

export type Encryption = 'none' | 'RSA' | 'auth_setup';

export interface OutputInfo {
  name: string;
  ip: string;
  port: number;
  encryption: string;
  node_name: string;
  loaded: boolean;
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

export interface SendspinInfo {
  name: string;
  port: number;
  node_name: string;
  running: boolean;
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
