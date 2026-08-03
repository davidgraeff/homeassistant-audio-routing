use std::path::PathBuf;

// Compiles the vendored libairptp (OwnTone's MIT AirPlay-2 gPTP grandmaster,
// vendor/libairptp/) into the daemon and links its runtime deps. libairptp owns
// the PTP protocol on UDP 319/320 for the AirPlay-2 sender (see src/ap2_ptp.rs).
fn main() {
    let base = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("vendor/libairptp");
    let src = base.join("src");

    println!("cargo:rerun-if-changed={}", src.display());
    println!("cargo:rerun-if-changed={}", base.join("config.h").display());
    println!("cargo:rerun-if-changed={}", base.join("airptp.h").display());

    cc::Build::new()
        .include(&base) // config.h, airptp.h
        .include(&src) // internal headers
        .define("HAVE_CONFIG_H", None)
        .define("_GNU_SOURCE", None)
        .flag_if_supported("-Wno-unused-parameter")
        .flag_if_supported("-Wno-sign-compare")
        .flag_if_supported("-Wno-unused-variable")
        // Upstream writes `const char __thread *x`; GCC wants the storage class
        // first. Suppressed rather than patched, like the warnings above, so the
        // vendored tree stays diffable against OwnTone.
        .flag_if_supported("-Wno-old-style-declaration")
        .file(src.join("airptp.c"))
        .file(src.join("utils.c"))
        .file(src.join("daemon.c"))
        .file(src.join("ptp_msg_handle.c"))
        .file(src.join("airptp_shim.c")) // LOCAL: names the worker + routes logs
        .compile("airptp"); // -> libairptp.a, auto-linked static

    // libairptp's runtime deps. libevent headers live in the default include
    // path (libevent-devel on Fedora / libevent-dev on Debian / libevent-dev on
    // Alpine); the add-on build image must provide them (see build-daemon.sh).
    println!("cargo:rustc-link-lib=event");
    println!("cargo:rustc-link-lib=event_pthreads");
    println!("cargo:rustc-link-lib=pthread");
    println!("cargo:rustc-link-lib=rt");
}
