//! #5160 — les crédits écrits dans les BALISES du fichier (`PERFORMER`,
//! `PRODUCER`, `COMPOSER`, `CONDUCTOR`, `LYRICIST`, `REMIXER`), rendus dans le
//! vocabulaire de `track_credits`.
//!
//! Le tiroir « Crédits » (fiche album et fiche titre) lit
//! `GET /library/albums/{id}/credits` et `GET /library/tracks/{id}/credits`,
//! qui ne lisaient QUE `track_credits`. Or cette table n'est remplie que par
//! les passes MusicBrainz (`credits_release`) et l'import du pont Roon. Les
//! balises du fichier, elles, sont rangées au scan dans le magasin plat
//! `track_metadata` (`read_extended_metadata`, rattrapées par #5043/#5048),
//! sous les clés `performer`, `producer`… — et aucun chemin ne les portait
//! jusqu'au tiroir. Un fichier étiqueté `Performer = Christian McBride (bass);
//! Nasheet Waits (drums)` montrait ses interprètes dans « Tous les champs
//! piste » et nulle part dans « Crédits » (Reivax66, fil forum 1965).
//!
//! Ce module les relit à la DEMANDE, sans rien écrire : `track_credits` reste
//! la chose de MusicBrainz et de Roon (sa purge par piste, `only_missing`, la
//! page artiste), et un crédit de balise corrigé dans le fichier se voit au
//! scan suivant sans qu'aucune ligne périmée ne reste en base.
//!
//! ## Formes lues
//!
//! - une valeur, plusieurs noms séparés par `;` (la forme de la capture) ;
//! - plusieurs balises du même nom : `read_extended_metadata` les joint par
//!   `; ` (la forme de Picard, `PERFORMER=…` répété) ;
//! - pour `performer` seulement, l'instrument entre parenthèses à la FIN du
//!   nom : `Christian McBride (bass)` → (`Christian McBride`, `bass`).
//!
//! ## Arbitrage avec MusicBrainz
//!
//! Une personne que `track_credits` crédite déjà au même rôle sur la même
//! piste n'est PAS répétée depuis les balises : la ligne MusicBrainz porte
//! davantage (fiche d'artiste, MBID, instrument canonique). Le parolier des
//! balises (`lyricist`) et le `writer` de MusicBrainz sont le même rôle pour
//! ce dédoublonnage.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::db::artist_repo::ArtistRepo;
use crate::db::backend::DbBackend;
use crate::db::track_metadata_repo::TrackMetadataRepo;

/// Clé de `track_metadata` → rôle de `track_credits`, dans l'ordre où les
/// lignes sortent pour une piste (l'écriture, puis l'interprétation, puis la
/// production — l'ordre du tiroir).
pub const CLES_DE_CREDIT: [(&str, &str); 6] = [
    ("composer", "composer"),
    ("lyricist", "lyricist"),
    ("conductor", "conductor"),
    ("performer", "performer"),
    ("producer", "producer"),
    ("remixer", "remixer"),
];

/// Une ligne de crédit tirée des balises d'une piste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreditDeBalise {
    pub track_id: i64,
    /// La fiche d'artiste de la bibliothèque qui porte ce nom, s'il y en a
    /// une — jamais créée, comme pour MusicBrainz (`ecrire_credits_piste`).
    pub artist_id: Option<i64>,
    pub artist_name: String,
    pub role: &'static str,
    pub instrument: Option<String>,
}

/// Le rôle tel que le dédoublonnage le compare : `lyricist` (balises) et
/// `writer` (MusicBrainz, `role_canonique`) désignent le même crédit.
fn role_de_comparaison(role: &str) -> &str {
    match role {
        "lyricist" => "writer",
        autre => autre,
    }
}

/// La clé sous laquelle un crédit DÉJÀ présent dans `track_credits` écarte le
/// même crédit venu des balises : (piste, rôle, nom en minuscules).
pub fn cle_de_doublon(track_id: i64, role: &str, nom: &str) -> (i64, String, String) {
    (
        track_id,
        role_de_comparaison(role.trim()).to_string(),
        nom.trim().to_lowercase(),
    )
}

/// Découpe la valeur d'une balise de crédit en (nom, instrument).
///
/// Séparateur `;` (et NUL, séparateur ID3v2.4 des valeurs multiples). Avec
/// `avec_instrument`, une parenthèse FERMANTE en fin de morceau détache
/// l'instrument : `Nasheet Waits (drums)` → (`Nasheet Waits`, `drums`). Un
/// morceau vide est ignoré ; une parenthèse sans nom devant reste le nom.
pub fn decouper_valeur(valeur: &str, avec_instrument: bool) -> Vec<(String, Option<String>)> {
    valeur
        .split([';', '\0'])
        .filter_map(|morceau| {
            let m = morceau.trim();
            if m.is_empty() {
                return None;
            }
            if avec_instrument
                && let Some(sans_fermante) = m.strip_suffix(')')
                && let Some(ouvrante) = sans_fermante.rfind('(')
            {
                let nom = sans_fermante[..ouvrante].trim();
                let instrument = sans_fermante[ouvrante + 1..].trim();
                if !nom.is_empty() {
                    return Some((
                        nom.to_string(),
                        (!instrument.is_empty()).then(|| instrument.to_string()),
                    ));
                }
            }
            Some((m.to_string(), None))
        })
        .collect()
}

