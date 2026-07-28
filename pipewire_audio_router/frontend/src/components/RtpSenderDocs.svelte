<script lang="ts">
  import { toast } from '../lib/toast';

  // Setup documentation for the *other* end of the RTP-receive source: how to
  // turn a PipeWire machine (desktop, laptop, another Pi) into a sender that
  // feeds this add-on. Shown as a modal from the "Explain RTP sender setup"
  // button in the Input-sources tab header (SourcesTab.svelte).
  //
  // The config samples are generated from the most relevant live settings (the
  // open RTP editor's, else the first configured RTP source's, else the
  // defaults — see openDocs()) and from `location.hostname`, so what
  // the user sees is copy-pasteable for *their* install rather than a generic
  // template. Format and channel count are the exception — those are fixed by
  // rtp_source.rs on the receiving side, and the whole point of this page is
  // that both ends must agree on every one of them exactly.
  //
  // Sources for the content: firmware/pi-bridge/setup_pi_bridge.py (the same
  // module-rtp-sink drop-in, proven on real hardware against this receiver),
  // bridge-daemon/src/pw_sink.rs (S16LE-vs-S16BE endianness trap),
  // docs/decisions.md (hot module loading is owned by the loading process, so
  // a non-interactive `pw-cli load-module` drops the node immediately) and
  // spikes/03b-rtp-pc-transfer.md (`media.class` must be set explicitly).

  interface Props {
    /** The receiver's listen port, so the sample config targets the right one. */
    port: number;
    /** Receiver-side jitter buffer in ms — quoted in the latency section. */
    latencyMsec: number;
    /** `audio.rate` the receiver decodes at; the sender must transmit the same. */
    rate: number;
    /** `source.ip`: `0.0.0.0` for unicast, or the joined multicast group. */
    sourceAddr: string;
    /** `sess.ignore-ssrc`: false = the "Only one client" mode, which needs a
     *  warning here because PipeWire senders re-roll their SSRC on restart. */
    ignoreSsrc: boolean;
    onClose: () => void;
  }
  let { port, latencyMsec, rate, sourceAddr, ignoreSsrc, onClose }: Props = $props();

  const multicast = $derived(sourceAddr !== '' && sourceAddr !== '0.0.0.0');

  // Where the sender should aim. In multicast mode that's the group the
  // receiver joined; otherwise the host serving this UI, which is the HA host
  // the add-on runs on (host networking, so its LAN IP is the add-on's).
  const destIp = $derived(multicast ? sourceAddr : location.hostname || '192.168.1.10');

  // The persistent drop-in — the recommended route, and the same shape the Pi
  // bridge writes (firmware/pi-bridge/setup_pi_bridge.py).
  const dropIn = $derived(`# ~/.config/pipewire/pipewire.conf.d/60-audio-router-rtp.conf
context.modules = [
  { name = libpipewire-module-rtp-sink
    args = {
      destination.ip = "${destIp}"
      destination.port = ${port}
      sess.name = "My PC"
      sess.media = "audio"
      audio.format = "S16LE"
      audio.rate = ${rate}
      audio.channels = 2
      #local.ifname = "eth0"${multicast ? '\n      net.ttl = 16' : ''}
      stream.props = {
        media.class = "Audio/Sink"
        node.name = "audio-router-rtp"
        node.description = "Audio Router (RTP)"
      }
    }
  }
]`);

  // Same module, loaded live for a quick try. `-m` keeps pw-cli running: the
  // module is hosted in *its* process, so the sink exists only while it does.
  const oneLiner = $derived(
    `pw-cli -m load-module libpipewire-module-rtp-sink '{ destination.ip = "${destIp}" destination.port = ${port} ` +
      `sess.media = "audio" audio.format = "S16LE" audio.rate = ${rate} audio.channels = 2 ` +
      `stream.props = { media.class = "Audio/Sink" node.name = "audio-router-rtp" node.description = "Audio Router (RTP)" } }'`,
  );

  // Focus the dialog when it opens: it's a long document, so this is what makes
  // Page Down / arrow keys scroll it instead of the page behind.
  let dialogEl = $state<HTMLDivElement>();
  $effect(() => {
    dialogEl?.focus();
  });

  async function copy(text: string) {
    try {
      await navigator.clipboard.writeText(text);
      toast('success', 'Copied to clipboard');
    } catch {
      // Ingress over plain HTTP has no clipboard API — the text is on screen
      // anyway, so this is a nudge, not a failure worth an error toast.
      toast('info', 'Clipboard unavailable — select the text to copy it');
    }
  }
