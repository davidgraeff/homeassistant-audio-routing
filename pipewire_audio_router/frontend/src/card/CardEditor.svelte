<script lang="ts">
  import type { EditorModel } from './editor.svelte';
  import { DEFAULT_TITLE, type CardConfig } from './types';

  // The card's visual editor, built on Home Assistant's own `<ha-form>` rather
  // than hand-rolled inputs: it renders the same switches and text fields as
  // every built-in card's editor, in the user's language and with the mobile
  // behaviour already right. The card has four options, so a declarative schema
  // is the whole editor.
  //
  // `ha-form` is part of the frontend bundle, not something we ship. It is
  // reliably defined by the time the card-editor dialog opens (the dialog itself
  // uses it), and `getConfigElement` nudges it in besides — but if it somehow
  // isn't, say so instead of showing an empty pane.

  interface Props {
    model: EditorModel;
  }
  let { model }: Props = $props();

  const DOMAIN = 'pipewire_audio_router';

  /** Form values: the config with its defaults filled in, so a switch that has
   *  never been touched shows the state the card is actually in. */
  const data = $derived({
    show_title: model.config.show_title !== false,
    title: model.config.title ?? '',
    show_hint: model.config.show_hint !== false,
    entry_id: model.config.entry_id ?? '',
  });

  const schema = $derived([
    { name: 'show_title', selector: { boolean: {} } },
    // Only worth asking for a heading when there will be one.
    ...(data.show_title ? [{ name: 'title', selector: { text: {} } }] : []),
    { name: 'show_hint', selector: { boolean: {} } },
    // A picker over this integration's config entries, so the value is a real
    // entry id and not a hand-copied string.
    { name: 'entry_id', selector: { config_entry: { integration: DOMAIN } } },
  ]);

  const LABELS: Record<string, string> = {
    show_title: 'Show title',
    title: 'Title',
    show_hint: 'Show instructions',
    entry_id: 'Router',
  };
  const HELPERS: Record<string, string> = {
    title: `Leave empty for “${DEFAULT_TITLE}”.`,
    show_hint: 'The line under the graph explaining how to route. It reappears by itself while an input is held.',
    entry_id: 'Only needed if you have more than one PipeWire Audio Router configured.',
  };

  const computeLabel = (s: { name: string }) => LABELS[s.name] ?? s.name;
  const computeHelper = (s: { name: string }) => HELPERS[s.name] ?? '';

  /** Write back only what differs from the defaults, so the YAML stays the short
   *  thing a user can read — and `type` always survives, since Lovelace replaces
   *  the config wholesale with what we emit. */
  function apply(value: Record<string, unknown>) {
    const next: CardConfig = { type: model.config.type ?? 'custom:pipewire-router-card' };
    if (value.show_title === false) next.show_title = false;
    if (typeof value.title === 'string' && value.title !== '') next.title = value.title;
    if (value.show_hint === false) next.show_hint = false;
    if (typeof value.entry_id === 'string' && value.entry_id !== '') next.entry_id = value.entry_id;
    model.onChange(next);
  }

  let form = $state<HTMLElement>();
  const haFormMissing = $derived(!!model.hass && !customElements.get('ha-form'));

  // `ha-form` takes objects and functions as *properties*; setting them as
  // attributes (which is what markup would do) would stringify them. The
  // `value-changed` listener is attached here for the same reason: one place that
  // owns the element's imperative surface.
  $effect(() => {
    const el = form as (HTMLElement & Record<string, unknown>) | undefined;
    if (!el) return;
    el.hass = model.hass;
    el.schema = schema;
    el.data = data;
    el.computeLabel = computeLabel;
    el.computeHelper = computeHelper;
  });

  $effect(() => {
    const el = form;
    if (!el) return;
    const onChanged = (ev: Event) => {
      const value = (ev as CustomEvent<{ value?: Record<string, unknown> }>).detail?.value;
      if (value) apply(value);
    };
    el.addEventListener('value-changed', onChanged);
    return () => el.removeEventListener('value-changed', onChanged);
  });
</script>

{#if haFormMissing}
  <p class="fallback">
    Home Assistant's form components aren't available here — use the code (YAML) editor for this card.
  </p>
{:else if model.hass}
  <ha-form bind:this={form}></ha-form>
{/if}

<style>
  .fallback {
    color: var(--secondary-text-color);
    font-size: 14px;
    margin: 8px 0;
  }
</style>
