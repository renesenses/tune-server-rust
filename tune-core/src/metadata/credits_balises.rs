//! Les crédits lus dans les balises des fichiers, écrits au scan dans
//! `track_credits` avec `source = 'balises'` (#7766, lot 2 / PR 2).
//!
//! Jusqu'ici, les balises de crédit (COMPOSER, CONDUCTOR, PERFORMER…)
//! n'atteignaient que `track_metadata` : la route des crédits les relisait à
//! chaque requête, et seulement l'interprète et le producteur ; la page
//! artiste, qui lit `track_credits`, n'en voyait rien.
//!
//! Le module part des clés que [`super::read_extended_metadata`] a déjà
//! rangées dans `track_metadata` : aucune relecture du fichier, le scan
//! n'y paie qu'une purge et quelques insertions par piste.
//!
//! Il ne touche QUE ses propres lignes : la purge ne prend que
//! `source = 'balises'`. Les crédits MusicBrainz, Discogs, Roon, manuels et les
//! lignes anciennes de source inconnue (NULL) restent en place — contrairement
//! à [`super::credits_release::ecrire_credits_piste`], qui purge aussi les
//! NULL.

use std::collections::HashMap;
use std::sync::Arc;

use super::credits_mb::{LigneCredit, role_canonique};
use super::instruments::canoniser_instrument;
use super::source_des_credits::BALISES;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::library::regle_compilation::est_artistes_divers;

/// Clé de `track_metadata` → type de relation MusicBrainz équivalent.
///
/// Le rôle écrit en base passe ensuite par [`role_canonique`] : les crédits
/// lus dans les fichiers et ceux venus de MusicBrainz parlent la même langue,
/// et une Smart Collection `role: engineer` trouve les deux.
///
/// L'ordre est celui de l'écriture : un compositeur avant ses interprètes,
/// comme sur une pochette.
pub const CLES_DE_CREDIT: [(&str, &str); 12] = [
    ("composer", "composer"),
    ("lyricist", "lyricist"),
    ("writer", "writer"),
    ("arranger", "arranger"),
    ("conductor", "conductor"),
    ("ensemble", "performing orchestra"),
    ("performer", "performer"),
    ("producer", "producer"),
    ("engineer", "engineer"),
    ("mixer", "mix"),
    ("djmixer", "mix-DJ"),
    ("remixer", "remixer"),
];

/// Les clés brutes d'un ensemble ou d'un orchestre (Vorbis, `TXXX:` ID3,
/// atome MP4 libre) : lofty n'en connaît aucune.
pub const CLES_ENSEMBLE: [&str; 2] = ["ENSEMBLE", "ORCHESTRA"];

/// Ajoute `valeur` à la clé `cle` de `meta`, jointe par « ; », sans doublon.
fn ajouter(meta: &mut HashMap<String, String>, cle: &str, valeur: &str) {
    let valeur = valeur.trim();
    if valeur.is_empty() {
        return;
    }
    match meta.get_mut(cle) {
        Some(deja) => {
            if !deja.split(';').any(|v| v.trim() == valeur) {
                deja.push_str("; ");
                deja.push_str(valeur);
            }
        }
        None => {
            meta.insert(cle.to_owned(), valeur.to_owned());
        }
    }
}

/// Complète les métadonnées étendues par les clés brutes du fichier
/// (`balises_brutes`) que lofty ne range sous aucun `ItemKey` :
///
/// - ENSEMBLE / ORCHESTRA → `ensemble` ;
/// - la trame ID3v2.4 TMCL, « instrument: nom ; instrument: nom », → une
///   entrée « nom (instrument) » de `performer`, la forme Vorbis ;
/// - l'atome MP4 `----:com.apple.iTunes:ARRANGER` → `arranger`.
pub fn completer_par_les_cles_brutes(
    meta: &mut HashMap<String, String>,
    brutes: &[(String, String)],
) {
    for (cle, valeur) in brutes {
        let nom = super::balises_brutes::nom_de_cle(cle);
        match nom.as_str() {
            "ENSEMBLE" | "ORCHESTRA" => {
                for v in valeur.split(';') {
                    ajouter(meta, "ensemble", v);
                }
            }
            "TMCL" => {
                for paire in valeur.split(" ; ") {
                    let Some((instrument, personne)) = paire.split_once(':') else {
                        continue;
                    };
                    let (instrument, personne) = (instrument.trim(), personne.trim());
                    if personne.is_empty() {
                        continue;
                    }
                    let entree = if instrument.is_empty() {
                        personne.to_owned()
                    } else {
                        format!("{personne} ({instrument})")
                    };
                    ajouter(meta, "performer", &entree);
                }
            }
            "ARRANGER" if cle.starts_with("----:") => {
                for v in valeur.split(';') {
                    ajouter(meta, "arranger", v);
                }
            }
            _ => {}
        }
    }
}

