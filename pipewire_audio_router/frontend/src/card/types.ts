// The card's wire contract with the integration's WebSocket API
// (custom_components/pipewire_audio_router/ws_api.py).
//
// Deliberately NOT `../lib/types.ts`: that file describes the daemon's full REST
// API, which this card never talks to. The integration sends a reduced payload —
// inputs, outputs, links, music groups, and nothing else — and these types are
// that payload. Keeping them separate is what stops the card from growing a
// dependency on fields (volumes, meters, now-playing, node ids) it doesn't show.

/** One routing endpoint, by its stable node name. */
export interface CardNode {
  /** Stable name — the primary key every routing call uses. */
  node_name: string;
  display_name: string;
  /** In the live graph right now. `false` = configured/previously routed but
   *  currently absent: drawn grayed, and its routes are kept and reapplied when
   *  it comes back, so they are still drawn too. */
  present: boolean;
}

export interface CardLink {
  source: string;
  output: string;
}

/** A named music group: outputs that play the same stream in sync. Membership is
 *  exclusive, so the groups partition the outputs they cover. */
export interface CardGroup {
  id: string;
  name: string;
  /** Member node names. May name an output absent from `outputs` (configured but
   *  never seen) — the card counts it, and routing the group still reaches it. */
  members: string[];
}

export interface Snapshot {
  sources: CardNode[];
  outputs: CardNode[];
  links: CardLink[];
  groups: CardGroup[];
}

export const EMPTY_SNAPSHOT: Snapshot = { sources: [], outputs: [], links: [], groups: [] };

/** Card YAML configuration. Everything is optional: with one router configured
 *  and no options, `type: custom:pipewire-router-card` is the whole card. */
export interface CardConfig {
  type?: string;
  /** Card heading. Empty string removes the header row, same as
   *  `show_title: false`. */
  title?: string;
  /** Show the heading at all. Default `true`. */
  show_title?: boolean;
  /** Show the instruction line under the graph ("Tap an input, then where it
   *  should play…"). Default `true` — worth turning off once the gesture is
   *  known, since it costs two lines on a phone. */
  show_hint?: boolean;
  /** Which router, when several are configured. Omitted = the only one (the
   *  integration answers with an explicit error if that's ambiguous). */
  entry_id?: string;
}

export const DEFAULT_TITLE = 'Audio routing';

/** Whether the header row is drawn. Two ways to say no, because `title: ""` was
 *  the only way before there was a switch — and a config in the wild still means
 *  it. */
export function showTitleOf(config: CardConfig): boolean {
  return config.show_title !== false && (config.title ?? DEFAULT_TITLE) !== '';
}

export const showHintOf = (config: CardConfig): boolean => config.show_hint !== false;

/** The slice of Home Assistant's frontend `hass` object this card uses. It only
 *  needs the connection — no entity states, since routing isn't modelled as
 *  entity attributes. */
export interface HassLike {
  connection: {
    subscribeMessage<T>(
      callback: (message: T) => void,
      subscribeMessage: Record<string, unknown>,
    ): Promise<() => Promise<void>>;
  };
  callWS<T>(message: Record<string, unknown>): Promise<T>;
}
