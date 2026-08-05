//! `pwrouter-agent` — the receiver-side helper for pw-sink targets.
//!
//! See docs/receiver-agent-plan.md. The helper runs in a user's session, dials
//! *out* to the add-on, becomes the receiver for one router session (loading
//! `libpipewire-module-rtp-session` itself, so the host needs no config file), and
//! accepts a fixed command set: volume, mute, duck, unduck.
//!
//! Commands:
//!
//! * `run` — the service (see the shipped systemd user unit). Pairs on first
//!   start, then reconnects on its own.
//! * `autostart` — install or remove the systemd user unit, which is embedded in
//!   this binary (plan §8.1). The tray menu's Autostart switch is the same code.
//! * `spike-receiver` / `spike-volume` — the standalone spikes from plan §11,
//!   kept because they are the fastest way to diagnose a host by hand.
//! * `spike-desktop` — does this session show a tray icon and a notification?
//!   Answers that without involving the add-on at all.

mod autostart;
mod client;
mod config;
mod desktop;
mod proto;
mod pw_thread;
mod receiver;
mod volume;

use anyhow::{bail, Context as _};
use pipewire as pw;
use pw_control::module::LoadedModule;

const USAGE: &str = "\
pwrouter-agent — receiver-side helper for the PipeWire audio router add-on

USAGE:
  pwrouter-agent run             [--daemon <host:port>]
  pwrouter-agent autostart       [enable | disable]
  pwrouter-agent spike-receiver  [--ifname <iface>] [--node-name <name>]
  pwrouter-agent spike-volume    [--node-name <name>] [--session <rtp.session>] [--set <0.0-1.0>]
  pwrouter-agent spike-desktop   [--code <XXXXXX>]

`run` discovers the add-on over mDNS unless --daemon is given, and pairs on first
start (approve the request in the add-on UI; it prints a code to match — to the log,
and to a desktop notification and tray icon where the session has them).

`autostart` writes (or removes) the systemd user unit that starts this binary at
login; with no argument it reports the current state. The unit is built in, so
nothing has to be downloaded. Same switch as the tray menu's Autostart entry.
";

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first().map(String::as_str) else {
        print!("{USAGE}");
        return Ok(());
    };
    let opt = |name: &str| -> Option<String> {
        args.iter()
            .position(|a| a == name)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };

    match cmd {
        "run" => run(opt("--daemon")),
        "autostart" => autostart_cmd(args.get(1).map(String::as_str)),
        "spike-receiver" => spike_receiver(
            opt("--ifname"),
            opt("--node-name").unwrap_or_else(|| receiver::RECEIVE_NODE_NAME.into()),
        ),
        "spike-desktop" => spike_desktop(opt("--code").unwrap_or_else(|| "4F2A9C".into())),
        "spike-volume" => {
            let set = match opt("--set") {
                Some(v) => Some(v.parse::<f32>().context("--set expects a float 0.0-1.0")?),
                None => None,
            };
            spike_volume(
                opt("--node-name").unwrap_or_else(|| receiver::RECEIVE_NODE_NAME.into()),
                opt("--session"),
                set,
            )
        }
        other => {
            print!("{USAGE}");
            bail!("unknown command: {other}")
        }
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();
}

/// The service: PipeWire worker on its own thread, control plane on tokio.
fn run(daemon: Option<String>) -> anyhow::Result<()> {
    init_tracing();
    tracing::info!(
        "pwrouter-agent {} starting as {} ({})",
        env!("CARGO_PKG_VERSION"),
        config::label(),
        config::identity()
    );

    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    // Spawned before the runtime so a PipeWire failure is a plain startup error.
    let handle = pw_thread::spawn(event_tx)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = client::run(handle, event_rx, daemon) => result,
            _ = tokio::signal::ctrl_c() => {
                tracing::info!("interrupted; restoring host state");
                Ok(())
            }
            _ = sigterm.recv() => {
                tracing::info!("SIGTERM; restoring host state");
                Ok(())
            }
        }
        // Either arm drops the `Handle`, whose Drop restores ducked streams and
        // unloads the receiver module on the PipeWire thread (plan §9.1).
    })
}

/// `autostart [enable|disable]` — the command-line half of the tray's Autostart
/// switch. No PipeWire and no add-on involved: it writes a file and talks to
/// systemd.
fn autostart_cmd(action: Option<&str>) -> anyhow::Result<()> {
    init_tracing();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let path = autostart::unit_path()?;
        match action {
            None | Some("status") => {
                println!("autostart: {}", autostart::state().await.label());
                println!("unit:      {}", path.display());
                println!("\nchange it with: pwrouter-agent autostart enable|disable");
            }
            Some("enable") => {
                let path = autostart::enable().await?;
                println!("wrote {}", path.display());
                for line in autostart::unit_text().lines() {
                    if let Some(exec) = line.strip_prefix("ExecStart=") {
                        println!("it starts: {exec}");
                    }
                }
                println!(
                    "enabled; it will start at your next login.\n\
                     to start it now as well: systemctl --user start {}",
                    autostart::UNIT_NAME
                );
            }
            Some("disable") => {
                autostart::disable().await?;
                println!("disabled and removed {}", path.display());
                println!(
                    "an agent already running keeps running; stop it with: \
                     systemctl --user stop {}",
                    autostart::UNIT_NAME
                );
            }
            Some(other) => {
                print!("{USAGE}");
                bail!("unknown autostart action: {other}")
            }
        }
        Ok(())
    })
}

