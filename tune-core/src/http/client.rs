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

/// Nombre d'en-têtes acceptés dans une réponse HTTP/1 (#5522).
///
/// hyper en refuse plus de 100 par défaut (`message head is too large`). Le
/// CDN Qobuz (Akamai) ajoute une paire `X-AK-FWD-ERROR` / `X-AK-GRN` à chaque
/// nouvel essai interne : mesuré le 11/10/2026, **108 en-têtes** sur la sonde
/// Range de la piste 921595, qui ne pouvait donc jamais être lue. La marge
/// couvre un CDN qui réessaie dix fois plus ; la taille totale de l'en-tête
/// reste bornée par hyper (environ 400 Kio).
pub const MAX_ENTETES_HTTP1: usize = 1024;

/// A `reqwest` client builder preconfigured with webpki-root TLS. Use this
/// instead of `reqwest::Client::builder()` everywhere.
pub fn builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .use_preconfigured_tls(TLS_CONFIG.clone())
        .http1_max_headers(MAX_ENTETES_HTTP1)
}

/// #9450 — le `User-Agent` d'un client INTERNE de Tune, celui qui lit un flux
/// que Tune sert lui-même (`/stream/<id>`) : sondes, décodeurs des sorties qui
/// décodent, lecture par plages. Sans lui, ces connexions s'écrivaient
/// `agent=-` dans les journaux du serveur de flux, indiscernables d'un
/// renderer muet. `role` dit qui tire (`niveaux`, `airplay`…).
pub fn agent_interne(role: &str) -> String {
    format!("Tune/{} (interne; {role})", env!("CARGO_PKG_VERSION"))
}

/// L'adresse vise-t-elle un flux que Tune sert lui-même (`http://…/stream/<id>`) ?
/// Les flux de Tune sont en HTTP clair ; une adresse de CDN (HTTPS) garde ses
/// en-têtes d'origine, rien n'y est ajouté.
pub fn vise_un_flux_de_tune(url: &str) -> bool {
    url.starts_with("http://") && crate::http::streamer::stream_id_de_l_url(url).is_some()
}

/// Le constructeur bloquant, avec l'agent interne `role` quand `url` est un
/// flux de Tune ([`vise_un_flux_de_tune`]), sans rien changer sinon.
pub fn blocking_builder_pour(url: &str, role: &str) -> reqwest::blocking::ClientBuilder {
    let b = blocking_builder();
    if vise_un_flux_de_tune(url) {
        b.user_agent(agent_interne(role))
    } else {
        b
    }
}

/// Une requête asynchrone marquée de l'agent interne `role` quand `url` est un
/// flux de Tune ([`vise_un_flux_de_tune`]), inchangée sinon.
pub fn marquer_si_flux_de_tune(
    requete: reqwest::RequestBuilder,
    url: &str,
    role: &str,
) -> reqwest::RequestBuilder {
    if vise_un_flux_de_tune(url) {
        requete.header(reqwest::header::USER_AGENT, agent_interne(role))
    } else {
        requete
    }
}

/// L'agent interne est-il celui d'un client de Tune ?
pub fn est_un_agent_interne(agent: &str) -> bool {
    agent.starts_with("Tune/") && agent.contains("(interne;")
}

/// Blocking variant of [`builder`].
pub fn blocking_builder() -> reqwest::blocking::ClientBuilder {
    reqwest::blocking::Client::builder()
        .use_preconfigured_tls(TLS_CONFIG.clone())
        .http1_max_headers(MAX_ENTETES_HTTP1)
}

/// Décrit une erreur `reqwest` pour un journal, **sans son URL** et avec la
/// chaîne de ses causes (#5553, #5522).
///
/// Une URL de CDN signée porte ses paramètres d'autorisation : elle ne doit
/// pas finir dans un journal qu'un testeur publie. `reqwest::Error` l'écrit
/// pourtant dans son `Display` (« error sending request for url (…) »), et
/// s'arrête là : la cause réelle — connexion fermée avant la réponse, remise
/// à zéro, TLS — n'apparaît que dans ses `source()`. Ce texte garde la nature
/// de l'erreur, ajoute chaque cause, et laisse l'URL de côté : l'URL vit à
/// part dans l'erreur, aucune cause ne la recopie.
pub fn decrire_erreur_http(error: &reqwest::Error) -> String {
    let nature = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection failed"
    } else if error.is_body() {
        "response body failed"
    } else if error.is_decode() {
        "response decode failed"
    } else {
        "request failed"
    };
    let mut texte = nature.to_string();
    let mut cause = std::error::Error::source(error);
    while let Some(c) = cause {
        texte.push_str(": ");
        texte.push_str(&c.to_string());
        cause = c.source();
    }
    texte
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

#[cfg(test)]
#[path = "client_tests_5522.rs"]
mod tests_5522;
