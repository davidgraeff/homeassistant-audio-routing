//! `pwrouter-agent` — the receiver-side helper for pw-sink targets.
//!
//! See docs/receiver-agent-plan.md. Right now this binary is **spikes only**;
//! the pairing/WS control plane (P1) is not here yet.
//!
//! * `spike-receiver` — proves the agent can *be* the receiver by loading
//!   `libpipewire-module-rtp-session` into its own client context, with no
//!   `pipewire.conf.d` drop-in (plan §11 S1). Logs the nodes and links the module
//!   creates, including each session's `rtp.session` property, which is how the
//!   agent tells its own session apart from a stranger's (plan §7 / S1b).
//! * `spike-volume` — proves master volume control: find our receive stream,
//!   follow its link to the sink it actually feeds, read/set `channelVolumes`
//!   (plan §11 S2).
//!
//! Both spikes are run against a live daemon session on the LAN.

mod pw_module;
mod receiver;
mod volume;

use anyhow::{bail, Context as _};
use pipewire as pw;
use pw_module::LoadedModule;

const USAGE: &str = "\
pwrouter-agent (spikes)

USAGE:
  pwrouter-agent spike-receiver [--ifname <iface>] [--node-name <name>]
  pwrouter-agent spike-volume   [--node-name <name>] [--session <rtp.session>] [--set <0.0-1.0>]

spike-receiver runs until killed; the module (and every node it created) is
unloaded on a clean exit.
";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        print!("{USAGE}");
        return Ok(());
    };
    let opt = |name: &str| -> Option<String> {
        args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
    };

    match cmd {
        "spike-receiver" => spike_receiver(opt("--ifname"), opt("--node-name").unwrap_or_else(|| "pwsink-in".into())),
        "spike-volume" => {
            let set = match opt("--set") {
                Some(v) => Some(v.parse::<f32>().context("--set expects a float 0.0-1.0")?),
                None => None,
            };
            spike_volume(opt("--node-name").unwrap_or_else(|| "pwsink-in".into()), opt("--session"), set)
        }
        other => {
            print!("{USAGE}");
            bail!("unknown command: {other}")
        }
    }
}

/// S1: become the receiver with no config file.
fn spike_receiver(ifname: Option<String>, node_name: String) -> anyhow::Result<()> {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| anyhow::anyhow!("mainloop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| anyhow::anyhow!("context: {e}"))?;
    let core = context.connect_rc(None).map_err(|e| anyhow::anyhow!("connect to PipeWire: {e}"))?;
    let registry = core.get_registry_rc().map_err(|e| anyhow::anyhow!("get registry: {e}"))?;

    // Log every node/link the module creates. `rtp.session` is set per discovered
    // session (module-rtp-session's make_session), so a host receiving two
    // routers' sessions gets two distinguishable stream nodes — that is the hook
    // the agent will use for scoping (plan §7).
    let _listener = registry
        .add_listener_local()
        .global(|global| {
            let Some(props) = global.props else { return };
            match global.type_ {
                pw::types::ObjectType::Node => {
                    let name = props.get("node.name").unwrap_or("?");
                    let class = props.get("media.class").unwrap_or("-");
                    let session = props.get("rtp.session").unwrap_or("-");
                    println!("node  id={:<5} name={name:<24} class={class:<22} rtp.session={session}", global.id);
                }
                pw::types::ObjectType::Link => {
                    println!(
                        "link  id={:<5} {}:{} -> {}:{}",
                        global.id,
                        props.get("link.output.node").unwrap_or("?"),
                        props.get("link.output.port").unwrap_or("?"),
                        props.get("link.input.node").unwrap_or("?"),
                        props.get("link.input.port").unwrap_or("?"),
                    );
                }
                _ => {}
            }
        })
        .global_remove(|id| println!("gone  id={id}"))
        .register();

    let args = receiver::rtp_session_module_args(ifname.as_deref(), &node_name, "pw-router sink");
    println!("loading {} with args:\n  {args}\n", receiver::RTP_SESSION_MODULE_NAME);
    // SAFETY: we are on the thread that owns `context` (it was created here and
    // the loop has not been handed to another thread), which is pw_module's
    // contract. `_module` is dropped at the end of this function, on this thread.
    let _module = unsafe { LoadedModule::load(context.as_raw_ptr(), receiver::RTP_SESSION_MODULE_NAME, &args) }
        .map_err(|e| anyhow::anyhow!("load {}: {e}", receiver::RTP_SESSION_MODULE_NAME))?;
    println!("module loaded; running (Ctrl-C or SIGTERM to stop)\n");

    mainloop.run();
    Ok(())
}

/// S2: master volume of the sink our receive stream actually feeds.
fn spike_volume(node_name: String, session: Option<String>, set: Option<f32>) -> anyhow::Result<()> {
    let graph = volume::Graph::snapshot()?;

    let stream = graph
        .find_receive_stream(&node_name, session.as_deref())
        .with_context(|| match &session {
            Some(s) => format!("no receive stream node with rtp.session='{s}' (is the daemon streaming to this host?)"),
            None => format!("no receive stream node named '{node_name}'"),
        })?;
    println!("receive stream: id={} name={} rtp.session={}", stream.id, stream.name, stream.session.as_deref().unwrap_or("-"));

    let sink_id = graph
        .linked_sink(stream.id)
        .context("receive stream is not linked to any sink (WirePlumber has not routed it yet?)")?;
    let sink = graph.node(sink_id);
    println!(
        "target sink:    id={sink_id} name={} class={}",
        sink.map(|n| n.name.as_str()).unwrap_or("?"),
        sink.and_then(|n| n.media_class.as_deref()).unwrap_or("-")
    );

    match volume::get_volume(sink_id)? {
        Some(v) => println!("volume:         {:.3} (cubic, = wpctl/HA scale)", v),
        None => println!("volume:         <node exposes no channelVolumes>"),
    }

    if let Some(v) = set {
        volume::set_volume(sink_id, v)?;
        println!("set volume to   {v:.3}");
        if let Some(readback) = volume::get_volume(sink_id)? {
            println!("read back:      {readback:.3}");
        }
    }
    Ok(())
}
