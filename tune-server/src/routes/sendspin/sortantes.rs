//! Connexions Sendspin INITIÉES PAR LE SERVEUR (#3326).
//!
//! `connection.md` : « Servers MUST support both methods », et le mode où le
//! serveur compose est RECOMMANDÉ — c'est celui des enceintes qui
//! s'annoncent en `_sendspin._tcp` (Home Assistant Voice PE, ESPHome) et
//! attendent qu'un serveur vienne. Tune les découvre par mDNS (le scanner
//! existant) et compose `ws://<hôte>:<port><path>` (« Connections to an
//! address advertised via mDNS MUST use plain ws:// »). Ensuite, la session
//! est exactement celle d'une connexion entrante : l'enceinte envoie
//! `client/init`, Tune répond en serveur et reste l'initiateur Noise ;
//! appairage, rôle `player@v1` et zone suivent les mêmes règles (seule une
//! enceinte APPAIRÉE devient une zone — décision de Bertrand du 09/10/2026).
//!
//! Reconnexion (`client/goodbye`) :
//! - `restart`, ou une coupure sans `client/goodbye` : Tune recompose
//!   (« servers SHOULD assume the disconnect reason is restart and attempt to
//!   auto-reconnect »), avec un délai croissant ;
//! - `concurrent_attempt` : l'enceinte a gardé un autre serveur ; Tune
//!   réessaie plus tard (« Server MAY retry later ») ;
//! - `another_server`, `shutdown`, `user_request`, `unauthorized`,
//!   `pairing_required`, `unpaired` : pas de recomposition automatique ; elle
//!   ne reprend qu'après la `PAUSE_APRES_REFUS`, ou quand l'annonce mDNS
//!   disparaît puis revient.
use super::ContexteSendspin;
use super::prise::Prise;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tune_core::discovery::device::{DiscoveredDevice, OutputType};
use tune_core::discovery::mdns::MdnsScanner;
use tune_core::sendspin::ModeTransition;

/// Cadence de relecture des annonces mDNS.
pub const CADENCE_DECOUVERTE: Duration = Duration::from_secs(2);
/// Délai de reconnexion après une coupure, doublé à chaque échec.
pub const PREMIER_DELAI: Duration = Duration::from_secs(2);
pub const DELAI_MAXIMAL: Duration = Duration::from_secs(60);
/// Après `concurrent_attempt` : un autre serveur tient l'enceinte.
pub const PAUSE_APRES_CONCURRENCE: Duration = Duration::from_secs(60);
/// Après un départ que la spécification interdit de forcer.
pub const PAUSE_APRES_REFUS: Duration = Duration::from_secs(600);
/// Délai d'ouverture de la prise TCP + WebSocket.
pub const DELAI_CONNEXION: Duration = Duration::from_secs(10);

tokio::task_local! {
    static AU_REVOIR: Arc<Mutex<Option<String>>>;
}

/// Le pilote note la raison du `client/goodbye` reçu ; sans effet hors d'une
/// prise sortante.
pub(super) fn noter_au_revoir(raison: String) {
    let _ = AU_REVOIR.try_with(|r| {
        *r.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(raison);
    });
}

/// Comment s'est terminée une session sortante.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Fin {
    /// Prise impossible à ouvrir (hôte injoignable, refus HTTP…).
    Injoignable,
    /// Session terminée sans `client/goodbye` (coupure, erreur de protocole).
    Coupure,
    /// `client/goodbye` avec sa raison.
    AuRevoir(String),
}

/// Quand recomposer après `fin`, `echecs` étant le nombre de fins
/// consécutives sans session utile. `None` : pas avant la pause de refus.
#[must_use]
pub fn delai_avant_recomposition(fin: &Fin, echecs: u32) -> Duration {
    let croissant = || {
        PREMIER_DELAI
            .saturating_mul(1u32 << echecs.min(5))
            .min(DELAI_MAXIMAL)
    };
    match fin {
        Fin::Injoignable | Fin::Coupure => croissant(),
        Fin::AuRevoir(r) => match r.as_str() {
            "restart" => PREMIER_DELAI,
            "concurrent_attempt" => PAUSE_APRES_CONCURRENCE,
            "another_server" | "shutdown" | "user_request" | "unauthorized"
            | "pairing_required" | "unpaired" => PAUSE_APRES_REFUS,
            // Raison inconnue (version future) : prudence, comme une coupure.
            _ => croissant(),
        },
    }
}

