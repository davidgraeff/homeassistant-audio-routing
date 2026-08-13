<script lang="ts">
  // "Setup Linux/PipeWire host" — the document behind the second help button in the
  // Supported outputs header, and the place the agent binaries are downloaded
  // from. Mirrors RtpSenderDocs: the commands are built for *this* install, so
  // they can be copied without editing.
  import { onMount } from 'svelte';

  interface Props {
    onClose: () => void;
  }
  let { onClose }: Props = $props();

  let dialogEl = $state<HTMLDivElement>();
  $effect(() => {
    dialogEl?.focus();
  });

  // Served by the daemon next to the UI (Dockerfile's `agent` stage copies them
  // into /app/www/agent), so the link works through the Home Assistant ingress
  // path without knowing the add-on's address.
  const downloads = [
    { arch: 'x86-64', file: 'pwrouter-agent-x86_64', hint: 'Intel/AMD desktops, laptops, mini PCs' },
    { arch: 'aarch64', file: 'pwrouter-agent-aarch64', hint: 'Raspberry Pi 3/4/5, ARM boards, Apple-silicon VMs' },
  ];

  // The agent dials back to this add-on, so the address it needs is the one the
  // browser is talking to right now. Behind the Home Assistant ingress that
  // address is Home Assistant itself, which the agent cannot use — hence the note
  // in the pairing step rather than a fabricated command line.
  let origin = $state('');
  onMount(() => {
    origin = window.location.host;
  });
</script>

