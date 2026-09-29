//! Tune Circle, étape T5 (#5328) : la RÉFÉRENCE d'un morceau.
//!
//! Une playlist de cercle ne contient que des références — ce qui désigne un
//! morceau hors de ce serveur : titre, artiste, album, durée, ISRC, et les
//! identifiants des services. Jamais un fichier, un flux, un chemin ou un
//! identifiant de ligne local. Chacun la rejoue avec ses propres services
//! ([`crate::resolution`]).
//!
//! Le format est celui du Playlist Hub
//! ([`tune_core::cloud::playlist_hub::reference_de_piste`]), réemployé tel
//! quel, puis mis à la forme que le cloud du cercle accepte
//! (site-mozaiklabs#236, « Référence ») : ses clés seules
//! ([`CLES_DE_REFERENCE`]), et chaque valeur à son format. Une valeur que le
//! cloud refuserait (un ISRC mal étiqueté dans un fichier, un MBID qui n'est
//! pas un UUID) est ÉCARTÉE plutôt qu'envoyée : le cloud refuse la requête
//! entière pour une seule clé fautive, et l'ajout ne doit pas échouer pour
//! un tag abîmé.

use std::sync::Arc;

use serde_json::{Map, Value};
use tune_core::cloud::playlist_hub::{SERVICES_DE_REFERENCE, reference_de_piste};
use tune_core::db::backend::DbBackend;
use tune_core::streaming::traits::StreamTrack;

/// Les clés qu'une référence peut porter vers le cloud du cercle
/// (site-mozaiklabs#236, services étendus par la décision 4 du 28/09) :
/// aucune autre.
pub const CLES_DE_REFERENCE: [&str; 11] = [
    "title",
    "artist_name",
    "album_title",
    "duration_ms",
    "isrc",
    "musicbrainz_recording_id",
    "qobuz_id",
    "tidal_id",
    "spotify_id",
    "deezer_id",
    "youtube_id",
];

/// La clé de l'identifiant d'un service dans une référence (`qobuz_id`…).
pub fn cle_du_service(service: &str) -> String {
    format!("{service}_id")
}

/// Longueur maximale d'un titre, d'un artiste ou d'un album (contrat cloud).
pub const LONGUEUR_MAX_TEXTE: usize = 300;
/// Durée maximale d'un morceau, en millisecondes (contrat cloud : 24 h).
pub const DUREE_MAX_MS: u64 = 86_400_000;

fn tronquer(s: &str) -> String {
    s.trim().chars().take(LONGUEUR_MAX_TEXTE).collect()
}

/// `^[A-Z]{2}[A-Z0-9]{3}[0-9]{7}$` après normalisation, sinon `None`.
fn isrc_valide(brut: &str) -> Option<String> {
    let n = isrc_normalise(brut);
    let b = n.as_bytes();
    let ok = b.len() == 12
        && b[..2].iter().all(u8::is_ascii_uppercase)
        && b[2..5]
            .iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        && b[5..].iter().all(u8::is_ascii_digit);
    ok.then_some(n)
}

/// Un UUID, en minuscules, sinon `None`.
fn uuid_valide(brut: &str) -> Option<String> {
    let n = brut.trim().to_ascii_lowercase();
    let groupes: Vec<&str> = n.split('-').collect();
    let ok = groupes.iter().map(|g| g.len()).eq([8, 4, 4, 4, 12])
        && groupes
            .iter()
            .all(|g| g.chars().all(|c| c.is_ascii_hexdigit()));
    ok.then_some(n)
}

/// `^[A-Za-z0-9._:-]{1,64}$`, sinon `None`.
fn identifiant_de_service_valide(brut: &str) -> Option<String> {
    let n = brut.trim();
    let ok = (1..=64).contains(&n.len())
        && n.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'));
    ok.then(|| n.to_string())
}

