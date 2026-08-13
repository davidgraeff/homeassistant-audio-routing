<script lang="ts">
  import { onMount } from 'svelte';
  import { api } from '../../lib/api';
  import { routing } from '../../lib/routing';
  import { PAGES, route, type Page } from '../../lib/route.svelte';

  // The front page, behind the app's own title. Two jobs, in this order:
  //
  //   1. Say what this add-on *is*, for someone who has just installed it and is looking
  //      at seven tabs. The words are the README's, deliberately — a newcomer should not
  //      have to find GitHub to learn what they installed.
  //   2. Show the three setup steps as the flow they actually are. The tab bar already
  //      numbers them, but a chevron can only say "in this order"; audio coming in, being
  //      mixed, and coming out in two rooms while a voice talks over it is a *picture*,
  //      and it is the picture that makes the three tabs obvious.
  //
  // The diagram deliberately reuses the routing graph's own idiom for "audio is flowing"
  // (travelling dashes along a wire, `FlowGraph`'s `.wire.active`), so the animation here
  // teaches the thing the user will see on the Music groups page for real.
  //
  // The same drawing also exists as standalone files for the README —
  // docs/diagrams/audio-flow.svg and audio-flow-dark.svg. Change one, change the other. They
  // are deliberately copies rather than one shared asset: GitHub renders an <img>, which has
  // no Home Assistant theme variables to read (hence a hardcoded palette, one file per
  // scheme) and no interactivity (hence no hover highlight and no live counts there).

  /** The three steps, in `PAGES` order — the same slice the tab bar's chevron flow takes,
   *  for the same reason: one list decides what "the setup path" is. */
  const SETUP: { id: Page; title: string; what: string; done: () => Status }[] = [
    {
      id: 'sources',
      title: 'Add what plays',
      what:
        'An input is anything that sends audio in: an iPhone, Mac or PC over AirPlay, a phone over Bluetooth through a small bridge box, a Linux machine, or YouTube Music’s own Cast button.',
      done: () => count($routing.matrix.sources.filter((s) => s.present).length, 'input', 'present'),
    },
    {
      id: 'outputs',
      title: 'Add the speakers',
      what:
        'Every compatible device on your network is *offered* — AirPlay-2 receivers and HomePods, AV receivers, ESPHome speakers, a PC running the agent — including the neighbours’. Nothing is routable, gets an entity or is ever sent audio until you add it.',
      done: () => count($routing.matrix.outputs.length, 'speaker', 'added'),
    },
    {
      id: 'music',
      title: 'Group the rooms',
      what:
        'A group is the set of speakers that plays the same thing, in sync, and it becomes one Home Assistant <code>media_player</code>. Pick what a group plays here — or wire it by hand in the routing graph on the same page.',
      done: () => (groups === null ? unknown : count(groups, 'group', '')),
    },
  ];

  // Live status per step, so the page is a checklist for the person who has done one and a
  // half of them — not a brochure that says the same thing on day 100 as on day 1.
  type Status = { ok: boolean; text: string } | null;
  const unknown: Status = null;
  function count(n: number, noun: string, verb: string): Status {
    // Not connected: the counts are all zero because nothing has arrived yet, and saying
    // "no speakers added" then would be a claim about the user's setup we cannot make.
    if (!$routing.connected) return null;
    const label = n === 1 ? noun : `${noun}s`;
    return { ok: n > 0, text: n === 0 ? `no ${noun} yet` : `${n} ${label}${verb ? ` ${verb}` : ''}` };
  }

  /** Music groups are not in the routing store (only the matrix is), so one GET. A failure
   *  is left as "unknown" rather than "none" — see `count`. */
  let groups = $state<number | null>(null);
  onMount(async () => {
    try {
      groups = (await api.musicGroups()).length;
    } catch {
      groups = null;
    }
  });

  /** Which stage of the diagram is lit. Set by hovering or focusing a step, so the card and
   *  the picture explain each other; null = the whole flow runs. */
  let focus = $state<Page | null>(null);

  const stepNo = (id: Page) => PAGES.indexOf(id) + 1;
</script>

