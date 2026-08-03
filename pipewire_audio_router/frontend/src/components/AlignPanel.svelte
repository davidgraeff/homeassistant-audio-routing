<script lang="ts">
  // The alignment slice for one input source, rendered inside its card on the
  // Sources page (collapsed or expanded). It shows the speakers currently
  // playing this source — i.e. its sync group — and either starts an alignment
  // session for them or, while one is running, the by-ear tuning controls.
  //
  // Session state is global (one at a time, server-owned): see lib/align.svelte.
  import { align, sliderMax } from '../lib/align.svelte';
  import { routing } from '../lib/routing';

  interface Props {
    /** The source's stable routing node name (SourceView.node_name). */
    sourceNodeName: string;
  }
  let { sourceNodeName }: Props = $props();

  const group = $derived(align.groupForSource(sourceNodeName));
  const active = $derived(!!group && align.isActive(group));
  const blocked = $derived(!!group && align.isBlocked(group));
  const members = $derived(group?.members ?? []);

  // Friendly names from the routing matrix (falls back to the raw node name).
  function label(nodeName: string): string {
    const all = [...$routing.matrix.outputs, ...$routing.matrix.sources];
    return all.find((n) => n.node_name === nodeName)?.display_name ?? nodeName;
  }

  // Other sources feeding the same speakers: then this sync group isn't "mine"
  // alone, and aligning it affects what they play too.
  const coSources = $derived((group?.sources ?? []).filter((s) => s !== sourceNodeName));
</script>

