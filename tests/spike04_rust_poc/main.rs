use pipewire as pw;
use pw::types::ObjectType;
use std::cell::{Cell, OnceCell};
use std::rc::Rc;
use std::time::Instant;

fn do_roundtrip(mainloop: &pw::main_loop::MainLoopRc, core: &pw::core::CoreRc) {
    let done = Rc::new(Cell::new(false));
    let done_clone = done.clone();
    let loop_clone = mainloop.clone();
    let pending = core.sync(0).expect("sync failed");
    let _listener_core = core
        .add_listener_local()
        .done(move |id, seq| {
            if id == pw::core::PW_ID_CORE && seq == pending {
                done_clone.set(true);
                loop_clone.quit();
            }
        })
        .register();
    while !done.get() {
        mainloop.run();
    }
}

fn main() {
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None).expect("mainloop");
    let context = pw::context::ContextRc::new(&mainloop, None).expect("context");
    let core = context.connect_rc(None).expect("connect to real pipewire daemon");

    // Phase 1: find the node ids for the two test nodes, and the link factory name.
    let registry = core.get_registry().expect("registry");
    let source_node_id: Rc<OnceCell<u32>> = Rc::new(OnceCell::new());
    let sink_node_id: Rc<OnceCell<u32>> = Rc::new(OnceCell::new());
    let link_factory: Rc<OnceCell<String>> = Rc::new(OnceCell::new());
    {
        let source_node_id = source_node_id.clone();
        let sink_node_id = sink_node_id.clone();
        let link_factory = link_factory.clone();
        let reg_listener = registry
            .add_listener_local()
            .global(move |global| {
                if let Some(props) = global.props {
                    if props.get("factory.type.name") == Some(ObjectType::Link.to_str()) {
                        let _ = link_factory.set(props.get("factory.name").unwrap().to_owned());
                    }
                    if props.get("node.name") == Some("rust-spike-source") {
                        let _ = source_node_id.set(global.id);
                    }
                    if props.get("node.name") == Some("rust-spike-sink") {
                        let _ = sink_node_id.set(global.id);
                    }
                }
            })
            .register();
        do_roundtrip(&mainloop, &core);
        std::mem::drop(reg_listener);
    }
    let source_node_id = *source_node_id.get().expect("source node not found");
    let sink_node_id = *sink_node_id.get().expect("sink node not found");
    let factory_name = link_factory.get().expect("no link factory found").clone();
    println!("source_node_id={source_node_id} sink_node_id={sink_node_id} link_factory={factory_name}");

    // Phase 2: fresh registry listener (replays all current globals) to find
    // the specific ports belonging to those two node ids.
    let registry2 = core.get_registry().expect("registry2");
    let out_port: Rc<OnceCell<u32>> = Rc::new(OnceCell::new());
    let in_port: Rc<OnceCell<u32>> = Rc::new(OnceCell::new());
    {
        let out_port = out_port.clone();
        let in_port = in_port.clone();
        let source_node_id_str = source_node_id.to_string();
        let sink_node_id_str = sink_node_id.to_string();
        let reg_listener2 = registry2
            .add_listener_local()
            .global(move |global| {
                if let Some(props) = global.props {
                    if props.get("node.id") == Some(source_node_id_str.as_str())
                        && props.get("port.name") == Some("capture_FL")
                    {
                        let _ = out_port.set(global.id);
                    }
                    if props.get("node.id") == Some(sink_node_id_str.as_str())
                        && props.get("port.name") == Some("playback_FL")
                    {
                        let _ = in_port.set(global.id);
                    }
                }
            })
            .register();
        do_roundtrip(&mainloop, &core);
        std::mem::drop(reg_listener2);
    }
    let out_port = *out_port.get().expect("source capture_FL port not found");
    let in_port = *in_port.get().expect("sink playback_FL port not found");
    println!("out_port={out_port} in_port={in_port}");

    // Phase 3: measure native link create+destroy latency, same operation
    // spike 4's shell-based test measured via pw-link, for direct comparison.
    const ITERATIONS: u32 = 20;
    let mut total_ns: u128 = 0;
    for _ in 0..ITERATIONS {
        let t0 = Instant::now();
        let link = core
            .create_object::<pw::link::Link>(
                &factory_name,
                &pw::properties::properties! {
                    "link.output.port" => out_port.to_string(),
                    "link.input.port" => in_port.to_string(),
                    "object.linger" => "1"
                },
            )
            .expect("create link failed");
        do_roundtrip(&mainloop, &core);
        core.destroy_object(link).expect("destroy link failed");
        do_roundtrip(&mainloop, &core);
        total_ns += t0.elapsed().as_nanos();
    }
    let avg_ms = (total_ns / ITERATIONS as u128) as f64 / 1_000_000.0;
    println!(
        "PASS: native pipewire-rs create+destroy link round trip avg over {ITERATIONS} iterations: {avg_ms:.2}ms"
    );
}
