use std::sync::LazyLock;
use std::time::Duration;

static SHARED_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(Duration::from_secs(60))
        .build()
        .expect("failed to create shared HTTP client")
});

static LONG_TIMEOUT_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
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
    reqwest::Client::builder()
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