<div class="align-block" class:running={active}>
  {#if align.loading}
    <div class="sync-row"><span class="sync-label">Playing on</span><span class="muted none">…</span></div>
  {:else if !group || members.length === 0}
    <div class="sync-row">
      <span class="sync-label">Playing on</span>
      <span class="muted none">no speaker right now — route this source to speakers on the Music groups page</span>
    </div>
  {:else if !active}
    <div class="sync-row">
      <span class="sync-label">Playing on</span>
      <span class="spk-list">
        {#each members as m (m.node_name)}
          <span class="spk">{label(m.node_name)}</span>
        {/each}
      </span>
      {#if coSources.length}
        <span class="muted co" title="These speakers are also fed by another source, so they share one clock with it">
          + {coSources.map(label).join(', ')}
        </span>
      {/if}
      <button
        class="ghost"
        disabled={align.busy || members.length < 2 || blocked}
        title={blocked
          ? 'An alignment session is running for another source — finish it first'
          : members.length < 2
            ? 'Alignment compares two speakers by ear — this source feeds only one'
            : 'Play a click on these speakers and align them by ear'}
        onclick={() => group && align.start(group)}
      >
        {members.length < 2 ? 'Align (needs 2+ speakers)' : 'Align speakers'}
      </button>
    </div>
  {:else}
    {@const session = align.session}
    <div class="sync-row">
      <span class="sync-label on">Aligning</span>
      <span class="spk-list">
        {#each members as m (m.node_name)}
          <span class="spk">{label(m.node_name)}</span>
        {/each}
      </span>
      <button class="danger" onclick={() => align.stop()} disabled={align.busy}>Finish</button>
    </div>

    <p class="explain">
      The <strong>reference</strong> and the speaker you're <strong>tuning</strong> play together; everything else is
      muted. Nudge the tuned speaker until its clicks sit exactly on the reference's, then move to the next speaker.
      Because you can only add delay, make the physically latest speaker the reference.
    </p>

    <div class="field level">
      <label for="align-vol">Playback volume: {align.level}%</label>
      <input
        id="align-vol"
        type="range"
        min="0"
        max="100"
        step="5"
        value={align.level}
        oninput={(e) => align.previewLevel(Number((e.currentTarget as HTMLInputElement).value))}
        onchange={(e) => align.setLevel(Number((e.currentTarget as HTMLInputElement).value))}
      />
      <span class="hint">Applies to the reference and the speaker being tuned.</span>
    </div>

    <table>
      <thead>
        <tr><th>Speaker</th><th>Role</th><th>Offset</th><th></th></tr>
      </thead>
      <tbody>
        {#each session?.members ?? [] as m (m.node_name)}
          {@const isRef = session?.reference === m.node_name}
          {@const isTarget = session?.target === m.node_name}
          <tr class:audible={isRef || isTarget}>
            <td>
              {label(m.node_name)}
              <span class="badge">{m.kind === 'sendspin' ? 'Sendspin' : 'AirPlay 2'}</span>
            </td>
            <td>
              <label class="role">
                <input type="radio" name={`align-ref-${sourceNodeName}`} checked={isRef} onchange={() => align.setReference(m)} /> Reference
              </label>
              {#if isTarget}<span class="badge on">tuning</span>{/if}
            </td>
            <td>
              <div class="offset">
                <input
                  type="range"
                  min="0"
                  max={sliderMax(m)}
                  step={m.kind === 'sendspin' ? 5 : 10}
                  value={align.offsets[m.node_name] ?? 0}
                  disabled={!isTarget}
                  oninput={(e) => align.liveOffset(m, Number((e.currentTarget as HTMLInputElement).value))}
                  onchange={(e) => align.applyOffset(m, Number((e.currentTarget as HTMLInputElement).value))}
                />
                <span class="ms">{align.offsets[m.node_name] ?? 0} ms</span>
              </div>
              {#if isTarget}
                <div class="nudge">
                  <button onclick={() => align.applyOffset(m, (align.offsets[m.node_name] ?? 0) - 10)}>−10</button>
                  <button onclick={() => align.applyOffset(m, (align.offsets[m.node_name] ?? 0) - 1)}>−1</button>
                  <button onclick={() => align.applyOffset(m, (align.offsets[m.node_name] ?? 0) + 1)}>+1</button>
                  <button onclick={() => align.applyOffset(m, (align.offsets[m.node_name] ?? 0) + 10)}>+10</button>
                </div>
                {#if m.kind === 'sendspin' && !align.sendspinDelayLive}
                  <p class="muted warn">
                    Each change reconnects this speaker (the others keep playing) — expect a long gap, tens of seconds,
                    before the click returns from it.
                  </p>
                {/if}
              {/if}
            </td>
            <td style="text-align:right">
              {#if !isRef && !isTarget}
                <button onclick={() => align.tune(m)} disabled={align.busy}>Tune</button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</div>

<style>
  /* Sits under the card header, above (or instead of) the settings form. */
  .align-block {
    margin-top: 10px;
    padding-top: 10px;
    border-top: 1px solid var(--divider-color);
  }
  .align-block.running {
    margin-top: 12px;
    padding: 12px;
    border: 1px solid var(--primary-color);
    border-radius: 8px;
    background: color-mix(in srgb, var(--primary-color) 6%, transparent);
  }
  .sync-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .sync-label {
    font-size: 0.72rem;
    font-weight: 600;
    text-transform: uppercase;
    letter-spacing: 0.04em;
    color: var(--secondary-text-color);
  }
  .sync-label.on {
    color: var(--primary-color);
  }
  .spk-list {
    display: flex;
    flex-wrap: wrap;
    gap: 5px;
    min-width: 0;
    flex: 1 1 auto;
  }
  .spk {
    font-size: 0.8rem;
    padding: 1px 8px;
    border-radius: 999px;
    border: 1px solid var(--divider-color);
    background: var(--input-fill-color);
  }
  .none,
  .co {
    font-size: 0.8rem;
  }
  .sync-row button {
    flex: 0 0 auto;
    padding: 4px 10px;
    font-size: 0.8rem;
  }
  .explain {
    margin: 10px 0 12px;
    font-size: 0.82rem;
    color: var(--secondary-text-color);
  }
  .explain strong {
    color: var(--primary-text-color);
  }
  tr.audible td {
    background: color-mix(in srgb, var(--primary-color) 8%, transparent);
  }
  .role {
    display: inline-flex;
    gap: 6px;
    align-items: center;
    margin: 0;
  }
  .offset {
    display: flex;
    align-items: center;
    gap: 10px;
  }
  .offset input[type='range'] {
    flex: 1;
    min-width: 120px;
  }
  .ms {
    min-width: 64px;
    text-align: right;
    font-variant-numeric: tabular-nums;
  }
  .nudge {
    display: flex;
    gap: 4px;
    margin-top: 6px;
  }
  .nudge button {
    padding: 2px 8px;
  }
  .warn {
    font-size: 0.8rem;
    margin: 6px 0 0;
  }
  .level {
    max-width: 360px;
    margin-bottom: 4px;
  }
  .hint {
    display: block;
    margin-top: 4px;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
</style>
