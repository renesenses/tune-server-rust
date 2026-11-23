//! Diagnostic rc14 (#8963, #7398, #9174) : ce que le relais a reçu du pont,
//! et quand.
//!
//! Une lecture d'une zone navigateur jouée à distance (« Ce téléphone » par le
//! pont) est abandonnée au bout de 12 s quand personne ne tire le flux
//! (`lecture_sans_destination_abandonnee`). Le journal ne disait pas si le
//! téléphone parlait encore au serveur à ce moment-là. Trois histoires
//! restent possibles, et elles ne laissent pas les mêmes traces :
//!
//! - le téléphone ne dit plus rien par le pont (ni état, ni position) :
//!   aucune requête relayée depuis la lecture ;
//! - le téléphone parle (états, positions) mais ne demande pas le flux :
//!   requêtes relayées récentes, aucune demande de flux ;
//! - le pont demande un flux, mais pas celui de la lecture en cours.
//!
//! Ce module garde l'instant de la dernière requête d'API relayée et de la
//! dernière demande de flux relayée. Rien d'autre : ni chemin, ni appareil.

use std::sync::Mutex;
use std::time::Instant;

static DERNIERE_REQUETE: Mutex<Option<Instant>> = Mutex::new(None);
static DERNIERE_DEMANDE_DE_FLUX: Mutex<Option<(Instant, String)>> = Mutex::new(None);

/// Le pont vient de relayer une requête d'API (`relay.request`).
pub fn noter_requete_relayee() {
    if let Ok(mut d) = DERNIERE_REQUETE.lock() {
        *d = Some(Instant::now());
    }
}

/// Le pont vient de relayer une demande de flux (`relay.stream_request`).
pub fn noter_demande_de_flux_relayee(stream_id: &str) {
    if let Ok(mut d) = DERNIERE_DEMANDE_DE_FLUX.lock() {
        *d = Some((Instant::now(), identifiant_court(stream_id)));
    }
}

/// Millisecondes depuis la dernière requête d'API relayée, `None` si aucune
/// depuis le démarrage.
pub fn depuis_la_derniere_requete_relayee_ms() -> Option<u64> {
    DERNIERE_REQUETE
        .lock()
        .ok()
        .and_then(|d| *d)
        .map(|t| t.elapsed().as_millis() as u64)
}

/// Millisecondes depuis la dernière demande de flux relayée, avec
/// l'identifiant court du flux demandé.
pub fn derniere_demande_de_flux_relayee() -> Option<(u64, String)> {
    DERNIERE_DEMANDE_DE_FLUX
        .lock()
        .ok()
        .and_then(|d| d.clone())
        .map(|(t, id)| (t.elapsed().as_millis() as u64, id))
}

/// Les 8 premiers caractères d'un identifiant de flux, extension retirée :
/// assez pour rapprocher deux lignes du même flux, sans recopier
/// l'identifiant entier.
pub fn identifiant_court(stream_id: &str) -> String {
    let sans_extension = stream_id.split('.').next().unwrap_or(stream_id);
    sans_extension.chars().take(8).collect()
}

/// Diagnostic rc14 (#8963, suite) : les adresses de flux que le pont a
/// remises au téléphone, dans les réponses relayées sous `/api/v1/zones`
/// (réponse de `play`, relecture de la zone). Le téléphone ne reçoit AUCUN
/// évènement par le pont (`relay.client_message` n'est pas servi) : il ne
/// connaît l'adresse du flux que par ces réponses. Si l'adresse du flux
/// abandonné n'y est jamais passée, le téléphone ne pouvait pas le tirer.
static ADRESSES_REMISES: Mutex<std::collections::VecDeque<(Instant, String)>> =
    Mutex::new(std::collections::VecDeque::new());
/// Combien d'adresses on garde : quelques lectures, pas une histoire.
const ADRESSES_GARDEES: usize = 32;
/// La dernière zone que le pont a relue (`GET /api/v1/zones/{id}…`).
static DERNIERE_ZONE_RELUE: Mutex<Option<(Instant, i64)>> = Mutex::new(None);
/// WebSockets ouverts à travers le pont (`relay.client_connected` moins
/// `relay.client_disconnected`).
static CLIENTS_WS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

