//! Shared HTTP fetch utilities for the enrichment paths: a per-key
//! [`RateLimiter`] and a typed [`FetchOutcome`].
//!
//! Enrichment previously spaced its external calls with scattered
//! `tokio::time::sleep(Duration::from_millis(1100))` before each MusicBrainz /
//! Cover Art Archive request. That is fragile (magic numbers duplicated at
//! every call site) and, worse, only spaces requests *within one loop* — two
//! concurrent enrichment tasks (album covers + artist images) could still hit
//! MusicBrainz twice in the same second and earn a 503 block. A shared
//! limiter serialises across all callers.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Plafond de l'intervalle adaptatif : après des `503` répétés, deux requêtes
/// d'une même clé ne s'espacent jamais de plus de 5 s (idée : MetaRust).
pub const INTERVALLE_MAX_ADAPTATIF: Duration = Duration::from_secs(5);

/// Borne d'un `Retry-After` honoré. Un serveur qui demanderait une heure
/// gèlerait toutes les passes qui partagent la clé : au-delà de deux minutes,
/// on attend deux minutes, puis la requête suivante redira ce qu'il en est.
pub const RETRY_AFTER_MAX: Duration = Duration::from_secs(120);

/// L'état d'une clé du limiteur : le prochain créneau libre et l'intervalle
/// COURANT, qui s'écarte de l'intervalle de base après un refus.
#[derive(Debug, Clone, Copy)]
struct EtatCle {
    prochain: Instant,
    intervalle: Duration,
}

/// A minimum-interval limiter keyed by an arbitrary string (typically a host or
/// service name). Each `acquire(key)` reserves the next free time slot for that
/// key and sleeps until it, so N concurrent callers are serialised to one
/// request per interval — with no busy-waiting and no per-call-site magic
/// numbers.
///
/// # Intervalle adaptatif (#4805, idée 4 bis — d'après MetaRust)
///
/// L'intervalle n'est plus fixe : [`RateLimiter::constater`] lit chaque
/// réponse. Un `503` ou un `429` **double** l'intervalle de la clé, jusqu'à
/// [`INTERVALLE_MAX_ADAPTATIF`], et repousse le prochain créneau d'au moins cet
/// intervalle — ou du `Retry-After` du serveur s'il en donne un, borné à
/// [`RETRY_AFTER_MAX`]. Un succès le ramène à l'intervalle de base.
///
/// 🔴 L'état reste PAR CLÉ dans le limiteur PARTAGÉ (#4767) : MetaRust tient un
/// limiteur par client, donc deux tâches y doublent le débit ; ici, un `503`
/// reçu par la passe des pochettes ralentit aussi l'identification, puisque
/// MusicBrainz compte par IP.
pub struct RateLimiter {
    min_interval: Duration,
    etats: Mutex<HashMap<String, EtatCle>>,
}

impl RateLimiter {
    pub fn with_interval(min_interval: Duration) -> Self {
        Self {
            min_interval,
            etats: Mutex::new(HashMap::new()),
        }
    }

    /// Build a limiter allowing at most `rps` requests per second.
    pub fn per_second(rps: f64) -> Self {
        let secs = if rps > 0.0 { 1.0 / rps } else { 0.0 };
        Self::with_interval(Duration::from_secs_f64(secs))
    }

    fn etats(&self) -> std::sync::MutexGuard<'_, HashMap<String, EtatCle>> {
        self.etats.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn etat_neuf(&self, maintenant: Instant) -> EtatCle {
        EtatCle {
            prochain: maintenant,
            intervalle: self.min_interval,
        }
    }

    /// Block until a request for `key` may proceed, reserving that slot so
    /// concurrent callers queue behind it one interval apart.
    pub async fn acquire(&self, key: &str) {
        // Reserve a slot while holding the lock, then release the lock BEFORE
        // sleeping so other keys aren't blocked and same-key callers each get a
        // distinct, monotonically spaced slot.
        let slot = {
            let mut etats = self.etats();
            let now = Instant::now();
            let neuf = self.etat_neuf(now);
            let etat = etats.entry(key.to_string()).or_insert(neuf);
            let slot = etat.prochain.max(now);
            etat.prochain = slot + etat.intervalle;
            slot
        };
        let now = Instant::now();
        if slot > now {
            tokio::time::sleep(slot - now).await;
        }
    }

    /// L'intervalle courant de `key` — l'intervalle de base tant qu'aucun
    /// refus n'est venu.
    pub fn intervalle_courant(&self, key: &str) -> Duration {
        self.etats()
            .get(key)
            .map(|e| e.intervalle)
            .unwrap_or(self.min_interval)
    }

