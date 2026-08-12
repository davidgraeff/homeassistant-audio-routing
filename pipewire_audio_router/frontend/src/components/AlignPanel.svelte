<script lang="ts">
  // The **by-ear** alignment slice for one input source, rendered inside its card on the
  // Sources page (collapsed or expanded). It shows the speakers currently playing this
  // source — i.e. its sync group — and either starts a by-ear session for them or, while
  // one is running, the tuning sliders. It also keeps the revert offer for a measured run
  // (plan §9.4), which is why it watches the measurement status it no longer starts.
  //
  // **The microphone wizard used to open from here and deliberately does not any more**
  // (plan §12.1, W6c). Two reasons, and the second is the decisive one:
  //
  //   * a wizard run does not align "this source's speakers": the user picks an arbitrary
  //     set of outputs and the daemon forms a temporary group around them (§12.1). A
  //     source card is the wrong frame for that choice — it was only ever a *seed* — and
  //     the set being picked lives on the Outputs page, where it now is;
  //   * **there is exactly one alignment session, process-wide.** Two live entry points
  //     into it means two places that can each believe they own it: the source card's
  //     "Measure" button would happily open a second wizard over a session started from
  //     the Outputs page, and the wizard's own model of "is this hold mine?" is its
  //     *selection*, which a source card cannot see. So the second entry point is gone
  //     rather than guarded — a guard would still have to be right about which page won.
  //
  // What is left here cannot start a competing session either: the by-ear button is
  // disabled whenever any session is running that is not this group's (`align.isBlocked`),
  // and a wizard session's identity is its selected *outputs*, which can never equal a
  // group's *source* set — so a running wizard always reads as blocking here.
  import { align, knobNoun, memberKindLabel, sliderMax, sliderMin } from '../lib/align.svelte';
  import { measure } from '../lib/measure.svelte';
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

  // The measurement run's status, polled by the store. Wanted here and not only
  // inside the wizard: a run that wrote delays stays revertable after the wizard is
  // closed and the session finished (plan §9.4), and the offer to revert has to be
  // somewhere the user can find it.
  $effect(() => measure.attach());
  // Overlap, not an exact set match: a wizard run is scoped to a *selection of
  // speakers* (plan §12.3.1), so its `revert_scope` will rarely equal any one source
  // group's set — but a delay written to a speaker this source plays through is
  // still this card's business, and the undo has to be reachable from where the user
  // notices the problem.
  const revertable = $derived(measure.canRevert && measure.revertTouches(members.map((m) => m.node_name)));

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
          ? 'Another alignment is running — either another source’s by-ear session, or a microphone run started from the Outputs page. Finish that one first: there is only ever one at a time.'
          : members.length < 2
            ? 'Alignment compares two speakers by ear — this source feeds only one'
            : 'Play a click on these speakers and align them by ear'}
        onclick={() => group && align.start(group)}
      >
        {members.length < 2 ? 'Align (needs 2+ speakers)' : 'Align speakers'}
      </button>
    </div>
    <!-- The wizard moved, so say where it went: a button that simply disappeared is
         indistinguishable from a feature that was removed. -->
    <p class="hint mic">
      Measuring the delays with a phone instead of judging them by ear now lives on the <strong>Outputs</strong> page —
      it picks its own set of speakers rather than a source's, so it belongs where the speakers are.
    </p>
  {:else}
    <div class="sync-row">
      <span class="sync-label on">Aligning</span>
      <span class="spk-list">
        {#each members as m (m.node_name)}
          <span class="spk">{label(m.node_name)}</span>
        {/each}
      </span>
      <button class="danger" onclick={() => align.stop()} disabled={align.busy}>Finish</button>
    </div>
  {/if}

  {#if active}
    {@const session = align.session}
    <p class="explain">
      The <strong>reference</strong> and the speaker you're <strong>tuning</strong> play together; everything else is
      muted. Nudge the tuned speaker until its clicks sit exactly on the reference's, then move to the next speaker.
      Each knob only moves its speaker one way, and the two kinds go opposite ways: a <strong>Sendspin</strong> knob is
      an <em>advance</em> (higher = plays earlier), an <strong>AirPlay 2</strong> or <strong>PipeWire host</strong> knob
      is a delay (higher = plays later). So reference the earliest speaker in a Sendspin group and the latest one in an
      AirPlay group.
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
        <tr><th>Speaker</th><th>Role</th><th>Advance / delay</th><th></th></tr>
      </thead>
      <tbody>
        {#each session?.members ?? [] as m (m.node_name)}
          {@const isRef = session?.reference === m.node_name}
          {@const isTarget = session?.target === m.node_name}
          <tr class:audible={isRef || isTarget}>
            <td>
              {label(m.node_name)}
              <span class="badge">{memberKindLabel(m.kind)}</span>
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
                  min={sliderMin(m)}
                  max={sliderMax(m)}
                  step={m.kind === 'sendspin' ? 5 : 10}
                  value={align.offsets[m.node_name] ?? 0}
                  disabled={!isTarget}
                  oninput={(e) => align.liveOffset(m, Number((e.currentTarget as HTMLInputElement).value))}
                  onchange={(e) => align.applyOffset(m, Number((e.currentTarget as HTMLInputElement).value))}
                />
                <!-- The number is meaningless without the noun: the same 40 ms is
                     "plays 40 ms earlier" on a Sendspin speaker and "40 ms later" on
                     an AirPlay 2 one. -->
                <span class="ms">{align.offsets[m.node_name] ?? 0} ms {knobNoun(m.kind)}</span>
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

  <!-- Plan §9.4: the write is destructive to a previously-tuned setup, so one
       click has to undo it — and it has to still be here after the wizard is closed
       and the session finished, which is exactly when the user decides they
       preferred it the way it was. -->
  {#if revertable}
    <div class="revert">
      <!-- Names the *whole* scope, not just this card's speakers: a wizard run is
           scoped to a selection, and Revert restores the entire snapshot it took — so
           it can put back speakers this source never plays to. Saying "these
           speakers" while doing more than that is the kind of small lie that makes an
           undo untrustworthy. -->
      <span>A measurement wrote delays to {measure.revertScope.map(label).join(', ')}.</span>
      <button class="ghost" disabled={measure.busy} onclick={() => void measure.revert()}>
        Revert to the delays from before
      </button>
      <span class="hint">
        Every one of them goes back to the delay it had before that run, and each speaker whose delay changes reconnects —
        so expect another quiet gap.
      </span>
    </div>
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
  .hint.mic {
    margin: 6px 0 0;
  }
  .hint.mic strong {
    color: var(--primary-text-color);
  }
  .revert {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 10px;
    padding: 8px 10px;
    border-radius: 6px;
    border: 1px solid color-mix(in srgb, var(--warning-color, #ffa600) 55%, transparent);
    background: color-mix(in srgb, var(--warning-color, #ffa600) 10%, transparent);
    font-size: 0.82rem;
  }
  .revert button {
    padding: 4px 10px;
    font-size: 0.8rem;
  }
  .revert .hint {
    display: inline;
    margin: 0;
  }
</style>