/// Les crédits des balises de `track_ids`, dans l'ordre des pistes données
/// puis de [`CLES_DE_CREDIT`], sans ceux que `deja` (clés
/// [`cle_de_doublon`] des lignes de `track_credits`) porte déjà.
///
/// Une requête par clé de crédit pour tout l'ensemble de pistes
/// (`get_key_for_tracks`, servie par la clé primaire `(track_id, key)`), une
/// recherche de fiche par nom distinct. Une erreur de lecture ne casse pas le
/// tiroir : elle est journalisée et la clé n'apporte rien.
pub fn credits_des_balises(
    backend: &Arc<dyn DbBackend>,
    track_ids: &[i64],
    deja: &HashSet<(i64, String, String)>,
) -> Vec<CreditDeBalise> {
    if track_ids.is_empty() {
        return Vec::new();
    }
    let repo = TrackMetadataRepo::with_backend(backend.clone());
    let valeurs: Vec<(&'static str, &'static str, HashMap<i64, String>)> = CLES_DE_CREDIT
        .iter()
        .map(|&(cle, role)| {
            let par_piste = repo
                .get_key_for_tracks(cle, track_ids)
                .unwrap_or_else(|erreur| {
                    tracing::warn!(cle, %erreur, "credits_balises_lecture_impossible");
                    HashMap::new()
                });
            (cle, role, par_piste)
        })
        .collect();

    let artistes = ArtistRepo::with_backend(backend.clone());
    let mut fiches: HashMap<String, Option<i64>> = HashMap::new();
    let mut vus: HashSet<(i64, String, String)> = deja.clone();
    let mut sortie = Vec::new();
    for &track_id in track_ids {
        for (cle, role, par_piste) in &valeurs {
            let Some(valeur) = par_piste.get(&track_id) else {
                continue;
            };
            for (nom, instrument) in decouper_valeur(valeur, *cle == "performer") {
                if !vus.insert(cle_de_doublon(track_id, role, &nom)) {
                    continue;
                }
                let artist_id = *fiches.entry(nom.to_lowercase()).or_insert_with(|| {
                    artistes.get_by_name(&nom).ok().flatten().and_then(|a| a.id)
                });
                sortie.push(CreditDeBalise {
                    track_id,
                    artist_id,
                    artist_name: nom,
                    role,
                    instrument,
                });
            }
        }
    }
    sortie
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn la_valeur_de_la_capture_donne_un_interprete_par_nom_avec_son_instrument() {
        assert_eq!(
            decouper_valeur(
                "Christian McBride (bass); Nasheet Waits (drums); Marc Cary (keyboards)",
                true
            ),
            vec![
                ("Christian McBride".to_string(), Some("bass".to_string())),
                ("Nasheet Waits".to_string(), Some("drums".to_string())),
                ("Marc Cary".to_string(), Some("keyboards".to_string())),
            ]
        );
    }

    #[test]
    fn les_producteurs_se_decoupent_sans_instrument() {
        assert_eq!(
            decouper_valeur("Christian McBride; Todd Whitelock", false),
            vec![
                ("Christian McBride".to_string(), None),
                ("Todd Whitelock".to_string(), None),
            ]
        );
        // Hors `performer`, une parenthèse fait partie du nom.
        assert_eq!(
            decouper_valeur("Prince (1958-2016)", false),
            vec![("Prince (1958-2016)".to_string(), None)]
        );
    }

    #[test]
    fn morceaux_vides_nul_et_parenthese_seule_sont_tolerees() {
        assert_eq!(
            decouper_valeur(" ; A\0B (piano) ;; (voice) ; C ()", true),
            vec![
                ("A".to_string(), None),
                ("B".to_string(), Some("piano".to_string())),
                ("(voice)".to_string(), None),
                ("C".to_string(), None),
            ]
        );
    }

    #[test]
    fn le_parolier_des_balises_et_le_writer_mb_sont_le_meme_doublon() {
        assert_eq!(
            cle_de_doublon(3, "lyricist", " Jacques Brel "),
            cle_de_doublon(3, "writer", "jacques brel")
        );
        assert_ne!(
            cle_de_doublon(3, "producer", "X"),
            cle_de_doublon(3, "performer", "X")
        );
    }
}