    /// Le service a refusé (`503`, `429`) : l'intervalle double, jusqu'à
    /// [`INTERVALLE_MAX_ADAPTATIF`], et le prochain créneau recule d'au moins
    /// cet intervalle — ou du `Retry-After` reçu, borné à [`RETRY_AFTER_MAX`].
    pub fn signaler_refus(&self, key: &str, retry_after: Option<Duration>) {
        let mut etats = self.etats();
        let now = Instant::now();
        let neuf = self.etat_neuf(now);
        let etat = etats.entry(key.to_string()).or_insert(neuf);
        let double = etat.intervalle.saturating_mul(2);
        etat.intervalle = double
            .max(self.min_interval)
            .min(INTERVALLE_MAX_ADAPTATIF.max(self.min_interval));
        let mut attente = etat.intervalle;
        if let Some(demande) = retry_after {
            attente = attente.max(demande.min(RETRY_AFTER_MAX));
        }
        etat.prochain = etat.prochain.max(now + attente);
        tracing::info!(
            cle = key,
            intervalle_ms = etat.intervalle.as_millis() as u64,
            attente_ms = attente.as_millis() as u64,
            "limiteur_ralenti_apres_refus"
        );
    }

    /// Le service a répondu : l'intervalle revient à sa valeur de base.
    pub fn signaler_succes(&self, key: &str) {
        if let Some(etat) = self.etats().get_mut(key) {
            etat.intervalle = self.min_interval;
        }
    }

    /// Lit le statut d'une réponse et ajuste la clé : `429`/`503` ralentit
    /// (avec le `Retry-After` éventuel), un `2xx` rétablit. Les autres statuts
    /// (`404`, `400`…) ne disent rien de la cadence et ne changent rien.
    pub fn constater(&self, key: &str, statut: u16, retry_after: Option<&str>) {
        match statut {
            429 | 503 => self.signaler_refus(key, retry_after.and_then(lire_retry_after)),
            200..=299 => self.signaler_succes(key),
            _ => {}
        }
    }

    /// [`Self::constater`] sur une réponse `reqwest`.
    pub fn constater_reponse(&self, key: &str, reponse: &reqwest::Response) {
        let retry_after = reponse
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok());
        self.constater(key, reponse.status().as_u16(), retry_after);
    }
}

/// Un en-tête `Retry-After` : un nombre de secondes, ou une date HTTP
/// (RFC 7231, `Wed, 21 Oct 2026 07:28:00 GMT`). Une date passée vaut zéro ;
/// une valeur illisible est ignorée.
pub fn lire_retry_after(valeur: &str) -> Option<Duration> {
    let v = valeur.trim();
    if let Ok(secondes) = v.parse::<u64>() {
        return Some(Duration::from_secs(secondes));
    }
    let date = chrono::DateTime::parse_from_rfc2822(v).ok()?;
    let ecart = date.timestamp() - chrono::Utc::now().timestamp();
    Some(Duration::from_secs(ecart.max(0) as u64))
}

/// Shared limiter for MusicBrainz-operated endpoints (musicbrainz.org and the
/// Cover Art Archive), whose published policy is ~1 request/second. Every MB /
/// CAA call in [`crate::library::artwork`] acquires this before requesting.
pub static MUSICBRAINZ: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(1.0));

/// Shared limiter for LRCLIB (<https://lrclib.net>), le service de paroles.
///
/// C'est un service communautaire **gratuit et sans clé d'API** : la seule
/// protection dont il dispose est la retenue de ses clients. La passe de fond
/// « paroles » (`crate::library::lyrics_pass`) l'acquiert avant chaque requête,
/// à ~1 req/s — un rythme tenable pour une bibliothèque parcourue en fond, et
/// le même que celui déjà appliqué à MusicBrainz.
///
/// La récupération **à la demande** (quand une piste est jouée) ne passe
/// délibérément pas par ce limiteur : elle n'émet qu'une requête et l'utilisateur
/// l'attend. La faire patienter derrière une passe de fond serait la punir.
pub static LRCLIB: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(1.0));

/// Limiteur partagé d'AcoustID (<https://acoustid.org/webservice>) : leurs
/// règles d'usage plafonnent à **3 requêtes par seconde** par application.
/// La passe d'identification par empreinte (#4805) et la route d'une piste
/// (`POST /library/identify`) passent toutes deux par lui, sous
/// [`CLE_ACOUSTID`].
pub static ACOUSTID: LazyLock<RateLimiter> = LazyLock::new(|| RateLimiter::per_second(3.0));

