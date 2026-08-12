<script lang="ts">
  // A name, renamed in place: a button that turns into an input (Enter or blur
  // commits, Escape cancels). Shared by the Music-groups, Announcements and
  // Outputs pages; styling is global (`.gtitle` / `.rename` in app.css).
  import { toast } from '../../lib/toast';

  interface Props {
    name: string;
    /** Called only with a name that passed `minLength` and actually changed. */
    onRename: (name: string) => void;
    /** Shortest accepted name, after trimming. A shorter one is refused with a
     *  toast rather than silently dropped — an outright empty box still just
     *  cancels, which is how you back out of an edit. */
    minLength?: number;
    /** Tooltip on the pencil button — what this names ("Rename group"/"…output"). */
    title?: string;
    /** Drop the name and fall back to whatever the thing is called by default.
     *  Omit it — as a group does, having no name but the one it was given — and
     *  no clear control appears; the caller should also omit it when there is
     *  nothing to clear, so the icon never offers a no-op.
     *
     *  Fires on the click, unasked: the caller still holds the old name, so this
     *  is the caller's to offer as an Undo (which is what it used to ask about). */
    onReset?: () => void;
    /** Tooltip on the clear button. Name the default there when the caller knows
     *  it — it isn't shown anywhere else, and then the click isn't a blind one. */
    resetTitle?: string;
  }
  let { name, onRename, minLength = 1, title = 'Rename group', onReset, resetTitle = 'Use the default name again' }: Props =
    $props();

  // null = not editing; a string = the draft being typed.
  let draft = $state<string | null>(null);

  function focus(node: HTMLInputElement) {
    node.focus();
    node.select();
  }
  function commit() {
    if (draft === null) return; // Escape already cancelled (blur still fires)
    const next = draft.trim();
    draft = null;
    if (!next || next === name) return;
    if (next.length < minLength) {
      toast('error', `A name needs at least ${minLength} characters — kept "${name}"`);
      return;
    }
    onRename(next);
  }
  function onKey(e: KeyboardEvent) {
    if (e.key === 'Enter') (e.currentTarget as HTMLInputElement).blur();
    else if (e.key === 'Escape') draft = null;
  }
</script>

{#if draft !== null}
  <input class="rename" bind:value={draft} use:focus onblur={commit} onkeydown={onKey} />
{:else}
  <button class="gtitle" onclick={() => (draft = name)} {title}>
    <strong>{name}</strong>
    <svg class="pencil" viewBox="0 0 16 16" aria-hidden="true"><path d="M11.5 1.5l3 3L5 14l-3.5.5L2 11z" /></svg>
  </button>
  <!-- Emitted only when the caller passes `onReset`, so a group title is exactly
       the single element it has always been. -->
  {#if onReset}
    <button class="gclear" type="button" aria-label={resetTitle} title={resetTitle} onclick={() => onReset?.()}>
      <svg class="x" viewBox="0 0 16 16" aria-hidden="true"><path d="M4.5 4.5l7 7M11.5 4.5l-7 7" /></svg>
    </button>
  {/if}
{/if}
