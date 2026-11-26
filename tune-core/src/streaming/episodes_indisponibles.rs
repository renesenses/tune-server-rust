//! Épisodes de podcast indisponibles chez l'éditeur (forum, fil 2972).
//!
//! Un flux RSS peut lister encore un épisode dont le fichier a disparu : le
//! serveur de l'éditeur répond 404 (ou 410). Tune faisait semblant de
//! démarrer, le compteur avançait d'une ou deux secondes puis revenait en
//! arrière, sans son ni message.
//!
//! Ce registre retient, en mémoire, l'adresse audio des épisodes que
//! l'éditeur a refusés AU CLIC (sonde de `POST /podcasts/play`) ou à la
//! première requête du mandataire (sondeur, #9315). Les listes d'épisodes le
//! lisent pour marquer `unavailable: true`. Aucune sonde n'est faite à
//! l'affichage : rien n'est chargé tant qu'on ne clique pas.
//!
//! La marque tombe :
//! - après [`DUREE_DE_LA_MARQUE`] (24 h) ;
//! - quand le flux, relu, date l'épisode APRÈS la marque (l'éditeur l'a
//!   republié) ;
//! - quand un nouveau clic trouve le fichier ([`oublier`]).
//!
//! Pas de base : la marque est un indice d'affichage, pas une donnée. Un
//! redémarrage l'efface, et le prochain clic la repose si le fichier manque
//! toujours.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime};

/// Code stable de l'erreur, que le client traduit (`erreursServeur.<code>`).
pub const CODE_EPISODE_INDISPONIBLE: &str = "episode_unavailable";

/// Durée de vie d'une marque.
pub const DUREE_DE_LA_MARQUE: Duration = Duration::from_secs(24 * 3600);

/// Borne du registre : au-delà, les marques expirées partent, puis tout.
const PLAFOND: usize = 4096;

#[derive(Debug, Clone, Copy)]
struct Marque {
    depuis: SystemTime,
    statut: u16,
}

fn registre() -> &'static Mutex<HashMap<String, Marque>> {
    static REGISTRE: OnceLock<Mutex<HashMap<String, Marque>>> = OnceLock::new();
    REGISTRE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cle(adresse: &str) -> String {
    adresse.trim().to_string()
}

/// Ce statut de l'éditeur veut-il dire « le fichier n'existe plus » ?
pub fn statut_introuvable(statut: u16) -> bool {
    matches!(statut, 404 | 410)
}

/// Marque l'épisode d'adresse `adresse` indisponible (`statut` de l'éditeur).
pub fn marquer(adresse: &str, statut: u16) {
    marquer_a(adresse, statut, SystemTime::now());
}

fn marquer_a(adresse: &str, statut: u16, maintenant: SystemTime) {
    let Ok(mut r) = registre().lock() else {
        return;
    };
    if r.len() >= PLAFOND {
        r.retain(|_, m| !expiree(m, maintenant));
        if r.len() >= PLAFOND {
            r.clear();
        }
    }
    r.insert(
        cle(adresse),
        Marque {
            depuis: maintenant,
            statut,
        },
    );
}

/// Retire la marque (le fichier a été trouvé).
pub fn oublier(adresse: &str) {
    if let Ok(mut r) = registre().lock() {
        r.remove(&cle(adresse));
    }
}

fn expiree(m: &Marque, maintenant: SystemTime) -> bool {
    maintenant
        .duration_since(m.depuis)
        .is_ok_and(|d| d >= DUREE_DE_LA_MARQUE)
}

/// Le statut noté pour cet épisode, si la marque vaut encore.
pub fn statut_note(adresse: &str) -> Option<u16> {
    statut_note_a(adresse, None, SystemTime::now())
}

/// L'épisode, tel que le flux le décrit (`publie` : `pubDate` RSS, date Atom
/// ou Radio France), est-il marqué indisponible ? Une date de publication
/// postérieure à la marque la fait tomber : l'éditeur a republié.
pub fn indisponible(adresse: &str, publie: Option<&str>) -> bool {
    statut_note_a(adresse, publie, SystemTime::now()).is_some()
}