<svelte:window onkeydown={(e) => e.key === 'Escape' && onClose()} />

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="modal-backdrop" onclick={onClose}>
  <div
    class="modal-card card wide"
    role="dialog"
    aria-modal="true"
    aria-labelledby="agent-docs-title"
    tabindex="-1"
    bind:this={dialogEl}
    onclick={(e) => e.stopPropagation()}
  >
    <div class="card-head">
      <h2 id="agent-docs-title">Streaming to a Linux/PipeWire machine</h2>
      <button class="ghost" type="button" onclick={onClose}>Close</button>
    </div>

    <p class="card-sub">
      Any Linux machine running PipeWire can be an output — your desk PC, a Pi in the workshop, a media
      box. It needs a small helper, <code>pwrouter-agent</code>, which receives the audio and plays it
      through whatever that machine's speakers already are.
    </p>

    <h3>What the helper does, and what it deliberately cannot do</h3>
    <ul class="card-sub">
      <li>
        It <strong>dials out</strong> to this add-on. Nothing listens on your machine: no open port, no
        firewall rule, nothing for a network scan to find.
      </li>
      <li>
        It accepts a <strong>fixed set of commands</strong> — set volume, set mute, duck, unduck, start
        or stop receiving. That is the whole protocol: it cannot be asked to run a command, read a file,
        or reconfigure your audio.
      </li>
      <li>
        It <strong>configures the receive side itself</strong>, so there is no PipeWire config file to
        write. Stop the helper and the audio nodes it made disappear with it.
      </li>
      <li>
        It <strong>puts your volumes back</strong> when an announcement ends, when this add-on stops
        answering, and when it exits — a dead add-on cannot leave your desktop turned down.
      </li>
      <li>
        It never <em>owns</em> your volume: turn the knob locally and Home Assistant follows, rather than
        overwriting you.
      </li>
    </ul>

    <h3>1. Download</h3>
    <p class="card-sub">
      Pick the architecture of the machine you are installing on (<code>uname -m</code> tells you).
    </p>
    <p class="downloads">
      {#each downloads as d (d.file)}
        <a class="dl" href={`agent/${d.file}`} download={d.file}>
          <strong>{d.arch}</strong>
          <span>{d.hint}</span>
        </a>
      {/each}
    </p>
    <p class="card-sub hint">
      These are dynamically linked against the system's own PipeWire — a static build is not possible,
      since the helper is a PipeWire client and must use the library the host already runs. They need
      <strong>glibc 2.34 or newer</strong> and PipeWire 0.3, which means:
    </p>
    <ul class="card-sub hint">
      <li><strong>Ubuntu 22.04 LTS</strong> or newer (22.04 ships glibc 2.35)</li>
      <li><strong>Fedora 35</strong> or newer (Fedora 35 ships glibc 2.34)</li>
      <li><strong>Debian 12 “bookworm”</strong> or newer (glibc 2.36)</li>
      <li>Arch, openSUSE Tumbleweed and other rolling distributions: fine as they are</li>
    </ul>
    <p class="card-sub hint">
      Older than that — Ubuntu 20.04, Debian 11, Fedora 34 — and the binary will refuse to start with a
      <code>GLIBC_2.34 not found</code> error. Build from source on those:
      <code>cargo build --release</code> in <code>pipewire_audio_router/pwrouter-agent</code>, which
      needs a Rust toolchain and the <code>libpipewire-0.3</code> development headers.
    </p>

    <h3>2. Install</h3>
    <pre><code
        >chmod +x pwrouter-agent-* &amp;&amp; ./pwrouter-agent-* autostart enable
systemctl --user start pwrouter-agent</code
      ></pre>
    <p class="card-sub hint">
      <code>autostart enable</code> copies the binary to <code>~/.local/bin/pwrouter-agent</code> and
      points the service at that fixed path, so <strong>updating later is one copy</strong>: put a newer
      download over that file and <code>systemctl --user restart pwrouter-agent</code>. Nothing has to be
      re-enabled, and the service can never end up starting an older copy you left in
      <code>~/Downloads</code>. <code>pwrouter-agent version</code> says which build a binary is (and
      <code>autostart</code> with no argument says which one the service starts) — worth checking when a
      machine behaves like an older version.
    </p>
    <p class="card-sub hint">
      No download of a service file either: the systemd unit is built into the binary.
      <code>autostart enable</code> writes it to
      <code>~/.config/systemd/user/pwrouter-agent.service</code>
      and turns it on for your next login; <code>autostart disable</code> removes it again, and
      <code>autostart</code> alone reports which it is. The tray menu has the same switch under
      <strong>Autostart</strong>. Starting and stopping stay separate commands on purpose — an agent
      installing its own unit is usually already running, and two in one session would fight over your
      volume.
    </p>
    <p class="card-sub hint">
      To stop it, the tray menu's <strong>Quit</strong> is enough: on a machine where it runs as the
      systemd service it asks systemd to stop the unit (so it does not immediately come back), and
      otherwise it just exits. Either way it puts the machine's audio back and unloads its receiver, and
      autostart is untouched — so it returns at your next login unless you turn that off too.
    </p>
    <p class="card-sub hint">
      A <em>user</em> service, not a system one: the helper controls the audio of one logged-in session,
      so it runs as you and uses your PipeWire. Two people sharing a machine each install their own and
      appear as two separate outputs; neither can touch the other's audio.
    </p>

    <h3>3. Pair</h3>
    <p class="card-sub">
      On start the helper looks for this add-on on the network and asks to pair. Its log prints a short
      code, minted once per run — restart it and you get a fresh one, but reconnects keep this one:
    </p>
    <pre><code>journalctl --user -u pwrouter-agent -f
… not paired yet — pairing code for this host: 4F2A9C</code></pre>
    <p class="card-sub">
      On a desktop it also shows you the code directly: a notification when it starts asking, and a
      status icon in the tray whose menu keeps the code readable afterwards, next to the add-on it
      found and what it is currently playing. That needs the desktop to support it — KDE, Xfce,
      Cinnamon, MATE and most window-manager bars do, GNOME needs its AppIndicator extension — so the
      log line above is always there as the fallback.
      <code>pwrouter-agent spike-desktop</code> shows both for a made-up code, without asking this
      add-on for anything, if you want to know what a machine supports.
    </p>
    <p class="card-sub">
      The machine then appears under <strong>Discovered devices</strong> on this page, like any other
      speaker, with the same code on its card. Compare them before pressing <strong>Pair</strong>: that
      check is what stops you from handing control of your audio to someone else's machine on the
      network. Pairing stores a token on that machine
      (<code>~/.config/pwrouter-agent/config.json</code>, readable only by that user) and adds it as an
      output — routable, and a Home Assistant <code>media_player</code> if that setting is on.
    </p>
    <h3>Which speakers it comes out of</h3>
    <p class="card-sub">
      That is decided <em>on that machine</em>, not here: its tray menu has a <strong>Play to</strong>
      list of its own outputs. Left alone it follows the machine's default output, which is usually right
      for a laptop and wrong for a PC wired to the speakers in one room — pick that output once and it
      stays picked, whatever the desktop's default later becomes.
    </p>
    <p class="card-sub hint">
      A chosen output is never silently swapped: while it is unavailable (unplugged, powered down)
      nothing is played there, rather than the audio appearing somewhere unexpected. This page shows that
      as a host that is online but not playing; the machine's own tray says which output it is waiting
      for. Plug it back in and it resumes by itself.
    </p>

    <h3>Volume, from either end</h3>
    <p class="card-sub">
      The slider on this page drives that machine's <em>own</em> master volume — the same one its volume
      keys and its desktop mixer move — so whoever is sitting there can turn it too, and this page
      follows. Its tray menu has the same two controls: <strong>Volume</strong> (a submenu of levels, and
      the mouse wheel over the icon for anything in between) and <strong>Mute</strong>, which
      middle-clicking the icon also toggles. Neither end owns the value; both show what the machine
      reports.
    </p>
    <p class="card-sub hint">
      They appear only while something is actually routed to that machine: until then it has no stream,
      so there is no output of its to attenuate, and a control that could not move would be worse than
      none.
    </p>

    <h3>When that machine sleeps</h3>
    <p class="card-sub">
      The helper hears the suspend coming (from <code>logind</code>) and tells this add-on <em>before</em>
      the machine freezes, so its card says <strong>asleep</strong> — or <strong>shut down</strong> — rather
      than offline. Its routing, level and group membership are kept, its audio session is closed cleanly,
      and Home Assistant marks it unavailable instead of offering a slider for a sleeping computer.
    </p>
    <p class="card-sub hint">
      Without that warning nobody would know for minutes: a suspended machine never closes its connection,
      so it keeps looking connected while being sent audio nobody can hear. Waking it reconnects within a
      second or so. A machine that just falls off the network still reads offline — only its own word
      earns the gentler wording.
    </p>

    <p class="card-sub hint">
      If the machine cannot find the add-on — a routed VLAN, or mDNS blocked — point it straight at the
      add-on's host and port instead: <code>pwrouter-agent run --daemon &lt;host&gt;:8099</code>.
      {#if origin}
        The address in your browser right now is <code>{origin}</code>; through the Home Assistant
        ingress that is Home Assistant's address, not the add-on's, so use the add-on host's own IP with
        port 8099.
      {/if}
    </p>

    <h3>Removing a host</h3>
    <p class="card-sub">
      <strong>Unpair</strong> on the output's card revokes the token: that machine stops receiving
      immediately and loses its routing, group membership and Home Assistant player. Its helper keeps
      running and keeps asking, so it comes back under Discovered devices — press <strong>Ignore</strong>
      there to put it away, or <strong>Pair</strong> to let it back in.
    </p>
    <p class="card-sub hint">
      To be rid of it for good, stop it on the machine itself:
      <code>pwrouter-agent autostart disable</code> (removes the unit) and
      <code>systemctl --user stop pwrouter-agent</code>, then delete the binary and
      <code>~/.config/pwrouter-agent</code>. Nothing you do in this add-on can stop a helper from
      asking — that is deliberate, so a lost add-on configuration never means logging in to every
      machine to get your outputs back.
    </p>
  </div>
</div>

<style>
  .downloads {
    display: flex;
    gap: 12px;
    flex-wrap: wrap;
    margin: 0 0 10px;
  }
  .dl {
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 10px 14px;
    border-radius: 10px;
    border: 1px solid var(--divider-color, #e0e0e0);
    background: var(--card-background-color, #fff);
    text-decoration: none;
    color: inherit;
    transition: border-color 0.12s, background 0.12s;
  }
  .dl:hover {
    border-color: var(--primary-color, #03a9f4);
    background: color-mix(in srgb, var(--primary-color, #03a9f4) 8%, transparent);
  }
  .dl strong {
    font-size: 0.95rem;
  }
  .dl span {
    font-size: 0.78rem;
    color: var(--secondary-text-color);
  }
  pre {
    overflow-x: auto;
    padding: 10px 12px;
    border-radius: 8px;
    background: color-mix(in srgb, var(--primary-text-color, #000) 6%, transparent);
    font-size: 0.8rem;
    line-height: 1.5;
  }
  .hint {
    font-size: 0.78rem;
  }
</style>