fn chaine(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Met une référence à la forme du cloud : ses clés seules
/// ([`CLES_DE_REFERENCE`]), chaque valeur à son format ; une valeur hors
/// format est écartée. `None` sans titre.
fn restreindre(reference: &Value) -> Option<Value> {
    let objet = reference.as_object()?;
    let titre = objet.get("title").and_then(chaine).map(|t| tronquer(&t))?;
    if titre.is_empty() {
        return None;
    }
    let mut sortie = Map::new();
    sortie.insert("title".into(), Value::String(titre));
    for cle in CLES_DE_REFERENCE.iter().skip(1) {
        let Some(v) = objet.get(*cle).filter(|v| !v.is_null()) else {
            continue;
        };
        let rendu = match *cle {
            "artist_name" | "album_title" => chaine(v)
                .map(|t| tronquer(&t))
                .filter(|t| !t.is_empty())
                .map(Value::String),
            "duration_ms" => v
                .as_u64()
                .or_else(|| v.as_i64().map(|d| d.max(0) as u64))
                .map(|d| Value::from(d.min(DUREE_MAX_MS))),
            "isrc" => chaine(v).and_then(|i| isrc_valide(&i)).map(Value::String),
            "musicbrainz_recording_id" => {
                chaine(v).and_then(|m| uuid_valide(&m)).map(Value::String)
            }
            _ => chaine(v)
                .and_then(|i| identifiant_de_service_valide(&i))
                .map(Value::String),
        };
        if let Some(r) = rendu {
            sortie.insert(cle.to_string(), r);
        }
    }
    Some(Value::Object(sortie))
}

/// La référence d'une piste de la base (locale, ou titre de service rangé en
/// bibliothèque), `Ok(None)` si l'identifiant n'a pas de ligne.
///
/// Construite par le Playlist Hub, puis restreinte : ni `file_path`, ni le
/// `source_id` d'une piste locale ne sont même lus.
pub fn depuis_la_bibliotheque(
    backend: &Arc<dyn DbBackend>,
    track_id: i64,
) -> Result<Option<Value>, String> {
    Ok(reference_de_piste(backend, track_id)?.and_then(|r| restreindre(&r)))
}

/// La référence d'un titre de SERVICE, tel que le service le décrit :
/// l'identifiant sous la clé de son service, l'ISRC s'il est connu.
///
/// Un service hors de [`SERVICES_DE_REFERENCE`] n'a pas de clé dans la
/// référence : `None`.
pub fn depuis_le_service(service: &str, piste: &StreamTrack) -> Option<Value> {
    if !SERVICES_DE_REFERENCE.contains(&service) {
        return None;
    }
    let mut r = serde_json::json!({
        "title": piste.title,
        "artist_name": piste.artist,
        "album_title": piste.album,
        "duration_ms": (piste.duration_ms > 0).then_some(piste.duration_ms),
        "isrc": piste.isrc,
    });
    r[cle_du_service(service)] = Value::String(piste.id.clone());
    restreindre(&r)
}

/// Ce qu'une référence lue du cloud dit d'elle-même, pour la résolution.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reference {
    pub title: String,
    pub artist_name: String,
    pub album_title: String,
    pub duration_ms: u64,
    pub isrc: String,
    /// `(service, identifiant)`, dans l'ordre de [`SERVICES_DE_REFERENCE`].
    pub identifiants: Vec<(String, String)>,
}

fn texte(v: &Value, cle: &str) -> String {
    match v.get(cle) {
        Some(Value::String(s)) => s.trim().to_string(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}

impl Reference {
    pub fn lire(v: &Value) -> Self {
        Self {
            title: texte(v, "title"),
            artist_name: texte(v, "artist_name"),
            album_title: texte(v, "album_title"),
            duration_ms: v
                .get("duration_ms")
                .and_then(|d| d.as_u64().or_else(|| d.as_f64().map(|f| f.max(0.0) as u64)))
                .unwrap_or(0),
            isrc: texte(v, "isrc"),
            identifiants: SERVICES_DE_REFERENCE
                .iter()
                .filter_map(|s| {
                    let id = texte(v, &cle_du_service(s));
                    (!id.is_empty()).then(|| (s.to_string(), id))
                })
                .collect(),
        }
    }
}

/// Un ISRC sous sa forme comparable : sans tirets ni espaces, en capitales.
pub fn isrc_normalise(isrc: &str) -> String {
    isrc.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restreindre_ne_garde_que_la_liste_blanche() {
        let r = restreindre(&serde_json::json!({
            "title": "So What", "artist_name": "Miles Davis",
            "musicbrainz_recording_id": "abc", "file_path": "/Users/x/a.flac",
            "source_id": "/Users/x/a.flac", "qobuz_id": "123", "tidal_id": "",
            "isrc": null,
        }));
        assert_eq!(
            r,
            Some(
                serde_json::json!({ "title": "So What", "artist_name": "Miles Davis", "qobuz_id": "123" })
            )
        );
    }

    /// Une seule valeur hors format et le cloud refuserait TOUT l'ajout :
    /// elle est écartée, le reste part, normalisé.
    #[test]
    fn une_valeur_hors_format_est_ecartee_et_le_reste_normalise() {
        let r = restreindre(&serde_json::json!({
            "title": "  X  ", "isrc": "us-sm1-59-00113", "deezer_id": 42,
            "musicbrainz_recording_id": "8E8A594F-2175-3B2C-A7A9-3A5B7A8B2B8F",
            "youtube_id": "https://youtu.be/abc", "duration_ms": -5,
        }))
        .unwrap();
        assert_eq!(
            r,
            serde_json::json!({
                "title": "X", "duration_ms": 0, "isrc": "USSM15900113",
                "musicbrainz_recording_id": "8e8a594f-2175-3b2c-a7a9-3a5b7a8b2b8f",
                "deezer_id": "42",
            })
        );
        assert!(restreindre(&serde_json::json!({ "title": "   " })).is_none());
        assert_eq!(isrc_valide("FRUM7160012"), None, "11 caractères");
    }

    #[test]
    fn l_isrc_se_compare_sans_tirets_ni_casse() {
        assert_eq!(isrc_normalise("us-sm1-59-00123"), "USSM15900123");
    }

    #[test]
    fn une_reference_lue_porte_ses_identifiants_dans_l_ordre_des_services() {
        let r = Reference::lire(&serde_json::json!({
            "title": " So What ", "tidal_id": 55, "qobuz_id": "q1", "duration_ms": 562000
        }));
        assert_eq!(r.title, "So What");
        assert_eq!(r.duration_ms, 562_000);
        assert_eq!(
            r.identifiants,
            vec![("qobuz".into(), "q1".into()), ("tidal".into(), "55".into())]
        );
    }
}
