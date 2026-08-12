<script lang="ts">
  // The **by-ear** body of the wizard: plan §1's third mode, and the documented fallback
  // for a microphone that cannot be used (§4.1) or an estimator that refuses (§5.5).
  //
  // This is what used to sit on every source card under `AlignPanel`. Two things changed
  // when it moved here (plan §12.1), and both are the point rather than incidental:
  //
  //   * **its group is a selection of speakers, not a source set.** A source card could
  //     only ever offer "the speakers currently playing me", which is not the set anyone
  //     wants to align — and it meant two live entry points into the one session that
  //     exists process-wide. Now by-ear forms the same temporary exclusive hold as the
  //     measured modes (§12.3.1), so there is one way in and one thing to stop;
  //   * **nothing here needs a microphone**, which is the whole reason the mode exists.
  //     The wizard does not mount the capture for this mode at all.
  //
  // What it does *not* change: the knobs. A sendspin knob is an **advance** and an
  // AirPlay-2 / PipeWire-host knob is a **delay** (plan §2.4.1), so every number is
  // printed with its noun — the same 40 ms means opposite things on two rows of this
  // table. And unlike a measured run, a by-ear change is written to the device
  // immediately: there is no proposal to approve, so each sendspin nudge costs that one
  // speaker a reconnect unless its firmware applies delay live.
  import { align, knobNoun, memberKindLabel, sliderMax, sliderMin } from '../../lib/align.svelte';

  interface Props {
    label: (nodeName: string) => string;
  }
  let { label }: Props = $props();

  const session = $derived(align.session);
  const members = $derived(session?.members ?? []);
</script>

{#if !session?.active}
  <p class="empty">
    No speakers are being held. Go back to <strong>Speakers</strong>, pick the ones you want to tune, and take them for
    the alignment.
  </p>
{:else}
  <p class="explain">
    The <strong>reference</strong> and the speaker you're <strong>tuning</strong> play together; everything else is muted.
    Nudge the tuned speaker until its clicks sit exactly on the reference's, then move to the next speaker. Each knob only
    moves its speaker one way, and the two kinds go opposite ways: a <strong>Sendspin</strong> knob is an
    <em>advance</em> (higher = plays earlier), an <strong>AirPlay 2</strong> or <strong>PipeWire host</strong> knob is a
    delay (higher = plays later). So reference the earliest speaker in a Sendspin group and the latest one in an AirPlay
    group.
  </p>
  <p class="hint">
    Stand where you actually listen: sound covers about 34 cm per millisecond, so a 3 m difference in your distance to two
    speakers is already ~9 ms of the very thing you are tuning. The clicks alternate high/low a second apart, which is how
    you can tell "lined up" from "a whole click out" — a speaker that has slipped a full interval lands its high click on
    the reference's low one.
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
      {#each members as m (m.node_name)}
        {@const isRef = session?.reference === m.node_name}
        {@const isTarget = session?.target === m.node_name}
        <tr class:audible={isRef || isTarget}>
          <td>
            {label(m.node_name)}
            <span class="badge">{memberKindLabel(m.kind)}</span>
          </td>
          <td>
            <label class="role">
              <input type="radio" name="align-reference" checked={isRef} onchange={() => align.setReference(m)} /> Reference
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

  <!-- By-ear has no proposal step, so this is the one place it can be said: unlike a
       measured run, these values are already on the devices. -->
  <p class="hint written">
    <strong>These changes are already written.</strong> Every nudge goes straight to that speaker's own setting and is
    persisted, so there is nothing to approve here — and nothing this page can undo either. <strong>Stop and restore</strong>
    gives the speakers back and puts levels, mutes and routing right, but it leaves the timing you tuned exactly as you
    left it.
  </p>
{/if}

<style>
  .explain {
    margin: 0 0 8px;
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
    margin: 10px 0 4px;
  }
  .muted {
    color: var(--secondary-text-color);
  }
  .empty {
    font-size: 0.84rem;
    color: var(--secondary-text-color);
    font-style: italic;
  }
  .hint {
    display: block;
    margin-top: 6px;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  .hint strong {
    color: var(--primary-text-color);
  }
  .hint.written {
    margin-top: 12px;
  }
</style>