/// Les identifiants de flux cités dans un corps de réponse : le segment qui
/// suit `/stream/` (ou `/stream/relay/<serveur>/`), sans extension. Seuls
/// les segments de 8 caractères ou plus comptent : un identifiant de flux est
/// un UUID.
pub fn flux_cites(corps: &[u8]) -> Vec<String> {
    let texte = String::from_utf8_lossy(corps);
    let mut flux = Vec::new();
    for (i, _) in texte.match_indices("/stream/") {
        let mut reste = &texte[i + "/stream/".len()..];
        if let Some(apres) = reste.strip_prefix("relay/") {
            match apres.find('/') {
                Some(j) => reste = &apres[j + 1..],
                None => continue,
            }
        }
        let id: String = reste
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .collect();
        if id.len() >= 8 && !flux.contains(&id) {
            flux.push(id);
        }
    }
    flux
}

/// La zone visée par un chemin `/api/v1/zones/{id}…`, requête retirée.
pub fn zone_du_chemin(chemin: &str) -> Option<i64> {
    let sans_requete = chemin.split('?').next().unwrap_or(chemin);
    sans_requete
        .strip_prefix("/api/v1/zones/")?
        .split('/')
        .next()?
        .parse()
        .ok()
}

/// Le pont vient de recevoir la réponse d'une requête relayée. Seules les
/// réponses sous `/api/v1/zones` sont lues : ce sont elles qui portent
/// `stream_url` / `stream_url_remote` d'une zone navigateur.
pub fn noter_reponse_relayee(methode: &str, chemin: &str, corps: &[u8]) {
    let sans_requete = chemin.split('?').next().unwrap_or(chemin);
    if sans_requete != "/api/v1/zones" && !sans_requete.starts_with("/api/v1/zones/") {
        return;
    }
    let maintenant = Instant::now();
    if methode.eq_ignore_ascii_case("GET")
        && let Some(zone) = zone_du_chemin(chemin)
        && let Ok(mut d) = DERNIERE_ZONE_RELUE.lock()
    {
        *d = Some((maintenant, zone));
    }
    let flux = flux_cites(corps);
    if flux.is_empty() {
        return;
    }
    if let Ok(mut a) = ADRESSES_REMISES.lock() {
        for id in flux {
            a.retain(|(_, f)| *f != id);
            a.push_back((maintenant, id));
        }
        while a.len() > ADRESSES_GARDEES {
            a.pop_front();
        }
    }
}

/// Millisecondes depuis que l'adresse de [stream_id] a été remise au pont
/// pour la dernière fois, `None` si elle ne l'a jamais été.
pub fn adresse_remise_au_pont_ms(stream_id: &str) -> Option<u64> {
    let id = stream_id.split('.').next().unwrap_or(stream_id);
    ADRESSES_REMISES.lock().ok().and_then(|a| {
        a.iter()
            .rev()
            .find(|(_, f)| f == id)
            .map(|(t, _)| t.elapsed().as_millis() as u64)
    })
}

/// Millisecondes depuis la dernière relecture d'une zone par le pont, et
/// laquelle.
pub fn derniere_zone_relue_par_le_pont() -> Option<(u64, i64)> {
    DERNIERE_ZONE_RELUE
        .lock()
        .ok()
        .and_then(|d| *d)
        .map(|(t, z)| (t.elapsed().as_millis() as u64, z))
}

/// Un client a ouvert (`true`) ou fermé (`false`) un WebSocket à travers le
/// pont. Rend le nombre de WebSockets ouverts.
pub fn noter_client_ws(connecte: bool) -> i64 {
    use std::sync::atomic::Ordering;
    if connecte {
        CLIENTS_WS.fetch_add(1, Ordering::Relaxed) + 1
    } else {
        let avant = CLIENTS_WS
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                Some((n - 1).max(0))
            })
            .unwrap_or(0);
        (avant - 1).max(0)
    }
}

