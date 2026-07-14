<script lang="ts">
  import { routing } from '../lib/routing';
  import { api } from '../lib/api';
  import { run } from '../lib/toast';

  let target = $state<number | null>(null);
  let mode = $state<'url' | 'wyoming'>('url');
  let duck = $state(0.25);
  let busy = $state(false);

  // url mode
  let url = $state('');
  // wyoming mode
  let host = $state('');
  let port = $state<number | ''>('');
  let text = $state('');
  let voice = $state('');

  // Announce is node-id based (the daemon ducks + plays into a live sink), so
  // only outputs with a live node_id are valid targets — this excludes virtual
  // sendspin devices and offline outputs (both node_id: null), which also keeps
  // the <option> list keyed uniquely (several share node_id null).
  const announceable = $derived($routing.matrix.outputs.filter((o) => o.node_id != null));

  // Default the target to the first announceable output once the matrix loads.
  $effect(() => {
    if ((target === null || !announceable.some((o) => o.node_id === target)) && announceable.length > 0) {
      target = announceable[0].node_id;
    }
  });

  async function send(e: Event) {
    e.preventDefault();
    if (target === null) return;
    busy = true;
    if (mode === 'url') {
      await run(() => api.announceUrl(target!, url.trim(), duck), 'Announcement played');
    } else {
      await run(
        () =>
          api.announceWyoming(
            target!,
            { host: host.trim(), port: port === '' ? undefined : Number(port), text: text.trim(), voice: voice.trim() || null },
            duck,
          ),
        'Announcement played',
      );
    }
    busy = false;
  }
</script>

<div class="card">
  <h2>Announce test</h2>
  <p class="card-sub">
    Ducks every source currently feeding the chosen output, plays a clip, then restores volumes. The
    request blocks until playback finishes.
  </p>

  {#if announceable.length === 0}
    <p class="empty">No outputs available to announce to.</p>
  {:else}
    <form onsubmit={send}>
      <div class="field">
        <label for="an-target">Output</label>
        <select id="an-target" bind:value={target}>
          {#each announceable as o (o.node_name)}
            <option value={o.node_id}>{o.display_name}</option>
          {/each}
        </select>
      </div>

      <div class="field">
        <span class="group-label">Source</span>
        <div class="row" style="gap:16px">
          <label style="display:flex; gap:6px; align-items:center; margin:0">
            <input type="radio" value="url" bind:group={mode} /> URL (fetched + decoded)
          </label>
          <label style="display:flex; gap:6px; align-items:center; margin:0">
            <input type="radio" value="wyoming" bind:group={mode} /> Wyoming TTS
          </label>
        </div>
      </div>

      {#if mode === 'url'}
        <div class="field">
          <label for="an-url">Audio URL</label>
          <input id="an-url" type="url" bind:value={url} placeholder="http://homeassistant.local:8123/api/tts_proxy/….mp3" />
        </div>
      {:else}
        <div class="row">
          <div class="grow field">
            <label for="an-host">Wyoming host</label>
            <input id="an-host" type="text" bind:value={host} placeholder="192.168.1.20" />
          </div>
          <div class="field" style="flex:0 0 100px">
            <label for="an-port">Port</label>
            <input id="an-port" type="number" bind:value={port} placeholder="10200" />
          </div>
          <div class="field" style="flex:0 0 140px">
            <label for="an-voice">Voice (optional)</label>
            <input id="an-voice" type="text" bind:value={voice} placeholder="default" />
          </div>
        </div>
        <div class="field">
          <label for="an-text">Text</label>
          <textarea id="an-text" rows="2" bind:value={text} placeholder="Front door opened"></textarea>
        </div>
      {/if}

      <div class="field">
        <label for="an-duck">Duck volume: {duck.toFixed(2)}</label>
        <input id="an-duck" type="range" min="0" max="1" step="0.05" bind:value={duck} />
      </div>

      <button class="primary" type="submit" disabled={busy || target === null}>
        {busy ? 'Announcing…' : 'Send announcement'}
      </button>
    </form>
  {/if}
</div>