/// L'URL d'une enceinte annoncée en `_sendspin._tcp`, si l'annonce porte son
/// `path` (TXT REQUIRED).
#[must_use]
pub fn url_d_annonce(d: &DiscoveredDevice) -> Option<String> {
    if d.device_type != OutputType::Sendspin {
        return None;
    }
    let chemin = d
        .capabilities
        .get(tune_core::discovery::sendspin::CLE_CHEMIN)
        .and_then(|v| v.as_str())?;
    Some(tune_core::discovery::sendspin::url_websocket(
        &d.host, d.port, chemin,
    ))
}

/// Ouvre la prise vers `url` et mène la session côté serveur.
pub async fn composer(url: &str, mode: ModeTransition, contexte: ContexteSendspin) -> Fin {
    let ouverture =
        tokio::time::timeout(DELAI_CONNEXION, tokio_tungstenite::connect_async(url)).await;
    let ws = match ouverture {
        Ok(Ok((ws, _))) => ws,
        Ok(Err(e)) => {
            tracing::debug!(url, error = %e, "sendspin_sortante_injoignable");
            return Fin::Injoignable;
        }
        Err(_) => {
            tracing::debug!(url, "sendspin_sortante_delai_depasse");
            return Fin::Injoignable;
        }
    };
    tracing::info!(url, "sendspin_sortante_ouverte");
    let raison = Arc::new(Mutex::new(None));
    let resultat = AU_REVOIR
        .scope(
            raison.clone(),
            super::conduire(Prise::Sortante(Box::new(ws)), mode, contexte),
        )
        .await;
    let raison = raison
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    tracing::info!(url, raison = ?raison, erreur = ?resultat.as_ref().err(), "sendspin_sortante_fermee");
    match raison {
        Some(r) => Fin::AuRevoir(r),
        None => Fin::Coupure,
    }
}

struct Suivi {
    /// Une session (ou une tentative) est en cours.
    tache: Option<tokio::task::JoinHandle<Fin>>,
    /// Pas de nouvelle tentative avant.
    pas_avant: Instant,
    echecs: u32,
}

/// La boucle de composition : relit les annonces, compose vers chaque
/// enceinte qui n'a pas de session, applique les délais ci-dessus.
pub async fn boucle(
    scanner: std::sync::Weak<std::sync::Mutex<Option<Arc<MdnsScanner>>>>,
    contexte: ContexteSendspin,
    mode: ModeTransition,
) {
    let mut suivis: HashMap<String, Suivi> = HashMap::new();
    loop {
        // L'état du serveur a disparu (témoins) : la boucle s'arrête avec lui.
        let Some(etat) = scanner.upgrade() else {
            return;
        };
        let actuel = etat
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        drop(etat);
        let annonces: Vec<String> = match actuel {
            Some(s) => s.devices().await.iter().filter_map(url_d_annonce).collect(),
            None => Vec::new(),
        };
        // Une annonce disparue efface son historique : son retour la recompose.
        suivis.retain(|url, suivi| annonces.contains(url) || suivi.tache.is_some());
        let maintenant = Instant::now();
        let presentes = annonces.clone();
        for url in annonces {
            let suivi = suivis.entry(url.clone()).or_insert(Suivi {
                tache: None,
                pas_avant: maintenant,
                echecs: 0,
            });
            if let Some(t) = &suivi.tache {
                if !t.is_finished() {
                    continue;
                }
                let fin = suivi
                    .tache
                    .take()
                    .expect("tache presente")
                    .await
                    .unwrap_or(Fin::Coupure);
                let delai = delai_avant_recomposition(&fin, suivi.echecs);
                suivi.echecs = match fin {
                    Fin::AuRevoir(ref r) if r == "restart" => 0,
                    _ => suivi.echecs.saturating_add(1),
                };
                suivi.pas_avant = Instant::now() + delai;
                tracing::info!(url, fin = ?fin, delai_s = delai.as_secs(), "sendspin_sortante_recomposition_planifiee");
                continue;
            }
            if maintenant < suivi.pas_avant {
                continue;
            }
            let contexte = contexte.clone();
            let cible = url.clone();
            suivi.tache = Some(tokio::spawn(async move {
                composer(&cible, mode, contexte).await
            }));
        }
        // Une session dont l'annonce a disparu va jusqu'à sa fin ; son suivi
        // part ensuite.
        suivis.retain(|url, suivi| {
            presentes.contains(url) || suivi.tache.as_ref().is_some_and(|t| !t.is_finished())
        });
        tokio::time::sleep(CADENCE_DECOUVERTE).await;
    }
}