/// Sépare « Nom (instrument) » en ses deux parties.
///
/// Le découpage se fait à la DERNIÈRE parenthèse ouvrante : « Orchestre
/// (de chambre) de Paris (violon) » garde son nom entier. Une entrée sans
/// parenthèse finale, ou dont l'une des deux parties serait vide, rend le
/// nom tel quel. L'instrument est rendu BRUT : la canonisation est l'affaire
/// de l'appelant.
///
/// Le découpage UNIQUE des crédits de balise : la route des crédits
/// (`routes/library/credits.rs`) l'emploie aussi.
pub fn nom_et_instrument(entree: &str) -> (String, Option<String>) {
    let entree = entree.trim();
    if let Some(sans_parenthese) = entree.strip_suffix(')')
        && let Some((nom, instrument)) = sans_parenthese.rsplit_once(" (")
        && !nom.trim().is_empty()
        && !instrument.trim().is_empty()
    {
        return (nom.trim().to_owned(), Some(instrument.trim().to_owned()));
    }
    (entree.to_owned(), None)
}

/// Les lignes de crédit portées par les métadonnées étendues d'une piste.
///
/// Une valeur porte une ou plusieurs personnes séparées par « ; » — le
/// séparateur de [`super::read_extended_metadata`] pour une balise répétée.
/// « & » et « , » ne coupent PAS : « Simon & Garfunkel » est un seul crédit.
/// « Various Artists » et ses variantes ne sont pas des personnes et sont
/// écartés. Un même crédit (nom sans casse, rôle, instrument) n'est écrit
/// qu'une fois : ID3 range TEXT à la fois en parolier et en auteur.
pub fn lignes_des_balises(meta: &HashMap<String, String>) -> Vec<LigneCredit> {
    let mut lignes: Vec<LigneCredit> = Vec::new();
    for (cle, type_mb) in CLES_DE_CREDIT {
        let Some(valeur) = meta.get(cle) else {
            continue;
        };
        let Some(role) = role_canonique(type_mb) else {
            continue;
        };
        for entree in valeur.split(';') {
            let (nom, instrument) = if role == "performer" {
                nom_et_instrument(entree)
            } else {
                (entree.trim().to_owned(), None)
            };
            if nom.is_empty() || est_artistes_divers(&nom) {
                continue;
            }
            let instrument = instrument
                .map(|i| canoniser_instrument(&i))
                .filter(|i| !i.is_empty());
            let deja = lignes.iter().any(|l| {
                l.role == role
                    && l.instrument == instrument
                    && l.artist_name.to_lowercase() == nom.to_lowercase()
            });
            if deja {
                continue;
            }
            lignes.push(LigneCredit {
                artist_name: nom,
                role: role.to_owned(),
                instrument,
                artist_mbid: None,
            });
        }
    }
    lignes
}

/// Remplace les crédits `balises` d'une piste par `lignes`.
///
/// Idempotente : deux scans de suite laissent exactement les mêmes lignes.
/// Aucune fiche artiste n'est cherchée ici (`artist_id` reste NULL) : le scan
/// ne peut pas payer deux requêtes par crédit, et la page artiste retrouve
/// déjà une ligne par son nom.
pub fn ecrire_credits_des_balises(
    backend: &Arc<dyn DbBackend>,
    track_id: i64,
    lignes: &[LigneCredit],
) -> Result<usize, String> {
    let source = BALISES;
    backend.execute(
        "DELETE FROM track_credits WHERE track_id = ? AND source = ?",
        &[&track_id as &dyn ToSqlValue, &source as &dyn ToSqlValue],
    )?;
    for (pos, ligne) in lignes.iter().enumerate() {
        let pos = pos as i32;
        backend.execute(
            "INSERT INTO track_credits (track_id, artist_id, artist_name, role, instrument, position, artist_mbid, source) \
             VALUES (?, NULL, ?, ?, ?, ?, NULL, ?)",
            &[
                &track_id as &dyn ToSqlValue,
                &ligne.artist_name as &dyn ToSqlValue,
                &ligne.role as &dyn ToSqlValue,
                &ligne.instrument as &dyn ToSqlValue,
                &pos as &dyn ToSqlValue,
                &source as &dyn ToSqlValue,
            ],
        )?;
    }
    Ok(lignes.len())
}

/// Écrit les crédits `balises` d'un lot de scan, à partir des métadonnées
/// étendues que le lot vient de ranger dans `track_metadata`.
///
/// Appelée par le scan manuel et le scan automatique, dans la transaction du
/// lot, juste après `TrackMetadataRepo::set_batch_multi`. Une piste en échec
/// est journalisée et n'arrête pas le lot. Rend le nombre de lignes écrites.
pub fn ecrire_lot(backend: &Arc<dyn DbBackend>, lot: &[(i64, HashMap<String, String>)]) -> usize {
    let mut ecrites = 0usize;
    for (track_id, meta) in lot {
        let lignes = lignes_des_balises(meta);
        match ecrire_credits_des_balises(backend, *track_id, &lignes) {
            Ok(n) => ecrites += n,
            Err(e) => {
                tracing::warn!(track_id, error = %e, "scan_credits_des_balises_echec");
            }
        }
    }
    ecrites
}