<section class="hero card">
  <h2>Whole-home audio for Home Assistant, without the stutter</h2>
  <p>
    Play from your phone, your PC or any Bluetooth device, send it to any speakers in the house from one dashboard
    card, and let Home Assistant talk over the top of it — ducked, not interrupted.
  </p>
  <p class="sub">
    Under the hood the audio path is PipeWire's own realtime graph, driven by a small Rust daemon: mixing and
    re-clocking several streams for several rooms is a realtime-scheduling problem, and this sits <em>under</em> a music
    library rather than in place of one.
  </p>
  <ul class="gets">
    <li><strong>In sync across protocols</strong> — AirPlay, ESPHome and PipeWire speakers in one group.</li>
    <li><strong>Ordinary entities</strong> — a <code>media_player</code> per speaker and per group, with volume, drivable from automations.</li>
    <li><strong>Announcements that duck</strong> — TTS plays over the music at full clarity; the music dips and comes back.</li>
    <li><strong>Discovery you stay in charge of</strong> — every device is offered, none is used until you add it.</li>
  </ul>
</section>

<h3 class="flow-title">How it fits together</h3>

<!-- The picture. `data-focus` is what a hovered step lights up; without one, everything runs. -->
<div class="figure card" data-focus={focus ?? 'all'}>
  <svg viewBox="0 0 660 258" role="img" aria-label="Audio comes in from a phone, a Bluetooth bridge and a PC, is mixed by the router, and plays in two grouped rooms while a Home Assistant announcement ducks the music.">
    <!-- ---- stage 1: what plays ------------------------------------------ -->
    <!-- Text, no pictograms: an emoji here would be the only colour-font glyph on the page
         and would out-shout the wires, which are the thing that carries the meaning. -->
    <g class="stage s-sources">
      <g class="chip" transform="translate(8,26)">
        <rect width="132" height="40" rx="9" />
        <text x="16" y="24">AirPlay</text>
      </g>
      <g class="chip" transform="translate(8,96)">
        <rect width="132" height="40" rx="9" />
        <text x="16" y="24">Bluetooth</text>
      </g>
      <g class="chip" transform="translate(8,166)">
        <rect width="132" height="40" rx="9" />
        <text x="16" y="24">A PC</text>
      </g>
    </g>

    <!-- in-wires: source → router.
         Wrapped in a group, and every animated thing in this diagram is, because that is
         the only way the highlight below can dim them: a running animation's `opacity`
         wins over any static rule, so dimming the wire itself does nothing while it is
         flowing (and worked only with motion turned off). A parent group's opacity
         multiplies with the child's animated one, so this dims either way. -->
    <g class="stage w-in">
      <path class="wire music" d="M140,46 C200,46 210,105 262,113" />
      <path class="wire music" d="M140,116 C200,116 214,118 262,121" />
      <path class="wire music" d="M140,186 C200,186 210,137 262,129" />
    </g>

    <!-- ---- stage 2+3: the router, and the rooms it feeds ---------------- -->
    <g class="stage s-router">
      <g class="box" transform="translate(262,88)">
        <rect width="126" height="66" rx="12" />
        <text class="box-t" x="63" y="28">PipeWire</text>
        <text class="box-s" x="63" y="48">realtime graph</text>
      </g>
      <!-- The mixing beat: one pulse per bar, so the box reads as doing something. -->
      <circle class="beat" cx="325" cy="121" r="30" />
    </g>

    <!-- out-wires: router → rooms -->
    <g class="stage w-out">
      <path class="wire music out" d="M388,110 C460,110 470,66 524,60" />
      <path class="wire music out" d="M388,132 C460,132 470,176 524,182" />
    </g>

    <g class="stage s-rooms">
      <!-- The group is what step 3 adds: a ring *around* both speakers rather than a
           bracket beside them, which crossed the wires feeding them and read as a third
           kind of line. -->
      <rect class="bracket" x="512" y="28" width="146" height="186" rx="14" />
      <text class="bracket-t" x="585" y="230">one group, in sync</text>
      <g class="chip room" transform="translate(524,40)">
        <rect width="124" height="40" rx="9" />
        <text x="16" y="24">Kitchen</text>
      </g>
      <g class="chip room" transform="translate(524,162)">
        <rect width="124" height="40" rx="9" />
        <text x="16" y="24">Living room</text>
      </g>
    </g>

    <!-- ---- the announcement, over the top -------------------------------
         Two nested groups on purpose: the inner one's opacity is the animation (it is only
         there while HA is speaking), the outer one is what the highlight dims. -->
    <g class="stage s-voice">
      <g class="voice">
        <path class="wire talk" d="M325,222 C325,190 325,170 325,156" />
        <g class="chip talk-chip" transform="translate(232,222)">
          <rect width="186" height="28" rx="8" />
          <text x="14" y="19">“Someone’s at the door”</text>
        </g>
      </g>
    </g>
  </svg>
  <p class="legend">
    Audio flows left to right; the music dips when Home Assistant speaks, then comes back. Hover a step below to see
    which part of this it sets up.
  </p>
</div>