fn statut_note_a(adresse: &str, publie: Option<&str>, maintenant: SystemTime) -> Option<u16> {
    let mut r = registre().lock().ok()?;
    let k = cle(adresse);
    let m = *r.get(&k)?;
    // Une date dans le futur (flux mal daté, horloge de l'éditeur en avance)
    // ne prouve aucune republication : elle laisserait tomber la marque à
    // chaque lecture de la liste.
    let republie = publie
        .and_then(date_de_publication)
        .is_some_and(|d| d > m.depuis && d <= maintenant);
    if expiree(&m, maintenant) || republie {
        r.remove(&k);
        return None;
    }
    Some(m.statut)
}

/// RFC 2822 (`pubDate` RSS) ou RFC 3339 (Atom, Radio France).
fn date_de_publication(brut: &str) -> Option<SystemTime> {
    let brut = brut.trim();
    if brut.is_empty() {
        return None;
    }
    let d = chrono::DateTime::parse_from_rfc2822(brut)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(brut))
        .ok()?;
    let secs = u64::try_from(d.timestamp()).ok()?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Ajoute `unavailable` à chaque épisode d'une liste JSON (`audio_url`, et
/// `published` ou `published_date`). Ce qui n'est pas un objet est laissé.
pub fn annoter(episodes: &mut serde_json::Value) {
    let Some(liste) = episodes.as_array_mut() else {
        return;
    };
    for ep in liste {
        let Some(obj) = ep.as_object_mut() else {
            continue;
        };
        let adresse = obj.get("audio_url").and_then(|v| v.as_str()).unwrap_or("");
        let publie = obj
            .get("published")
            .or_else(|| obj.get("published_date"))
            .and_then(|v| v.as_str());
        let marque = !adresse.is_empty() && indisponible(adresse, publie);
        obj.insert("unavailable".into(), serde_json::Value::Bool(marque));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_marque_tombe_apres_24_h() {
        let url = "https://media.test/indisponible-24h.mp3";
        let t0 = SystemTime::now();
        marquer_a(url, 404, t0);
        assert_eq!(
            statut_note_a(url, None, t0 + Duration::from_secs(3600)),
            Some(404)
        );
        assert_eq!(statut_note_a(url, None, t0 + DUREE_DE_LA_MARQUE), None);
        // Tombée, elle ne revient pas.
        assert_eq!(statut_note_a(url, None, t0), None);
    }

    #[test]
    fn un_episode_republie_apres_la_marque_n_est_plus_indisponible() {
        let url = "https://media.test/republie.mp3";
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_763_000_000);
        let plus_tard = t0 + Duration::from_secs(7200);
        marquer_a(url, 410, t0);
        // Publié avant la marque : toujours indisponible.
        assert_eq!(
            statut_note_a(url, Some("Thu, 01 Jan 2015 10:00:00 +0000"), plus_tard),
            Some(410)
        );
        // Republié une heure après la marque, lu deux heures après : elle tombe.
        let republie = chrono::DateTime::<chrono::Utc>::from(t0 + Duration::from_secs(3600));
        assert_eq!(
            statut_note_a(url, Some(&republie.to_rfc3339()), plus_tard),
            None
        );
        assert_eq!(statut_note_a(url, None, plus_tard), None);
    }

    #[test]
    fn une_date_de_publication_dans_le_futur_ne_leve_pas_la_marque() {
        let url = "https://media.test/date-future.mp3";
        let t0 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_763_000_000);
        marquer_a(url, 404, t0);
        assert_eq!(
            statut_note_a(
                url,
                Some("2099-01-01T10:00:00+01:00"),
                t0 + Duration::from_secs(60)
            ),
            Some(404),
            "une date future ne prouve pas que l'éditeur a republié"
        );
        oublier(url);
    }

    #[test]
    fn oublier_retire_la_marque_et_les_autres_statuts_ne_comptent_pas() {
        let url = "https://media.test/retrouve.mp3";
        marquer(url, 404);
        assert_eq!(statut_note(url), Some(404));
        oublier(url);
        assert_eq!(statut_note(url), None);
        assert!(statut_introuvable(404) && statut_introuvable(410));
        assert!(!statut_introuvable(403) && !statut_introuvable(500));
    }

    #[test]
    fn annoter_marque_seulement_les_episodes_notes() {
        let absent = "https://media.test/annoter-absent.mp3";
        marquer(absent, 404);
        let mut v = serde_json::json!([
            {"audio_url": absent, "published": ""},
            {"audio_url": "https://media.test/annoter-present.mp3"},
        ]);
        annoter(&mut v);
        assert_eq!(v[0]["unavailable"], true);
        assert_eq!(v[1]["unavailable"], false);
    }
}