/// La clé unique d'AcoustID dans [`ACOUSTID`] : une clé par SERVICE.
pub const CLE_ACOUSTID: &str = "acoustid";

/// Typed result of fetching a binary resource (e.g. an image), so callers can
/// tell a genuine "not found" from a transient rate-limit or network error.
#[derive(Debug)]
pub enum FetchOutcome {
    /// Body received and at least `min_len` bytes.
    Success(Vec<u8>),
    /// HTTP 404 — the resource does not exist.
    NotFound,
    /// HTTP 429/503 — throttled; the caller should back off, not treat as final.
    RateLimited,
    /// 2xx but the body was smaller than `min_len` (usually an error page).
    TooSmall(usize),
    /// Transport error or any other non-success status.
    Error(String),
}

impl FetchOutcome {
    /// Consume the outcome, yielding the bytes only on success.
    pub fn into_bytes(self) -> Option<Vec<u8>> {
        match self {
            FetchOutcome::Success(b) => Some(b),
            _ => None,
        }
    }

    /// A short static reason for logging (never includes the body).
    pub fn reason(&self) -> &'static str {
        match self {
            FetchOutcome::Success(_) => "success",
            FetchOutcome::NotFound => "not_found",
            FetchOutcome::RateLimited => "rate_limited",
            FetchOutcome::TooSmall(_) => "too_small",
            FetchOutcome::Error(_) => "error",
        }
    }
}

