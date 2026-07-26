// Pointer-based drag-and-drop that works with mouse, touch, and pen.
//
// Native HTML5 drag events don't fire on touchscreens, and this admin console is
// used from Home Assistant dashboards on tablets and phones. So we track pointer
// events ourselves: a small move threshold separates a tap from a drag, and on
// release we hit-test the element under the pointer for the nearest
// `[data-dropzone]`.
//
// The payload is opaque to the helper — the caller decides what a drop means.
// Drop zones are any DOM element carrying `data-dropzone="<id>"`; the id under
// the pointer (or null) is handed to `onDrop` on release and exposed as `hover`
// while dragging so the caller can highlight the target.

const DRAG_THRESHOLD_PX = 6;

export interface DndController<T> {
  /** True once the pointer has moved past the tap/drag threshold. */
  readonly active: boolean;
  /** The payload of the in-flight drag, or null. */
  readonly payload: T | null;
  /** Human-readable label for the drag ghost. */
  readonly label: string;
  /** `data-dropzone` id currently under the pointer, or null. */
  readonly hover: string | null;
  /** Live pointer position (viewport coords) for the ghost. */
  readonly x: number;
  readonly y: number;
  /** Wire to a draggable element's `onpointerdown`. */
  begin(e: PointerEvent, payload: T, label: string): void;
  /** Wire to `<svelte:window onpointermove>`. */
  move(e: PointerEvent): void;
  /** Wire to `<svelte:window onpointerup>` and `onpointercancel`. */
  end(e: PointerEvent): void;
}

function zoneAt(x: number, y: number): string | null {
  const el = document.elementFromPoint(x, y);
  const zone = el?.closest('[data-dropzone]') as HTMLElement | null;
  return zone?.getAttribute('data-dropzone') ?? null;
}

export function createDnd<T>(onDrop: (payload: T, zoneId: string | null) => void): DndController<T> {
  const s = $state<{ active: boolean; payload: T | null; label: string; hover: string | null; x: number; y: number }>({
    active: false,
    payload: null,
    label: '',
    hover: null,
    x: 0,
    y: 0,
  });
  // Pending press that hasn't crossed the threshold yet — not a drag until it does.
  let pending: { payload: T; label: string; startX: number; startY: number } | null = null;

  function reset() {
    pending = null;
    s.active = false;
    s.payload = null;
    s.hover = null;
  }

  return {
    get active() {
      return s.active;
    },
    get payload() {
      return s.payload;
    },
    get label() {
      return s.label;
    },
    get hover() {
      return s.hover;
    },
    get x() {
      return s.x;
    },
    get y() {
      return s.y;
    },

    begin(e, payload, label) {
      if (e.button > 0) return; // left button / touch / pen only
      pending = { payload, label, startX: e.clientX, startY: e.clientY };
    },

    move(e) {
      if (!pending) return;
      if (!s.active) {
        if (Math.hypot(e.clientX - pending.startX, e.clientY - pending.startY) < DRAG_THRESHOLD_PX) return;
        s.active = true;
        s.payload = pending.payload;
        s.label = pending.label;
      }
      s.x = e.clientX;
      s.y = e.clientY;
      s.hover = zoneAt(e.clientX, e.clientY);
      e.preventDefault(); // suppress text selection / scroll while dragging
    },

    end(e) {
      const p = pending;
      const dragged = s.active;
      if (!p || !dragged) {
        reset();
        return;
      }
      const zone = zoneAt(e.clientX, e.clientY);
      reset();
      onDrop(p.payload, zone);
    },
  };
}
