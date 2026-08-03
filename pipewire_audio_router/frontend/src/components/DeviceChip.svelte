<script lang="ts">
  // One draggable speaker chip in the group editor (Music groups + Announcements
  // pages). Styling is global (`.chip` in app.css) so both pages render the same
  // object; the drag gesture itself is owned by the caller's dnd controller —
  // this component only forwards the pointer-down that starts it.
  interface Props {
    label: string;
    onDragStart: (e: PointerEvent) => void;
  }
  let { label, onDragStart }: Props = $props();
</script>

<span class="chip" role="button" tabindex="0" title={label} onpointerdown={onDragStart}>
  <svg class="grip" viewBox="0 0 16 16" aria-hidden="true"><circle cx="5" cy="4" r="1.3" /><circle cx="11" cy="4" r="1.3" /><circle cx="5" cy="8" r="1.3" /><circle cx="11" cy="8" r="1.3" /><circle cx="5" cy="12" r="1.3" /><circle cx="11" cy="12" r="1.3" /></svg>
  {label}
</span>
