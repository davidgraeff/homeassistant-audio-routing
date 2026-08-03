<script lang="ts">
  // A group's name, renamed in place: a button that turns into an input (Enter or
  // blur commits, Escape cancels). Shared by the Music-groups and Announcements
  // pages; styling is global (`.gtitle` / `.rename` in app.css).
  interface Props {
    name: string;
    /** Called only with a non-empty name that actually changed. */
    onRename: (name: string) => void;
  }
  let { name, onRename }: Props = $props();

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
    if (next && next !== name) onRename(next);
  }
  function onKey(e: KeyboardEvent) {
    if (e.key === 'Enter') (e.currentTarget as HTMLInputElement).blur();
    else if (e.key === 'Escape') draft = null;
  }
</script>

{#if draft !== null}
  <input class="rename" bind:value={draft} use:focus onblur={commit} onkeydown={onKey} />
{:else}
  <button class="gtitle" onclick={() => (draft = name)} title="Rename group">
    <strong>{name}</strong>
    <svg class="pencil" viewBox="0 0 16 16" aria-hidden="true"><path d="M11.5 1.5l3 3L5 14l-3.5.5L2 11z" /></svg>
  </button>
{/if}
