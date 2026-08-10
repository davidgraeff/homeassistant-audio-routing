// Card state + every call to Home Assistant, in one reactive object.
//
// The custom element (card.ts) owns an instance and feeds it `hass`/`setConfig`;
// RoutingCard.svelte only reads it and calls its actions. That split is what
// keeps the Lovelace card contract (imperative property setters on an element)
// out of the component.

import { EMPTY_SNAPSHOT, type CardConfig, type HassLike, type Snapshot } from './types';

const DOMAIN = 'pipewire_audio_router';

export class RoutingModel {
  snapshot = $state<Snapshot>(EMPTY_SNAPSHOT);
  config = $state<CardConfig>({});
  /** `false` until the first snapshot lands — the card shows a placeholder
   *  rather than an empty graph, which would read as "nothing is routed". */
  loaded = $state(false);
  /** Last failure, shown in the card. Cleared by the next successful call, so a
   *  transient daemon hiccup doesn't stick around after it recovers. */
  error = $state<string | null>(null);
  /** A routing call is in flight. The card ignores clicks while it is: the
   *  daemon's push is the confirmation, and a second click before it arrives
   *  would act on the graph as it was, not as it now is. */
  busy = $state(false);

  #hass: HassLike | null = null;
  #connection: unknown = null;
  #unsub: (() => Promise<void>) | null = null;
  /** Bumped on every (re)subscribe so a late-arriving unsubscribe or snapshot
   *  from a superseded subscription can be discarded. */
  #generation = 0;

  setConfig(config: CardConfig): void {
    const previous = this.config.entry_id;
    this.config = config;
    // A different router means a different subscription; a changed title doesn't.
    if (config.entry_id !== previous && this.#hass) void this.#subscribe();
  }

  /** Called by the element on every `hass` update — which in Home Assistant is
   *  every state change in the house, so this must be cheap and must not
   *  resubscribe. `connection` is stable for the life of the frontend session,
   *  so it is the thing worth comparing. */
  setHass(hass: HassLike): void {
    this.#hass = hass;
    if (hass.connection === this.#connection) return;
    this.#connection = hass.connection;
    void this.#subscribe();
  }

  async #subscribe(): Promise<void> {
    const hass = this.#hass;
    if (!hass) return;
    const generation = ++this.#generation;
    await this.#unsubscribe();
    try {
      const unsub = await hass.connection.subscribeMessage<Snapshot>(
        (snapshot) => {
          if (generation !== this.#generation) return;
          this.snapshot = snapshot;
          this.loaded = true;
          this.error = null;
        },
        { type: `${DOMAIN}/subscribe`, ...this.#target() },
      );
      if (generation !== this.#generation) {
        void unsub();
        return;
      }
      this.#unsub = unsub;
    } catch (err) {
      if (generation !== this.#generation) return;
      // Nothing to retry against: this fails when the integration isn't loaded or
      // the card names a router that isn't there, and both need the user to fix
      // something. The message says which.
      this.error = messageOf(err);
      this.loaded = true;
    }
  }

  async #unsubscribe(): Promise<void> {
    const unsub = this.#unsub;
    this.#unsub = null;
    if (!unsub) return;
    try {
      await unsub();
    } catch {
      // The socket went away — which already unsubscribed us.
    }
  }

  /** Detached from the DOM: drop the subscription, and forget which connection we
   *  were on so re-attaching (the same element, on the same session, when its view
   *  comes back) subscribes again instead of sitting silent. */
  disconnect(): void {
    this.#generation++;
    this.#connection = null;
    void this.#unsubscribe();
  }

  #target(): Record<string, string> {
    return this.config.entry_id ? { entry_id: this.config.entry_id } : {};
  }

  async #call(type: string, payload: Record<string, unknown>): Promise<void> {
    const hass = this.#hass;
    if (!hass || this.busy) return;
    this.busy = true;
    try {
      await hass.callWS({ type: `${DOMAIN}/${type}`, ...this.#target(), ...payload });
      this.error = null;
    } catch (err) {
      this.error = messageOf(err);
    } finally {
      this.busy = false;
    }
  }

  /** Route one source into one lone output (additive: an output can mix several). */
  link(source: string, output: string): Promise<void> {
    return this.#call('link', { source, output });
  }

  unlink(source: string, output: string): Promise<void> {
    return this.#call('unlink', { source, output });
  }

  /** Put a whole group on one source — exclusive, so this also drops whatever
   *  else its members were playing. Same call as the group's Source dropdown. */
  routeGroup(groupId: string, source: string): Promise<void> {
    return this.#call('route_group', { group_id: groupId, source });
  }

  unrouteGroup(groupId: string): Promise<void> {
    return this.#call('unroute_group', { group_id: groupId });
  }
}

/** Home Assistant rejects a WebSocket command with `{code, message}`; anything
 *  else here is a thrown Error or a connection teardown constant. */
function messageOf(err: unknown): string {
  if (typeof err === 'object' && err !== null && 'message' in err) {
    const message = (err as { message?: unknown }).message;
    if (typeof message === 'string' && message) return message;
  }
  if (err instanceof Error && err.message) return err.message;
  return 'Home Assistant rejected the request';
}
