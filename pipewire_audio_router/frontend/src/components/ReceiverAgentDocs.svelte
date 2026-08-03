<script lang="ts">
  // "Explain receiver hosts" — the document behind the second help button in the
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
      <h2 id="agent-docs-title">Receiver hosts: streaming to a Linux machine</h2>
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
        >chmod +x pwrouter-agent-* &amp;&amp; install -Dm755 pwrouter-agent-* ~/.local/bin/pwrouter-agent
mkdir -p ~/.config/systemd/user
curl -sSL -o ~/.config/systemd/user/pwrouter-agent.service \\
  https://raw.githubusercontent.com/davidgraeff/homeassistant-pipewire-audio-routing/main/pipewire_audio_router/pwrouter-agent/pwrouter-agent.service
systemctl --user daemon-reload
systemctl --user enable --now pwrouter-agent</code
      ></pre>
    <p class="card-sub hint">
      A <em>user</em> service, not a system one: the helper controls the audio of one logged-in session,
      so it runs as you and uses your PipeWire. Two people sharing a machine each install their own and
      appear as two separate outputs; neither can touch the other's audio.
    </p>

    <h3>3. Pair</h3>
    <p class="card-sub">
      On first start the helper looks for this add-on on the network and asks to pair. Its log prints a
      short code:
    </p>
    <pre><code>journalctl --user -u pwrouter-agent -f
… waiting for approval in the add-on UI — pairing code: 4F2A9C</code></pre>
    <p class="card-sub">
      The request then appears under <strong>Receiver hosts</strong> on this page with the same code.
      Compare them before approving: that check is what stops you from handing control of your audio to
      someone else's machine on the network. Approving stores a token on that machine
      (<code>~/.config/pwrouter-agent/config.json</code>, readable only by that user), and the host
      shows up under Discovered devices for you to add like any other speaker.
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
      <strong>Remove</strong> under Receiver hosts revokes the token — that machine stops receiving
      immediately. On the machine itself:
      <code>systemctl --user disable --now pwrouter-agent</code>, then delete the binary, the unit and
      <code>~/.config/pwrouter-agent</code>.
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