</script>

<svelte:window onkeydown={(e) => e.key === 'Escape' && onClose()} />

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="modal-backdrop" onclick={onClose}>
  <div
    class="modal-card card wide"
    role="dialog"
    aria-modal="true"
    aria-labelledby="rtp-docs-title"
    tabindex="-1"
    bind:this={dialogEl}
    onclick={(e) => e.stopPropagation()}
  >
    <div class="card-head">
      <h2 id="rtp-docs-title">Set up PipeWire as a sender</h2>
      <button class="ghost" type="button" onclick={onClose}>Close</button>
    </div>
    <p class="card-sub">
      Any machine running PipeWire (desktop, laptop, another Pi) can feed this source. You load
      <code>libpipewire-module-rtp-sink</code> on that machine: it appears there as a normal
      sound output called <em>Audio Router (RTP)</em>, and everything played into it is streamed
      here over RTP/UDP, landing on the <code>bt-bridge-rtp</code> node in the routing matrix.
      Nothing is sent while nothing plays.
    </p>

    <section>
      <h3>Before you start</h3>
      <ul>
        <li>PipeWire <strong>0.3.60 or newer</strong> on the sender (<code>pipewire --version</code>) — that's when
          <code>module-rtp-sink</code> arrived.</li>
        <li>This source is <strong>enabled</strong> (the button in the card corner) and listening on
          <strong>:{port}</strong>. The sender must target that exact port.</li>
        <li>The sender can reach this host on <strong>UDP :{port}</strong> — same LAN, and no firewall in
          between dropping it.</li>
        <li>Nothing needs SAP or mDNS: this receiver listens statically on the port, so the sender
          doesn't have to announce itself.</li>
      </ul>
    </section>

    <section>
      <h3>The wire format has to match exactly</h3>
      <p>
        There is no negotiation in either direction — the receiver decodes whatever arrives using
        fixed settings. Get one of these wrong and you get noise, not silence:
      </p>
      <table>
        <thead>
          <tr><th>Property</th><th>Value</th><th>If it doesn't match</th></tr>
        </thead>
        <tbody>
          <tr>
            <td><code>audio.format</code></td>
            <td><strong>S16LE</strong></td>
            <td>
              Loud static. Note PipeWire's own example config suggests <code>S16BE</code>
              (RFC 3551 <code>L16</code>, big-endian) — this receiver wants native-endian
              <strong>little</strong>-endian.
            </td>
          </tr>
          <tr>
            <td><code>audio.rate</code></td>
            <td><strong>{rate}</strong></td>
            <td>
              Audio plays at the wrong speed and pitch. This is the <em>Sample rate</em> under
              Advanced in this card — change it there if the sender can only transmit the other
              rate, and keep the two equal either way.
            </td>
          </tr>
          <tr>
            <td><code>audio.channels</code></td>
            <td><strong>2</strong></td>
            <td>Garbled / swapped audio. Mono sources are up-mixed on the sender.</td>
          </tr>
          <tr>
            <td><code>destination.port</code></td>
            <td><strong>{port}</strong></td>
            <td>Nothing arrives at all — the level meter in this card stays flat.</td>
          </tr>
          <tr>
            <td><code>destination.ip</code></td>
            <td><strong>{destIp}</strong></td>
            <td>
              {#if multicast}
                The multicast group this receiver joined. Every receiver on the group shares the
                one stream.
              {:else}
                This add-on's host. A fixed IP is safer than an mDNS name if the sender's resolver
                is unreliable.
              {/if}
            </td>
          </tr>
          <tr>
            <td><code>media.class</code></td>
            <td><strong>Audio/Sink</strong></td>
            <td>
              The module never starts — it comes up as a stream looking for a default sink and
              fails with <code>no target node available</code>. Must be set explicitly.
            </td>
          </tr>
        </tbody>
      </table>
    </section>

    <section>
      <div class="sec-head">
        <h3>1. Load the sender module (persistent)</h3>
        <button class="ghost" type="button" onclick={() => copy(dropIn)}>Copy</button>
      </div>
      <p>
        Drop this into <code>~/.config/pipewire/pipewire.conf.d/</code> on the sending machine
        (or <code>/etc/pipewire/pipewire.conf.d/</code> for every user), then restart its audio
        stack. This is the same configuration the project's Raspberry&nbsp;Pi Bluetooth bridge uses.
      </p>
      <pre>{dropIn}</pre>
      <pre>systemctl --user restart pipewire pipewire-pulse wireplumber</pre>
      <p class="hint">
        Uncomment <code>local.ifname</code> if the machine has several interfaces (or a VPN) and the
        stream should leave over a specific one.
      </p>
    </section>

    <section>
      <div class="sec-head">
        <h3>… or try it without touching any files</h3>
        <button class="ghost" type="button" onclick={() => copy(oneLiner)}>Copy</button>
      </div>
      <p>
        A hot-loaded module is owned by the process that loaded it, so keep <code>pw-cli</code>
        running (<code>-m</code>) — the sink disappears the moment you press Ctrl-C. Good for a
        five-minute test, no good across a reboot.
      </p>
      <pre>{oneLiner}</pre>
    </section>

    <section>
      <h3>2. Play something into it</h3>
      <p>The new sink behaves like any other output on the sending machine:</p>
      <pre>wpctl status                      # find "Audio Router (RTP)"
wpctl set-default &lt;id&gt;            # send everything there
pactl set-default-sink audio-router-rtp   # same thing, PulseAudio-style</pre>
      <p>
        For a single application, move its stream in <code>pavucontrol</code> → <em>Playback</em>.
        To send a copy of what the machine's own speakers are playing, mirror that sink's monitor
        instead — leave the local speakers as the default, or the loopback feeds the RTP sink into
        itself:
      </p>
      <pre>pactl load-module module-loopback \
  source=@DEFAULT_SINK@.monitor sink=audio-router-rtp latency_msec=100</pre>
    </section>

    <section>
      <h3>3. Route it here</h3>
      <p>
        Once packets arrive, <code>bt-bridge-rtp</code> shows up as a source in
        <strong>Routing</strong> and the level meter in this card moves. Audio only reaches speakers
        after you link that source to an output — receiving alone plays nothing.
      </p>
      <ul>
        <li>
          <strong>Source mode:</strong> keep
          <em>Accept all senders</em> for a PipeWire sender. PipeWire picks a fresh random SSRC each
          time the module loads, so <em>Only one client</em> latches onto the old one and rejects the
          sender after it restarts.
          {#if !ignoreSsrc}
            <span class="badge warn">currently: Only one client</span>
          {/if}
        </li>
        <li>
          <strong>Jitter buffer</strong> (Advanced, currently {latencyMsec}&nbsp;ms) absorbs network
          jitter on this end. On the sender, <code>sess.min-ptime</code> /
          <code>sess.max-ptime</code> set how much audio goes in each packet — smaller is lower
          latency and more packets per second.
        </li>
        {#if multicast}
          <li>
            <strong>Multicast:</strong> point every sender and receiver at
            <code>{sourceAddr}</code>. <code>net.ttl</code> must be ≥&nbsp;2 if the stream crosses a
            router or VLAN (PipeWire's default of 1 keeps it in the local subnet). Add
            <code>net.loop = true</code> only if the sending machine should also play it locally.
          </li>
        {:else}
          <li>
            <strong>Several senders:</strong> only one unicast sender at a time — two streams on one
            port interleave into corrupted audio. For several receivers of one stream, switch this
            card to <em>Multicast group</em> and aim the senders at the group instead.
          </li>
        {/if}
      </ul>
    </section>

    <section>
      <h3>Verifying</h3>
      <pre>pw-cli ls Node | grep audio-router-rtp   # on the sender: the sink exists
pw-top                                   # its rate is non-zero while playing</pre>
      <p class="hint">
        Don't trust <code>tcpdump</code> on the sender's Wi-Fi interface to prove egress — some
        drivers offload locally-generated traffic past the capture hook and show zero packets while
        audio is flowing. Use the interface's <code>tx_packets</code> counter, or just watch the
        level meter here.
      </p>
    </section>

    <section>
      <h3>When it doesn't work</h3>
      <table>
        <thead>
          <tr><th>Symptom</th><th>Likely cause</th></tr>
        </thead>
        <tbody>
          <tr>
            <td>Meter never moves</td>
            <td>
              Port mismatch, a firewall between the two, sender and add-on on different subnets
              (<code>net.ttl</code>), or nothing actually routed into the sender's sink.
            </td>
          </tr>
          <tr>
            <td>Loud static / white noise</td>
            <td><code>audio.format</code> is <code>S16BE</code> — it must be <code>S16LE</code>.</td>
          </tr>
          <tr>
            <td>Wrong pitch, too fast or slow</td>
            <td>
              <code>audio.rate</code> differs from the <em>Sample rate</em> under Advanced
              (currently {rate}).
            </td>
          </tr>
          <tr>
            <td><code>no target node available</code> in the sender's log</td>
            <td><code>media.class = "Audio/Sink"</code> is missing from <code>stream.props</code>.</td>
          </tr>
          <tr>
            <td>Sink vanished after a restart</td>
            <td>It was loaded via <code>pw-cli</code>; use the drop-in file to make it permanent.</td>
          </tr>
          <tr>
            <td>Worked once, silent after the sender restarted</td>
            <td>
              <em>Only one client</em> is holding the previous SSRC. Press <strong>Save</strong> here
              to re-latch, or switch to <em>Accept all senders</em>.
            </td>
          </tr>
          <tr>
            <td>Stutter or dropouts</td>
            <td>
              Raise the jitter buffer under Advanced; on Wi-Fi senders also check for power-save
              parking the radio.
            </td>
          </tr>
        </tbody>
      </table>
    </section>
  </div>
</div>

<style>
  /* Same dialog chrome as the other modals in this tab, one size wider — this
     one is a document, not a short table. */
  .modal-card.wide {
    width: min(880px, 100%);
  }
  .card-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
    margin-bottom: 4px;
  }
  .card-head h2 {
    margin: 0;
  }

  section {
    margin-top: 20px;
    padding-top: 12px;
    border-top: 1px solid var(--divider-color);
  }
  section h3 {
    margin: 0 0 8px;
    font-size: 0.95rem;
    font-weight: 500;
  }
  /* Section title with its Copy button pushed to the right. */
  .sec-head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px;
  }
  .sec-head h3 {
    margin: 0;
  }
  .sec-head button {
    flex: 0 0 auto;
    padding: 4px 10px;
    font-size: 0.78rem;
  }
  section p,
  section li {
    font-size: 0.85rem;
    color: var(--secondary-text-color);
  }
  section p {
    margin: 8px 0;
  }
  section ul {
    margin: 8px 0;
    padding-left: 1.2rem;
  }
  section li {
    margin-bottom: 6px;
  }
  section strong,
  section em {
    color: var(--primary-text-color);
  }
  .hint {
    font-size: 0.78rem;
  }
  /* Config/command blocks: wide content scrolls inside the block rather than
     stretching the dialog. */
  pre {
    margin: 8px 0;
    padding: 10px 12px;
    background: var(--input-fill-color);
    border: 1px solid var(--divider-color);
    border-radius: 8px;
    font-family: 'Roboto Mono', ui-monospace, monospace;
    font-size: 0.78rem;
    line-height: 1.5;
    color: var(--primary-text-color);
    overflow-x: auto;
    white-space: pre;
  }
  section table {
    font-size: 0.82rem;
  }
  section td {
    color: var(--secondary-text-color);
    vertical-align: top;
  }
  section td:first-child {
    white-space: nowrap;
    color: var(--primary-text-color);
  }
  .badge.warn {
    margin-left: 0.3rem;
  }
</style>