<ol class="steps">
  {#each SETUP as s (s.id)}
    {@const st = s.done()}
    <li>
      <a
        class="step card"
        href={route.href(s.id)}
        onmouseenter={() => (focus = s.id)}
        onmouseleave={() => (focus = null)}
        onfocus={() => (focus = s.id)}
        onblur={() => (focus = null)}
      >
        <span class="head">
          <span class="n" aria-hidden="true">{stepNo(s.id)}</span>
          <span class="t">{s.title}</span>
        </span>
        <!-- The one place this app renders prose from a constant; `code` and `em` in it are
             ours, not user input. -->
        <span class="what">{@html s.what.replace(/\*(.+?)\*/g, '<em>$1</em>')}</span>
        <!-- Its own row at the foot of the card, not beside the title: the three titles are
             different lengths, so in the title row the badge landed inline on one card and
             wrapped on the next. `margin-top: auto` keeps all three on one line across the
             grid however tall the descriptions are. -->
        {#if st}
          <span class="badge" class:ok={st.ok}>{st.ok ? '✓' : '•'} {st.text}</span>
        {/if}
      </a>
    </li>
  {/each}
</ol>

<p class="after">
  Then: <a href={route.href('announcements')}>Announcements</a> for what ducks the music and by how much,
  <a href={route.href('alignment')}>Alignment</a> if a room sounds a beat late, and the
  <code>custom:pipewire-router-card</code> dashboard card for day-to-day routing.
</p>

<style>
  .hero h2 {
    font-size: 1.35rem;
    font-weight: 500;
    line-height: 1.3;
    margin: 0 0 10px;
  }
  .hero p {
    margin: 0 0 10px;
    max-width: 68ch;
  }
  .hero .sub {
    color: var(--secondary-text-color);
    font-size: 0.88rem;
  }
  .gets {
    margin: 14px 0 0;
    padding: 0;
    list-style: none;
    display: grid;
    gap: 8px;
    grid-template-columns: repeat(auto-fit, minmax(260px, 1fr));
    font-size: 0.88rem;
  }
  .gets li {
    padding-left: 18px;
    position: relative;
    color: var(--secondary-text-color);
  }
  /* A dash rather than a bullet glyph: four rows of round dots next to four rows of text
     is a lot of ink for "these are the four things". */
  .gets li::before {
    content: '';
    position: absolute;
    left: 0;
    top: 0.68em;
    width: 9px;
    height: 2px;
    background: var(--primary-color);
  }
  .gets strong {
    color: var(--primary-text-color);
    font-weight: 500;
  }

  .flow-title {
    margin: 22px 0 10px;
    font-size: 1rem;
    font-weight: 500;
  }
  .figure {
    padding: 8px 12px 4px;
  }
  .figure svg {
    display: block;
    width: 100%;
    height: auto;
  }
  .legend {
    margin: 0 4px 8px;
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }

  /* ---- the diagram ----------------------------------------------------- */
  .chip rect {
    fill: color-mix(in srgb, var(--primary-text-color) 6%, var(--card-background-color));
    stroke: color-mix(in srgb, var(--secondary-text-color) 30%, transparent);
  }
  .chip text {
    font-size: 14px;
    fill: var(--primary-text-color);
  }
  .box rect {
    fill: color-mix(in srgb, var(--primary-color) 12%, var(--card-background-color));
    stroke: color-mix(in srgb, var(--primary-color) 55%, transparent);
  }
  .box text {
    text-anchor: middle;
  }
  .box-t {
    font-size: 15px;
    font-weight: 600;
    fill: var(--primary-text-color);
  }
  .box-s {
    font-size: 11px;
    fill: var(--secondary-text-color);
  }
  .beat {
    fill: none;
    stroke: var(--primary-color);
    opacity: 0;
  }
  .bracket {
    fill: none;
    stroke: color-mix(in srgb, var(--primary-color) 45%, transparent);
    stroke-width: 1.5;
    stroke-dasharray: 5 5;
  }
  .bracket-t {
    font-size: 11px;
    text-anchor: middle;
    letter-spacing: 0.03em;
    fill: var(--secondary-text-color);
  }
  .wire {
    fill: none;
    stroke: var(--primary-color);
    stroke-width: 2.5;
    stroke-linecap: round;
  }
  .wire.talk {
    stroke: var(--warning-color, #f9a825);
    stroke-width: 2;
  }
  .talk-chip rect {
    fill: color-mix(in srgb, var(--warning-color, #f9a825) 16%, var(--card-background-color));
    stroke: color-mix(in srgb, var(--warning-color, #f9a825) 45%, transparent);
  }
  .talk-chip text {
    font-size: 12px;
    fill: var(--primary-text-color);
  }

  /* ---- the animation ---------------------------------------------------
     One 9s bar, and everything is a phase of it: dashes travel the wires the whole time
     (the same 6/8 dash and 14px period as the routing graph's live wires), the box pulses
     once a second, and from 5s to 7s Home Assistant talks — the announcement wire and its
     bubble come up while the music wires drop to a duck. */
  .wire.music {
    stroke-dasharray: 6 8;
    animation:
      flow 0.8s linear infinite,
      duck 9s ease-in-out infinite;
  }
  @keyframes flow {
    to {
      stroke-dashoffset: -14;
    }
  }
  @keyframes duck {
    0%,
    52% {
      opacity: 1;
      stroke-width: 2.5;
    }
    58%,
    74% {
      opacity: 0.4;
      stroke-width: 1.8;
    }
    82%,
    100% {
      opacity: 1;
      stroke-width: 2.5;
    }
  }
  .beat {
    animation: beat 1s ease-out infinite;
  }
  @keyframes beat {
    0% {
      opacity: 0.5;
      r: 6;
    }
    100% {
      opacity: 0;
      r: 34;
    }
  }
  .voice {
    opacity: 0;
    animation: talk 9s ease-in-out infinite;
  }
  .voice .wire {
    stroke-dasharray: 5 6;
    animation: flow-up 0.6s linear infinite;
  }
  @keyframes flow-up {
    to {
      stroke-dashoffset: 11;
    }
  }
  @keyframes talk {
    0%,
    50% {
      opacity: 0;
    }
    58%,
    74% {
      opacity: 1;
    }
    84%,
    100% {
      opacity: 0;
    }
  }

  /* Everything above is decoration on a diagram that is already legible standing still:
     with motion off, the wires are solid, the announcement bubble simply stays visible, and
     nothing pulses. */
  @media (prefers-reduced-motion: reduce) {
    .wire.music,
    .voice .wire,
    .beat,
    .voice {
      animation: none;
    }
    .wire.music,
    .voice .wire {
      stroke-dasharray: none;
    }
    .voice {
      opacity: 1;
    }
    .beat {
      opacity: 0;
    }
  }

  /* Hovering a step dims the parts of the picture that step is not about. Opacity only —
     the layout must not move, or the diagram would jump under the pointer. Everything named
     here is a `.stage` group or an un-animated shape, for the reason in the markup. */
  .stage,
  .bracket,
  .bracket-t {
    transition: opacity 0.2s ease;
  }
  /* 1 — what plays: the inputs and their wires into the router. */
  .figure[data-focus='sources'] :is(.s-router, .w-out, .s-rooms, .s-voice),
  /* 2 — the speakers themselves: the group ring is step 3's, so it dims with the rest. */
  .figure[data-focus='outputs'] :is(.s-sources, .w-in, .s-voice, .bracket, .bracket-t),
  /* 3 — the grouping: the rooms and the ring around them. */
  .figure[data-focus='music'] :is(.s-sources, .w-in, .s-voice) {
    opacity: 0.22;
  }

  /* ---- the three steps ------------------------------------------------- */
  .steps {
    list-style: none;
    margin: 14px 0 0;
    padding: 0;
    display: grid;
    gap: 12px;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
  }
  .step {
    display: flex;
    flex-direction: column;
    gap: 6px;
    height: 100%;
    text-decoration: none;
    color: inherit;
  }
  .step:hover {
    border-color: color-mix(in srgb, var(--primary-color) 55%, transparent);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  /* The same numbered circle as the tab bar's chevrons and the alignment wizard's stepper:
     one visual language for "step N of a sequence". */
  .n {
    flex: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 20px;
    height: 20px;
    border-radius: 50%;
    font-size: 0.72rem;
    background: var(--primary-color);
    color: var(--text-on-primary);
  }
  .t {
    font-weight: 500;
  }
  .badge {
    align-self: flex-start;
    margin-top: auto;
    font-size: 0.7rem;
    padding: 1px 7px;
    border-radius: 999px;
    white-space: nowrap;
    background: color-mix(in srgb, var(--secondary-text-color) 14%, transparent);
    color: var(--secondary-text-color);
  }
  .badge.ok {
    background: color-mix(in srgb, var(--success-color) 16%, transparent);
    color: var(--success-color);
  }
  .what {
    font-size: 0.84rem;
    color: var(--secondary-text-color);
  }
  .after {
    margin: 16px 0 0;
    font-size: 0.85rem;
    color: var(--secondary-text-color);
  }
</style>
