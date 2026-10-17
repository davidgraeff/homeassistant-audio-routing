//! The process-wide rustls crypto provider.
//!
//! Two rustls backends are compiled into this binary: `aws-lc-rs` (through
//! `reqwest`) and `ring` (through librespot's `hyper-rustls`). With more than one
//! available, rustls will not choose a process default, and every TLS client that
//! relies on the default panics on its first handshake — which is what a Spotify
//! login did, killing the device's task mid-connect. So one is installed
//! explicitly, before anything opens a connection: `aws-lc-rs`, which is what
//! `reqwest` resolved to while it was the only backend in the tree.

/// Install `aws-lc-rs` as the default provider. Idempotent: a provider that is
/// already installed (a second call, or a test that got there first) is kept.
pub(crate) fn install_crypto_provider() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_default_provider_exists_once_installed() {
        super::install_crypto_provider();
        super::install_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
        // The builder is what panicked: it consults the default provider.
        let _ = rustls::ClientConfig::builder();
    }
}