/// Fetch a binary resource, classifying the result. `min_len` rejects tiny
/// bodies (error pages served with a 200) as [`FetchOutcome::TooSmall`].
pub async fn fetch_bytes(client: &reqwest::Client, url: &str, min_len: usize) -> FetchOutcome {
    let resp = match client.get(url).send().await {
        Ok(r) => r,
        Err(e) => return FetchOutcome::Error(e.to_string()),
    };
    let status = resp.status();
    if status.as_u16() == 429 || status.as_u16() == 503 {
        return FetchOutcome::RateLimited;
    }
    if status.as_u16() == 404 {
        return FetchOutcome::NotFound;
    }
    if !status.is_success() {
        return FetchOutcome::Error(format!("http {}", status.as_u16()));
    }
    let bytes = match resp.bytes().await {
        Ok(b) => b,
        Err(e) => return FetchOutcome::Error(e.to_string()),
    };
    if bytes.len() < min_len {
        return FetchOutcome::TooSmall(bytes.len());
    }
    FetchOutcome::Success(bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_outcome_into_bytes_only_on_success() {
        assert_eq!(
            FetchOutcome::Success(vec![1, 2, 3]).into_bytes(),
            Some(vec![1, 2, 3])
        );
        assert_eq!(FetchOutcome::NotFound.into_bytes(), None);
        assert_eq!(FetchOutcome::RateLimited.into_bytes(), None);
        assert_eq!(FetchOutcome::TooSmall(4).into_bytes(), None);
        assert_eq!(FetchOutcome::Error("x".into()).into_bytes(), None);
    }

    #[test]
    fn fetch_outcome_reason_is_stable() {
        assert_eq!(FetchOutcome::RateLimited.reason(), "rate_limited");
        assert_eq!(FetchOutcome::NotFound.reason(), "not_found");
    }

    #[tokio::test]
    async fn rate_limiter_spaces_same_key_by_interval() {
        // Three sequential acquires on the same key must span at least
        // 2 * interval (the first proceeds immediately). Short interval keeps
        // the test fast while asserting a reliable lower bound.
        let rl = RateLimiter::with_interval(Duration::from_millis(50));
        let start = Instant::now();
        rl.acquire("mb").await;
        rl.acquire("mb").await;
        rl.acquire("mb").await;
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(100),
            "3 acquires spaced by 50ms must take >= 100ms, took {elapsed:?}"
        );
    }

    /// #4805 (4 bis) — un 503 double l'intervalle, jusqu'à 5 s, et un succès
    /// le ramène à la base.
    #[test]
    fn un_refus_double_l_intervalle_jusqu_a_cinq_secondes() {
        let rl = RateLimiter::with_interval(Duration::from_secs(1));
        assert_eq!(rl.intervalle_courant("mb"), Duration::from_secs(1));
        rl.constater("mb", 503, None);
        assert_eq!(rl.intervalle_courant("mb"), Duration::from_secs(2));
        rl.constater("mb", 503, None);
        assert_eq!(rl.intervalle_courant("mb"), Duration::from_secs(4));
        rl.constater("mb", 429, None);
        assert_eq!(rl.intervalle_courant("mb"), Duration::from_secs(5));
        rl.constater("mb", 503, None);
        assert_eq!(rl.intervalle_courant("mb"), Duration::from_secs(5));
        // Un 404 ne dit rien de la cadence.
        rl.constater("mb", 404, None);
        assert_eq!(rl.intervalle_courant("mb"), Duration::from_secs(5));
        // Un succès : retour à la normale d'un coup.
        rl.constater("mb", 200, None);
        assert_eq!(rl.intervalle_courant("mb"), Duration::from_secs(1));
        // Une autre clé n'a jamais bougé.
        assert_eq!(rl.intervalle_courant("autre"), Duration::from_secs(1));
    }

    /// Le refus repousse le PROCHAIN créneau : la requête suivante attend
    /// l'intervalle doublé, pas l'ancien.
    #[tokio::test]
    async fn apres_un_refus_la_requete_suivante_attend_l_intervalle_double() {
        let rl = RateLimiter::with_interval(Duration::from_millis(100));
        rl.acquire("k").await;
        rl.constater("k", 503, None);
        let debut = Instant::now();
        rl.acquire("k").await;
        assert!(
            debut.elapsed() >= Duration::from_millis(190),
            "attendu ~200 ms après un 503, mesuré {:?}",
            debut.elapsed()
        );
        rl.constater("k", 200, None);
        rl.acquire("k").await;
        let debut = Instant::now();
        rl.acquire("k").await;
        let attente = debut.elapsed();
        assert!(
            attente >= Duration::from_millis(90) && attente < Duration::from_millis(180),
            "après un succès, l'intervalle doit revenir à 100 ms : {attente:?}"
        );
    }

    /// `Retry-After` est respecté quand il dépasse l'intervalle doublé.
    #[tokio::test]
    async fn retry_after_est_respecte() {
        let rl = RateLimiter::with_interval(Duration::from_millis(10));
        rl.acquire("k").await;
        rl.constater("k", 503, Some("1"));
        let debut = Instant::now();
        rl.acquire("k").await;
        assert!(
            debut.elapsed() >= Duration::from_millis(950),
            "Retry-After: 1 non respecté : {:?}",
            debut.elapsed()
        );
        // L'intervalle, lui, n'a fait que doubler.
        assert_eq!(rl.intervalle_courant("k"), Duration::from_millis(20));
    }

    #[test]
    fn retry_after_se_lit_en_secondes_et_en_date_http() {
        assert_eq!(lire_retry_after("7"), Some(Duration::from_secs(7)));
        assert_eq!(lire_retry_after(" 0 "), Some(Duration::ZERO));
        assert_eq!(
            lire_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(Duration::ZERO),
            "une date passée vaut zéro"
        );
        let futur = (chrono::Utc::now() + chrono::Duration::seconds(30))
            .format("%a, %d %b %Y %H:%M:%S GMT")
            .to_string();
        let lu = lire_retry_after(&futur).unwrap();
        assert!(
            lu >= Duration::from_secs(28) && lu <= Duration::from_secs(30),
            "{lu:?}"
        );
        assert_eq!(lire_retry_after("bientôt"), None);
    }

    /// Un Retry-After démesuré est borné : il ne gèle pas les passes.
    #[test]
    fn un_retry_after_demesure_est_borne() {
        let rl = RateLimiter::with_interval(Duration::from_millis(10));
        rl.constater("k", 503, Some("86400"));
        let etat = *rl.etats().get("k").unwrap();
        let attente = etat.prochain.saturating_duration_since(Instant::now());
        assert!(attente <= RETRY_AFTER_MAX, "{attente:?}");
        assert!(
            attente >= RETRY_AFTER_MAX - Duration::from_secs(1),
            "{attente:?}"
        );
    }

    #[test]
    fn acoustid_plafonne_a_trois_requetes_par_seconde() {
        assert!(ACOUSTID.intervalle_courant(CLE_ACOUSTID) >= Duration::from_millis(333));
    }

    #[tokio::test]
    async fn rate_limiter_independent_keys_do_not_block_each_other() {
        // Distinct keys each get their own immediate first slot, so even a huge
        // interval must not make them wait on one another.
        let rl = RateLimiter::with_interval(Duration::from_secs(10));
        let start = Instant::now();
        rl.acquire("a").await;
        rl.acquire("b").await;
        rl.acquire("c").await;
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "distinct keys must not wait on each other"
        );
    }
}