/// Does this desktop show the tray icon and the pairing notification? Answers that
/// without touching the add-on: no pairing request is made, so nothing appears in
/// anyone's UI and no token can change. Ctrl-C to stop.
fn spike_desktop(code: String) -> anyhow::Result<()> {
    init_tracing();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        println!("posting a pairing notification and a tray icon for the fake code {code}");
        println!("(nothing is sent to the add-on; run with RUST_LOG=debug to see why either is missing)\n");
        let desktop = desktop::Desktop::start(config::label(), Some(code.clone()), false).await;
        desktop.set_daemon(Some("192.0.2.1:8099".into())).await;
        desktop.pair_pending(&code).await;
        println!("shown; Ctrl-C to stop");
        tokio::signal::ctrl_c().await?;
        Ok(())
    })
}

/// S1: become the receiver with no config file.
fn spike_receiver(ifname: Option<String>, node_name: String) -> anyhow::Result<()> {
    pw::init();
    let mainloop =
        pw::main_loop::MainLoopRc::new(None).map_err(|e| anyhow::anyhow!("mainloop: {e}"))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| anyhow::anyhow!("context: {e}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|e| anyhow::anyhow!("connect to PipeWire: {e}"))?;
    let registry = core
        .get_registry_rc()
        .map_err(|e| anyhow::anyhow!("get registry: {e}"))?;

    // Log every node/link the module creates. `rtp.session` is set per discovered
    // session (module-rtp-session's make_session), so a host receiving two
    // routers' sessions gets two distinguishable stream nodes — that is the hook
    // the agent uses for scoping (plan §7.1).
    let _listener = registry
        .add_listener_local()
        .global(|global| {
            let Some(props) = global.props else { return };
            match global.type_ {
                pw::types::ObjectType::Node => {
                    let name = props.get("node.name").unwrap_or("?");
                    let class = props.get("media.class").unwrap_or("-");
                    println!("node  id={:<5} name={name:<24} class={class}", global.id);
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

    let args = receiver::rtp_session_module_args(
        ifname.as_deref(),
        &node_name,
        receiver::RECEIVE_NODE_DESCRIPTION,
        None,
    );
    println!(
        "loading {} with args:\n  {args}\n",
        receiver::RTP_SESSION_MODULE_NAME
    );
    // SAFETY: we are on the thread that owns `context` (created here, loop not
    // handed elsewhere), which is pw_module's contract. `_module` is dropped at the
    // end of this function, on this thread.
    let _module = unsafe {
        LoadedModule::load(
            context.as_raw_ptr(),
            receiver::RTP_SESSION_MODULE_NAME,
            &args,
        )
    }
    .map_err(|e| anyhow::anyhow!("load {}: {e}", receiver::RTP_SESSION_MODULE_NAME))?;
    println!("module loaded; running (Ctrl-C or SIGTERM to stop)\n");

    mainloop.run();
    Ok(())
}

/// S2/S2b: the master volume of the sink our receive stream actually feeds.
fn spike_volume(
    node_name: String,
    session: Option<String>,
    set: Option<f32>,
) -> anyhow::Result<()> {
    let graph = volume::Graph::snapshot()?;

    let stream = graph
        .find_receive_stream(&node_name, session.as_deref())
        .with_context(|| match &session {
            Some(s) => format!("no receive stream node with rtp.session='{s}' (is the daemon streaming to this host?)"),
            None => format!("no receive stream node named '{node_name}'"),
        })?;
    println!(
        "receive stream: id={} name={} rtp.session={}",
        stream.id,
        stream.name,
        stream.session.as_deref().unwrap_or("-")
    );

    let sink_id = graph
        .linked_sink(stream.id)
        .context("receive stream is not linked to any sink (WirePlumber has not routed it yet?)")?;
    let sink = graph.node(sink_id);
    println!(
        "target sink:    id={sink_id} name={} class={}",
        sink.map(|n| n.name.as_str()).unwrap_or("?"),
        sink.and_then(|n| n.media_class.as_deref()).unwrap_or("-")
    );

    match volume::master_lever(sink_id)? {
        Some(lever) => {
            println!("lever:          {}", lever.describe());
            match lever.read()? {
                Some(props) => println!(
                    "volume:         {:.3} (cubic){}",
                    props.cubic().unwrap_or(0.0),
                    match props.mute {
                        Some(true) => "  [muted]",
                        Some(false) => "  [unmuted]",
                        None => "",
                    }
                ),
                None => println!("volume:         <unreadable>"),
            }
            if let Some(v) = set {
                lever.write(Some(v), None)?;
                println!("set volume to   {v:.3}");
                if let Some(props) = lever.read()? {
                    println!("read back:      {:.3}", props.cubic().unwrap_or(0.0));
                }
            }
        }
        None => println!("lever:          <none: sink has neither a device route nor node volume>"),
    }
    Ok(())
}
