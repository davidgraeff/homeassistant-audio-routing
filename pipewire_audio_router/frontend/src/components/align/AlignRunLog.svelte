<script lang="ts">
  // "Download the run log" — the persisted per-run transcript
  // (`GET /api/align/measure/log`, plan §11).
  //
  // Why it exists: a run that ends badly ends *silently* as far as the file system is
  // concerned, and the interesting evidence — which gate acquisitions were thrown away,
  // which member kept going quiet, how long a reconnect actually took — has scrolled past
  // by the time anyone wants it. So the daemon records a transcript per run and this hands
  // the file to a person. Nothing here parses it: a transcript is forensic, and a UI that
  // read numbers out of it would become a second, quieter report that can disagree with the
  // review page beside it.
  //
  // Two honesty constraints shape the wording, both from the API rather than from taste:
  //
  //   * **`MeasureStatus` carries no transcript id**, so the newest stored run is the best
  //     this can offer. It is named — id and start time — so a reader can tell whether it is
  //     the run they are looking at, instead of being told "this run's log" and having to
  //     trust it;
  //   * **the listing is read first.** A daemon with nowhere to write transcripts records
  //     none, and an empty listing is a real state — so it is reported as one rather than
  //     left as a button that fails when pressed.
  import { api, refusalOf } from '../../lib/api';
  import type { MeasureLogList } from '../../lib/types';

  interface Props {
    /** Why the log is being offered here, when it is worth saying. A refused run is the
     *  case the transcript exists for, so that placement says so. */
    reason?: string | null;
  }
  let { reason = null }: Props = $props();

  let list = $state<MeasureLogList | null>(null);
  /** Set when the listing itself could not be read. Distinct from "no runs stored", which
   *  is a legitimate answer and not an error. */
  let listError = $state<string | null>(null);
  let busy = $state(false);
  let problem = $state<string | null>(null);

  const newest = $derived(list?.runs[0] ?? null);

  $effect(() => {
    void api
      .measureLog()
      .then((l) => {
        // Shape-checked rather than trusted, the same reason the measurement socket is:
        // this route is newer than some daemons the UI will meet, and an ingress proxy
        // answering `{}` must read as "cannot tell" and not crash the report it sits under.
        if (!l || !Array.isArray(l.runs)) {
          listError = 'the add-on answered the transcript listing with something this UI does not recognise';
          return;
        }
        list = l;
        listError = null;
      })
      .catch((e) => {
        listError = e instanceof Error ? e.message : String(e);
      });
  });

  function when(unix: number): string {
    return new Date(unix * 1000).toLocaleString();
  }

  /** Fetch the document and hand it to the browser as a file. Done client-side rather than
   *  as a plain link so a refusal (the run was dropped by retention) arrives as its own
   *  sentence instead of a tab full of JSON or an opaque error page. */
  async function download() {
    if (!newest) return;
    busy = true;
    problem = null;
    try {
      const doc = await api.measureLogRun(newest.id);
      const blob = new Blob([JSON.stringify(doc, null, 2)], { type: 'application/json' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = `alignment-${doc.id}.json`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      // Verbatim when the daemon reasoned about it; its own message otherwise. Never
      // "something went wrong".
      const refusal = refusalOf(e);
      problem = refusal ? refusal.message : e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }
</script>

<div class="runlog">
  {#if newest}
    <button class="ghost" disabled={busy} title="The whole transcript of that run, as one JSON file" onclick={() => void download()}>
      {busy ? 'Fetching…' : 'Download the run log'}
    </button>
    <span class="hint">
      {#if reason}{reason}{/if}
      Newest stored run: <code>{newest.id}</code>, started {when(newest.started_unix)} — {newest.events} events, ended in
      <code>{newest.last_kind}</code>. The last {list?.retained ?? 'few'} runs are kept.
    </span>
  {:else if list && list.runs.length === 0}
    <span class="hint">
      No run transcript is stored{list.directory ? '' : ' — this add-on has nowhere to write them, so nothing is recorded'}.
    </span>
  {:else if listError}
    <span class="hint">The run transcripts could not be read: {listError}</span>
  {/if}
  <!-- The refusal's own sentence — a transcript dropped by retention says exactly that. -->
  {#if problem}<span class="problem">{problem}</span>{/if}
</div>

<style>
  /* Unobtrusive on purpose: it sits under a report someone is reading, and it is for the
     minority of visits where the report was not enough. */
  .runlog {
    display: flex;
    align-items: baseline;
    gap: 8px;
    flex-wrap: wrap;
    margin-top: 10px;
  }
  .runlog button {
    flex: 0 0 auto;
    padding: 3px 9px;
    font-size: 0.78rem;
  }
  .hint {
    font-size: 0.76rem;
    color: var(--secondary-text-color);
  }
  .problem {
    font-size: 0.78rem;
    color: var(--error-color, #db4437);
  }
</style>