/// WebSockets ouverts à travers le pont.
pub fn clients_ws_par_le_pont() -> i64 {
    CLIENTS_WS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Ce que l'abandon d'une lecture de zone navigateur dit du téléphone, en
/// un mot : la ligne `lecture_sans_destination_abandonnee` le porte
/// (`silence`). Toutes les durées sont « il y a N ms » ; `lecture_ms` est
/// l'âge de la lecture abandonnée.
///
/// - `pont_muet` : le pont n'a rien relayé depuis la lecture — le téléphone
///   ne parlait plus au serveur (réseau, application en arrière-plan) ;
/// - `autre_flux_demande` : le pont a demandé un flux depuis la lecture,
///   mais pas celui-ci ;
/// - `adresse_remise_flux_non_demande` : l'adresse de CE flux est partie
///   vers le pont, et le téléphone ne l'a pas tirée — défaut côté
///   application (lecteur, zone suivie) ;
/// - `adresse_jamais_remise` : le téléphone parlait, mais aucune réponse
///   relayée n'a porté l'adresse de ce flux — il n'a pas relu cette zone
///   après la lecture, ou l'a relue avant que le flux existe.
pub fn silence_du_telephone(
    lecture_ms: u64,
    depuis_requete_relayee_ms: Option<u64>,
    derniere_demande_de_flux: Option<(u64, &str)>,
    adresse_remise_ms: Option<u64>,
) -> &'static str {
    let depuis_la_lecture = |ms: Option<u64>| ms.is_some_and(|ms| ms <= lecture_ms);
    if !depuis_la_lecture(depuis_requete_relayee_ms)
        && !depuis_la_lecture(derniere_demande_de_flux.map(|(ms, _)| ms))
    {
        return "pont_muet";
    }
    if depuis_la_lecture(adresse_remise_ms) {
        return "adresse_remise_flux_non_demande";
    }
    if depuis_la_lecture(derniere_demande_de_flux.map(|(ms, _)| ms)) {
        return "autre_flux_demande";
    }
    "adresse_jamais_remise"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_identifiant_court_garde_huit_caracteres_sans_extension() {
        assert_eq!(identifiant_court("038e9337abcdef.flac"), "038e9337");
        assert_eq!(identifiant_court("abc"), "abc");
        assert_eq!(identifiant_court(""), "");
    }

    #[test]
    fn les_instants_notes_se_relisent() {
        noter_requete_relayee();
        noter_demande_de_flux_relayee("0123456789.wav");
        assert!(depuis_la_derniere_requete_relayee_ms().is_some_and(|ms| ms < 60_000));
        let (ms, id) = derniere_demande_de_flux_relayee().expect("demande notée");
        assert!(ms < 60_000);
        assert_eq!(id, "01234567");
    }

    #[test]
    fn les_flux_cites_se_lisent_dans_les_deux_adresses_8963() {
        let corps = br#"{"id":43,"stream_url":"http://192.168.1.5:8888/stream/038e9337-aaaa-bbbb-cccc-0123456789ab.flac","stream_url_remote":"https://bridge.mozaiklabs.fr/stream/relay/srv-1/038e9337-aaaa-bbbb-cccc-0123456789ab.flac","cover":"/stream/x"}"#;
        assert_eq!(
            flux_cites(corps),
            vec!["038e9337-aaaa-bbbb-cccc-0123456789ab".to_string()]
        );
        assert!(flux_cites(b"{}").is_empty());
    }

    #[test]
    fn la_zone_se_lit_dans_le_chemin_8963() {
        assert_eq!(zone_du_chemin("/api/v1/zones/43"), Some(43));
        assert_eq!(zone_du_chemin("/api/v1/zones/42/status?x=1"), Some(42));
        assert_eq!(zone_du_chemin("/api/v1/zones"), None);
        assert_eq!(zone_du_chemin("/api/v1/library/albums"), None);
    }

    #[test]
    fn une_adresse_remise_au_pont_se_retrouve_8963() {
        let id = "c0ffee00-1111-2222-3333-444455556666";
        assert_eq!(adresse_remise_au_pont_ms(id), None);
        let corps = format!(r#"{{"stream_url_remote":"https://b/stream/relay/s/{id}.flac"}}"#);
        // Hors de `/api/v1/zones` : rien n'est noté.
        noter_reponse_relayee("GET", "/api/v1/library/albums", corps.as_bytes());
        assert_eq!(adresse_remise_au_pont_ms(id), None);
        noter_reponse_relayee("GET", "/api/v1/zones/77", corps.as_bytes());
        assert!(adresse_remise_au_pont_ms(&format!("{id}.flac")).is_some_and(|ms| ms < 60_000));
    }

    #[test]
    fn le_silence_du_telephone_se_classe_8963() {
        // Rien relayé depuis la lecture (dernière requête il y a 60 s, lecture il y a 12 s).
        assert_eq!(
            silence_du_telephone(12_000, Some(60_000), None, None),
            "pont_muet"
        );
        assert_eq!(silence_du_telephone(12_000, None, None, None), "pont_muet");
        // Le téléphone parle, l'adresse est partie, rien n'est tiré.
        assert_eq!(
            silence_du_telephone(12_000, Some(2_000), None, Some(10_500)),
            "adresse_remise_flux_non_demande"
        );
        // Le téléphone parle, l'adresse n'est jamais partie depuis la lecture.
        assert_eq!(
            silence_du_telephone(12_000, Some(2_000), None, Some(90_000)),
            "adresse_jamais_remise"
        );
        // Le pont a tiré un autre flux depuis la lecture.
        assert_eq!(
            silence_du_telephone(12_000, Some(2_000), Some((5_000, "deadbeef")), None),
            "autre_flux_demande"
        );
    }
}
