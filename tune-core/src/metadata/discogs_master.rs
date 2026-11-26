//! Le MASTER Discogs d'un album : la clé qui réunit, sur Discogs, toutes les
//! éditions d'une même sortie (fil 3012, Dimitri : 19 pressages de
//! *Physical Graffiti*, GO de Bertrand du 23/11/2026 pour la rc14).
//!
//! Elle sert au regroupement des « Versions » d'un album
//! (`tune-server/src/routes/library/oeuvres.rs`), à côté du groupe de sortie
//! MusicBrainz.
//!
//! # Où elle vit
//!
//! Aucune colonne, aucune migration : les deux magasins clé-valeur qui
//! existent déjà sur les deux moteurs portent la même clé [`CLE`] :
//!
//! 1. `album_metadata` — posée sur l'ALBUM par un greffon ou un client,
//!    `PUT /api/v1/library/albums/{id}/metadata` ; elle prime ;
//! 2. `track_metadata` — sur chaque PISTE, lue des balises du fichier au scan
//!    ([`CLES_DE_BALISE`]) ou posée par `PUT /api/v1/library/tracks/{id}/metadata`
//!    (la porte qu'emploie déjà la balise `VERSION`) ; l'album la prend quand
//!    ses pistes n'en portent qu'UNE valeur.
//!
//! # La forme rangée
//!
//! Le numéro seul, en chiffres (`"12345"`). [`normaliser`] accepte ce qu'un
//! greffon ou une balise peut écrire : `12345`, `m12345`, `[m12345]`, ou
//! l'adresse d'une page master (`https://www.discogs.com/master/12345-…`).
//! L'adresse d'une RELEASE (`/release/…`) est refusée : un numéro de release
//! n'est pas un numéro de master, et les confondre grouperait au hasard.

/// La clé, dans `album_metadata` comme dans `track_metadata`.
pub const CLE: &str = "discogs_master_id";

/// Les noms de balise lus au scan, sans préfixe de conteneur (Vorbis
/// `DISCOGS_MASTER_ID`, ID3 `TXXX:DISCOGS_MASTER_ID`, MP4
/// `----:com.apple.iTunes:DISCOGS_MASTER_ID`, APE). Aucune balise n'est
/// normalisée pour le master Discogs : ces deux graphies sont celles que
/// Tune lit, la première est celle qu'il recommande.
pub const CLES_DE_BALISE: [&str; 2] = ["DISCOGS_MASTER_ID", "DISCOGS_MASTER_RELEASE_ID"];

/// Plus long numéro accepté : les masters Discogs tiennent en sept chiffres
/// aujourd'hui ; dix laisse de la marge sans accepter un nombre absurde.
const CHIFFRES_MAX: usize = 10;

/// Le numéro de master que dit `brut`, en chiffres, ou `None` quand `brut`
/// n'en dit pas un (vide, release, texte libre, zéro).
pub fn normaliser(brut: &str) -> Option<String> {
    let s = brut.trim();
    if s.is_empty() {
        return None;
    }
    let bas = s.to_ascii_lowercase();
    // Une adresse : seule une page MASTER est acceptée.
    let corps: &str = if bas.contains("://") || bas.contains("discogs.com") {
        let pos = bas.find("/master/")?;
        let reste = &s[pos + "/master/".len()..];
        let fin = reste
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(reste.len());
        &reste[..fin]
    } else {
        let s = s.trim_start_matches('[').trim_end_matches(']').trim();
        s.strip_prefix(['m', 'M']).unwrap_or(s).trim()
    };
    if corps.is_empty() || corps.len() > CHIFFRES_MAX || !corps.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let nu = corps.trim_start_matches('0');
    (!nu.is_empty()).then(|| nu.to_string())
}

/// Le master Discogs parmi des balises brutes `(clé, valeur)` : la première
/// des [`CLES_DE_BALISE`] dont la valeur se normalise.
pub fn parmi(paires: &[(String, String)]) -> Option<String> {
    CLES_DE_BALISE.iter().find_map(|attendue| {
        paires.iter().find_map(|(k, v)| {
            (super::balises_brutes::nom_de_cle(k) == *attendue)
                .then(|| normaliser(v))
                .flatten()
        })
    })
}

/// Ce qu'une écriture (`PUT …/metadata`) fait de la clé [`CLE`].
#[derive(Debug, PartialEq, Eq)]
pub enum Ecriture {
    /// La clé n'est pas dans le corps : rien à faire.
    Absente,
    /// La valeur a été rangée en chiffres dans le corps.
    Rangee,
    /// Valeur vide : la clé a été retirée du corps, l'appelant EFFACE la
    /// valeur stockée.
    Effacer,
}

/// Prépare le corps d'un `PUT …/metadata` : range la valeur de [`CLE`] en
/// chiffres ([`normaliser`]), ou la retire du corps quand elle est vide.
/// `Err` (le message à rendre en 400) quand la valeur ne dit pas un master.
pub fn preparer_ecriture(
    corps: &mut std::collections::HashMap<String, String>,
) -> Result<Ecriture, String> {
    let Some(brut) = corps.get(CLE).cloned() else {
        return Ok(Ecriture::Absente);
    };
    if brut.trim().is_empty() {
        corps.remove(CLE);
        return Ok(Ecriture::Effacer);
    }
    match normaliser(&brut) {
        Some(v) => {
            corps.insert(CLE.to_string(), v);
            Ok(Ecriture::Rangee)
        }
        None => Err(format!(
            "{CLE}: expected a Discogs master id (12345, m12345 or a discogs.com/master/ URL), got {brut:?}"
        )),
    }
}

#[cfg(test)]
#[path = "discogs_master_tests.rs"]
mod tests;