/// Lance la boucle si un runtime tokio tourne (le routeur se construit aussi
/// hors runtime dans certains témoins).
pub fn lancer(
    scanner: Arc<std::sync::Mutex<Option<Arc<MdnsScanner>>>>,
    contexte: ContexteSendspin,
    mode: ModeTransition,
) {
    // UNE boucle par état de serveur : deux routeurs construits sur le même
    // état (rechargement de greffons) composeraient deux fois vers la même
    // enceinte, et la seconde prise délogerait la première (même priorité,
    // « higher or equal is accepted »).
    type Scanner = std::sync::Mutex<Option<Arc<MdnsScanner>>>;
    static LANCEES: std::sync::OnceLock<Mutex<Vec<std::sync::Weak<Scanner>>>> =
        std::sync::OnceLock::new();
    let Ok(rt) = tokio::runtime::Handle::try_current() else {
        return;
    };
    {
        let mut lancees = LANCEES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        lancees.retain(|w| w.strong_count() > 0);
        if lancees.iter().any(|w| w.as_ptr() == Arc::as_ptr(&scanner)) {
            return;
        }
        lancees.push(Arc::downgrade(&scanner));
    }
    rt.spawn(boucle(Arc::downgrade(&scanner), contexte, mode));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i3326_sortante_delais_selon_la_raison_du_depart() {
        assert_eq!(
            delai_avant_recomposition(&Fin::AuRevoir("restart".into()), 4),
            PREMIER_DELAI
        );
        assert_eq!(
            delai_avant_recomposition(&Fin::AuRevoir("concurrent_attempt".into()), 0),
            PAUSE_APRES_CONCURRENCE
        );
        for r in [
            "another_server",
            "shutdown",
            "user_request",
            "unauthorized",
            "pairing_required",
            "unpaired",
        ] {
            assert_eq!(
                delai_avant_recomposition(&Fin::AuRevoir(r.into()), 0),
                PAUSE_APRES_REFUS,
                "{r}"
            );
        }
        assert_eq!(delai_avant_recomposition(&Fin::Coupure, 0), PREMIER_DELAI);
        assert_eq!(
            delai_avant_recomposition(&Fin::Coupure, 2),
            PREMIER_DELAI * 4
        );
        assert_eq!(
            delai_avant_recomposition(&Fin::Injoignable, 30),
            DELAI_MAXIMAL
        );
    }

    #[test]
    fn i3326_sortante_url_depuis_l_annonce_mdns() {
        let mut d = tune_core::discovery::sendspin::appareil_annonce(
            "192.168.1.20",
            8928,
            None,
            Some("Voice PE"),
        );
        assert_eq!(
            url_d_annonce(&d),
            None,
            "TXT path REQUIRED : sans lui, pas de prise"
        );
        d = tune_core::discovery::sendspin::appareil_annonce(
            "192.168.1.20",
            8928,
            Some("/sendspin"),
            Some("Voice PE"),
        );
        assert_eq!(
            url_d_annonce(&d).as_deref(),
            Some("ws://192.168.1.20:8928/sendspin")
        );
        d.device_type = OutputType::Chromecast;
        assert_eq!(url_d_annonce(&d), None);
    }
}
