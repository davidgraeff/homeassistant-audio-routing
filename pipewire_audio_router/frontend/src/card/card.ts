// `custom:pipewire-router-card` — the Lovelace entry point.
//
// This file is the whole Lovelace contract (an element with `setConfig`, a `hass`
// setter, and sizing hints) and nothing else; the UI is RoutingCard.svelte and all
// state/IO is RoutingModel. Built as one self-contained ES module and served by
// the integration itself (custom_components/pipewire_audio_router/frontend.py), so
// there is no resource to register by hand.

import { mount, unmount } from 'svelte';
import RoutingCard from './RoutingCard.svelte';
import CardEditor from './CardEditor.svelte';
import { RoutingModel } from './model.svelte';
import { EditorModel } from './editor.svelte';
import { showTitleOf, type CardConfig, type HassLike } from './types';

const TAG = 'pipewire-router-card';
const EDITOR_TAG = `${TAG}-editor`;

class PipewireRouterCard extends HTMLElement {
  #model = new RoutingModel();
  #app: ReturnType<typeof mount> | null = null;

  connectedCallback(): void {
    // Light DOM on purpose: the component's styles are compiled in and scoped by
    // Svelte, and Home Assistant's theme variables reach it by inheritance — a
    // shadow root would buy isolation we don't need and complicate `ha-card`.
    if (!this.#app) this.#app = mount(RoutingCard, { target: this, props: { model: this.#model } });
  }

  disconnectedCallback(): void {
    // Lovelace detaches a card whenever its view goes away and re-attaches it on
    // return, so this is the only place the subscription can be closed — holding
    // it open would leave one live subscription per view ever visited.
    if (this.#app) {
      void unmount(this.#app);
      this.#app = null;
    }
    this.#model.disconnect();
  }

  setConfig(config: CardConfig): void {
    this.#model.setConfig({ ...config });
  }

  /** Called by Lovelace on every Home Assistant state change — cheap by design
   *  (see `RoutingModel.setHass`). */
  set hass(hass: HassLike) {
    this.#model.setHass(hass);
  }

  /** Masonry-view height, in Lovelace's ~50 px units: the header (when there is
   *  one) plus one per row, the taller column deciding. */
  getCardSize(): number {
    return this.#height();
  }

  /** Sections-view sizing. Wide by default (the card is two columns and a gutter),
   *  and as tall as its rows need. */
  getGridOptions(): Record<string, unknown> {
    return { columns: 12, min_columns: 6, rows: this.#height(), min_rows: 2 };
  }

  #height(): number {
    return (showTitleOf(this.#model.config) ? 1 : 0) + Math.max(1, this.#rows());
  }

  #rows(): number {
    const { sources, outputs, groups } = this.#model.snapshot;
    const grouped = new Set(groups.flatMap((g) => g.members));
    const targets = groups.length + outputs.filter((o) => !grouped.has(o.node_name)).length;
    return Math.max(sources.length, targets);
  }

  /** What the card picker inserts. No options needed for the common case of one
   *  configured router. */
  static getStubConfig(): CardConfig {
    return { type: `custom:${TAG}` };
  }

  /** The visual editor behind the card's "Edit" pane. Without this, Lovelace only
   *  offers the YAML editor for a custom card. */
  static async getConfigElement(): Promise<HTMLElement> {
    await ensureHaFormLoaded();
    return document.createElement(EDITOR_TAG);
  }
}

/** `ha-form` lives in Home Assistant's frontend bundle, and a custom card cannot
 *  import it. It is loaded by the time the edit dialog opens in practice — but
 *  asking a built-in card for *its* editor is the established way to be sure,
 *  since that is what pulls the editor chunk in. Best-effort: the editor renders
 *  a "use the YAML editor" note if this doesn't work out. */
async function ensureHaFormLoaded(): Promise<void> {
  if (customElements.get('ha-form')) return;
  try {
    const loader = (window as unknown as { loadCardHelpers?: () => Promise<unknown> }).loadCardHelpers;
    const helpers = (await loader?.()) as
      | { createCardElement?: (config: unknown) => Promise<{ constructor: { getConfigElement?: () => unknown } }> }
      | undefined;
    const card = await helpers?.createCardElement?.({ type: 'entities', entities: [] });
    await (card?.constructor as { getConfigElement?: () => unknown } | undefined)?.getConfigElement?.();
  } catch {
    // Nothing to recover: the editor says so, and YAML editing still works.
  }
}

class PipewireRouterCardEditor extends HTMLElement {
  #model = new EditorModel();
  #app: ReturnType<typeof mount> | null = null;

  constructor() {
    super();
    // How a Lovelace editor reports an edit. `composed` matters: the dialog
    // listens outside this element's tree.
    this.#model.onChange = (config: CardConfig) => {
      this.dispatchEvent(
        new CustomEvent('config-changed', { detail: { config }, bubbles: true, composed: true }),
      );
    };
  }

  connectedCallback(): void {
    if (!this.#app) this.#app = mount(CardEditor, { target: this, props: { model: this.#model } });
  }

  disconnectedCallback(): void {
    if (this.#app) {
      void unmount(this.#app);
      this.#app = null;
    }
  }

  setConfig(config: CardConfig): void {
    this.#model.config = { ...config };
  }

  set hass(hass: Record<string, unknown>) {
    this.#model.hass = hass;
  }
}

// Guarded: `add_extra_js_url` loads this module once per page, but a browser that
// still holds a cached copy under a different query string would otherwise throw
// on the second definition and take the whole dashboard down with it.
if (!customElements.get(TAG)) {
  customElements.define(TAG, PipewireRouterCard);
}
if (!customElements.get(EDITOR_TAG)) {
  customElements.define(EDITOR_TAG, PipewireRouterCardEditor);
}

interface CustomCardEntry {
  type: string;
  name: string;
  description?: string;
  preview?: boolean;
  documentationURL?: string;
}
const registry: CustomCardEntry[] = ((window as unknown as { customCards?: CustomCardEntry[] }).customCards ??= []);
if (!registry.some((c) => c.type === TAG)) {
  registry.push({
    type: TAG,
    name: 'PipeWire Audio Routing',
    description: 'All audio routing at a glance — tap an input, then where it should play.',
    // The picker renders a live card: it is the real graph, and reading it is
    // harmless (nothing is routed until something is tapped).
    preview: true,
    documentationURL: 'https://github.com/davidgraeff/homeassistant-audio-routing',
  });
}
