<script lang="ts">
  // The preset chips: every named grouping of the house, which one is in force,
  // and which one the group cards below are editing (docs/music-group-presets-plan.md
  // §6.2–6.3).
  //
  // Two states on one row of chips, and they are *different things*:
  //   • active   — filled, with a ▶. The grouping the speakers are actually on.
  //   • selected — ringed. What the page below is editing.
  // On load they are the same chip, which is the state that behaves exactly like
  // the page did before presets existed. Selecting another chip changes only what
  // is edited: switching the house over is the explicit Activate button, because
  // building Friday's party layout must not start playing it.
  import { askConfirm } from '../../lib/confirm.svelte';
  import { DEFAULT_PRESET_ID, type Preset } from '../../lib/types';
  import GroupTitle from './GroupTitle.svelte';

  interface Props {
    presets: Preset[];
    /** Id of the preset in force. */
    active: string;
    /** Id of the preset being edited. */
    selected: string;
    onSelect: (id: string) => void;
    onActivate: (id: string) => void;
    /** `copyFrom` omitted ⇒ start empty. */
    onCreate: (name: string, copyFrom?: string) => void;
    onRename: (id: string, name: string) => void;
    onDelete: (id: string) => void;
  }
  let { presets, active, selected, onSelect, onActivate, onCreate, onRename, onDelete }: Props = $props();

  let creating = $state(false);
  let draft = $state('');
  // The common case by far is "the current grouping, but…", so a new preset copies
  // the one being edited unless this is ticked.
  let startEmpty = $state(false);

  let selectedPreset = $derived(presets.find((p) => p.id === selected) ?? null);
  let activeName = $derived(presets.find((p) => p.id === active)?.name ?? active);
  let editingActive = $derived(selected === active);

  function focus(node: HTMLInputElement) {
    node.focus();
  }

  function openForm() {
    draft = '';
    startEmpty = false;
    creating = true;
  }

  function submit() {
    const name = draft.trim();
    if (!name) {
      creating = false;
      return;
    }
    onCreate(name, startEmpty ? undefined : selected);
    creating = false;
  }

  async function remove(p: Preset) {
    const ok = await askConfirm({
      title: `Delete the preset '${p.name}'?`,
      body: [
        'The grouping it holds is lost. Your music groups themselves are kept — with them, their Home Assistant media players.',
        p.id === active ? 'It is the preset in force, so the house goes back to the default grouping.' : '',
      ].filter(Boolean),
      confirmLabel: 'Delete',
      danger: true,
    });
    if (ok) onDelete(p.id);
  }
</script>

<div class="card">
  <div class="bar">
    {#if creating}
      <form
        class="newform"
        onsubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        <input
          use:focus
          type="text"
          placeholder="Preset name…"
          bind:value={draft}
          onkeydown={(e) => {
            if (e.key === 'Escape') creating = false;
          }}
        />
        <label class="check" title="Otherwise the new preset starts as a copy of the one you are editing">
          <input type="checkbox" bind:checked={startEmpty} />
          Start empty
        </label>
        <button type="submit">Create</button>
        <button class="ghost" type="button" onclick={() => (creating = false)}>Cancel</button>
      </form>
    {:else}
      <button class="new" type="button" onclick={openForm}>+ New preset</button>
      {#each presets as p (p.id)}
        <span class="pchip" class:active={p.id === active} class:selected={p.id === selected}>
          <button
            class="pick"
            type="button"
            title={p.id === active ? 'In force — click to edit it' : 'Edit this preset'}
            aria-pressed={p.id === selected}
            onclick={() => onSelect(p.id)}
          >
            {#if p.id === active}<span class="marker" aria-label="in force">▶</span>{/if}
            {p.name}
          </button>
          {#if p.id !== DEFAULT_PRESET_ID}
            <button class="x" type="button" title={`Delete '${p.name}'`} onclick={() => remove(p)}>✕</button>
          {/if}
        </span>
      {/each}
    {/if}
  </div>

  <!-- Which preset the page below is about, and — when that is not the one in
       force — the only way to put it in force. -->
  <div class="banner" class:editing={!editingActive}>
    {#if selectedPreset}
      <span class="lead">Editing</span>
      <GroupTitle name={selectedPreset.name} title="Rename preset" onRename={(name) => onRename(selectedPreset.id, name)} />
      {#if editingActive}
        <span class="state">— in force</span>
      {:else}
        <span class="state warn">— not active ('{activeName}' is playing)</span>
        <button type="button" onclick={() => onActivate(selectedPreset.id)}>Activate</button>
      {/if}
    {/if}
  </div>
  {#if !editingActive}
    <p class="hintline">
      Membership only: a preset that is not in force has no speakers to route, so <strong>Source</strong> and the mixed-routing
      warning are hidden until you activate it.
    </p>
  {/if}
</div>

<style>
  .bar {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
  }
  .newform {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 8px;
  }
  .newform input[type='text'] {
    min-width: 12rem;
  }
  .new {
    border-style: dashed;
  }
  /* A preset chip is a control, not a draggable device chip (.chip in app.css) —
     hence its own class rather than a variant of that one. */
  .pchip {
    display: inline-flex;
    align-items: stretch;
    border-radius: 999px;
    border: 1px solid rgba(127, 127, 127, 0.35);
    background: var(--card-background-color, #fff);
    overflow: hidden;
  }
  .pchip.selected {
    box-shadow: 0 0 0 2px var(--primary-color, #03a9f4);
    border-color: var(--primary-color, #03a9f4);
  }
  .pchip.active {
    background: var(--primary-color, #03a9f4);
    border-color: var(--primary-color, #03a9f4);
    color: var(--text-primary-color, #fff);
  }
  .pchip button {
    border: 0;
    border-radius: 0;
    background: transparent;
    color: inherit;
    font-size: 0.85rem;
    padding: 5px 11px;
    cursor: pointer;
  }
  .pchip button:hover {
    background: rgba(127, 127, 127, 0.18);
  }
  .marker {
    font-size: 0.7rem;
    margin-right: 4px;
    opacity: 0.9;
  }
  .pchip .x {
    padding: 5px 9px 5px 5px;
    opacity: 0.65;
  }
  .pchip .x:hover {
    opacity: 1;
    color: var(--error-color, #db4437);
  }
  .banner {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 8px;
    margin-top: 10px;
  }
  /* `.gtitle` grows to fill a group card's header row (app.css); here it sits in a
     sentence, so it takes only the width of the name and "— in force" stays next
     to it instead of being flung to the far edge. */
  .banner :global(.gtitle) {
    flex: 0 1 auto;
  }
  .lead {
    opacity: 0.7;
    font-size: 0.85rem;
  }
  .state {
    font-size: 0.85rem;
    opacity: 0.7;
  }
  .state.warn {
    color: var(--warning-color, #f0a202);
    opacity: 1;
    font-weight: 600;
  }
  .banner.editing {
    padding: 8px 10px;
    margin-top: 8px;
    border-radius: 8px;
    border: 1px dashed color-mix(in srgb, var(--warning-color, #f0a202) 45%, transparent);
    background: color-mix(in srgb, var(--warning-color, #f0a202) 7%, transparent);
  }
  .hintline {
    margin: 6px 0 0;
    font-size: 0.8rem;
    opacity: 0.75;
  }
</style>
