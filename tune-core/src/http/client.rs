use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

/// Shared rustls client config trusting the bundled webpki root CAs.
///
/// reqwest's `rustls` feature defaults to `rustls-platform-verifier` for root
/// trust. On Android that verifier needs JNI initialisation (a `Context` /
/// `JavaVM`) which the FFI build (`libtuneserver.so`) never performs, so the
/// first HTTPS request aborts the process with
/// `Expect rustls-platform-verifier to be initialized`.
///
/// Passing this config through `reqwest`'s `use_preconfigured_tls` overrides the
/// platform verifier with webpki roots, which need no platform init and behave
/// identically on desktop and Android. Every client MUST be built via
/// [`builder`] / [`blocking_builder`] for this to hold.
static TLS_CONFIG: LazyLock<rustls::ClientConfig> = LazyLock::new(|| {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    // Build with an explicit aws-lc-rs provider (matches reqwest's `rustls`
    // feature) rather than relying on a process-default being installed.
    rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("aws-lc-rs supports the default protocol versions")
    .with_root_certificates(roots)
    .with_no_client_auth()
});

/// A `reqwest` client builder preconfigured with webpki-root TLS. Use this
/// instead of `reqwest::Client::builder()` everywhere.
pub fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().use_preconfigured_tls(TLS_CONFIG.clone())
}

/// Blocking variant of [`builder`].
pub fn blocking_builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder().use_preconfigured_tls(TLS_CONFIG.clone())
}

static SHARED_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    builder()
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(Duration::from_secs(60))
        .build()
        .expect("failed to create shared HTTP client")
});

static LONG_TIMEOUT_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    builder()
        .timeout(Duration::from_secs(600))
        .pool_max_idle_per_host(2)
        .pool_idle_timeout(Duration::from_secs(60))
        .build()
        .expect("failed to create long-timeout HTTP client")
});

/// Client with no total timeout — for infinite streams like radio.
/// Only the connect timeout is set (10s) to fail fast on unreachable hosts.
/// No `.timeout()` call = no total timeout (reqwest default).
static INFINITE_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    builder()
        .connect_timeout(Duration::from_secs(10))
        .pool_max_idle_per_host(2)
        .pool_idle_timeout(Duration::from_secs(120))
        .build()
        .expect("failed to create infinite-stream HTTP client")
});

pub fn shared() -> &'static reqwest::Client {
    &SHARED_CLIENT
}

pub fn long_timeout() -> &'static reqwest::Client {
    &LONG_TIMEOUT_CLIENT
}

/// Client for infinite streams (radio). No total timeout, only connect timeout.
pub fn infinite_stream() -> &'static reqwest::Client {
    &INFINITE_CLIENT
}
