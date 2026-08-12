<script lang="ts">
  // One of plan §10's cross-checks, as a row.
  //
  // Three states, not two: a check that could not run (`unavailable`) must not
  // read as a pass. And a *failed* check is drawn as blocking when it blocks the
  // write, because §10.2's transitivity failure is exactly the case that must not
  // look like a warning.
  interface Props {
    name: string;
    state: 'pass' | 'fail' | 'unavailable';
    /** Whether failing this check stops the write. */
    blocking?: boolean;
    /** The numbers: worst case against the tolerance. Shown pass or fail — a
     *  passing check with 2.9 ms of 3 ms used up is worth seeing. */
    detail: string;
    /** What this check does *not* prove, or why it could not run. */
    note?: string;
  }
  let { name, state, blocking = false, detail, note }: Props = $props();

  const verdict = $derived(
    state === 'pass' ? 'passed' : state === 'fail' ? (blocking ? 'failed — blocks the write' : 'failed') : 'not available',
  );
</script>

<div class="check" class:pass={state === 'pass'} class:fail={state === 'fail'} class:na={state === 'unavailable'}>
  <div class="head">
    <span class="dot"></span>
    <strong>{name}</strong>
    <span class="verdict">{verdict}</span>
  </div>
  <p class="detail">{detail}</p>
  {#if note}<p class="note">{note}</p>{/if}
</div>

<style>
  .check {
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid var(--divider-color);
    background: color-mix(in srgb, var(--secondary-text-color) 6%, transparent);
  }
  .check.pass {
    border-color: color-mix(in srgb, var(--success-color, #43a047) 45%, transparent);
    background: color-mix(in srgb, var(--success-color, #43a047) 8%, transparent);
  }
  .check.fail {
    border-color: color-mix(in srgb, var(--error-color, #db4437) 55%, transparent);
    background: color-mix(in srgb, var(--error-color, #db4437) 10%, transparent);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    font-size: 0.84rem;
  }
  /* Colour reinforces; the word "passed"/"failed"/"not available" is what carries
     the state, so this survives both themes and colour-vision differences. */
  .dot {
    flex: 0 0 auto;
    width: 8px;
    height: 8px;
    border-radius: 50%;
    background: var(--secondary-text-color);
  }
  .check.pass .dot {
    background: var(--success-color, #43a047);
  }
  .check.fail .dot {
    background: var(--error-color, #db4437);
  }
  .verdict {
    color: var(--secondary-text-color);
  }
  .detail,
  .note {
    margin: 5px 0 0;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  .detail {
    font-variant-numeric: tabular-nums;
  }
</style>
