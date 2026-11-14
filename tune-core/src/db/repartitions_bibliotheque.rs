//! #7849 — les répartitions de la bibliothèque locale, servies par
//! `GET /library/stats` sous la clé `distributions` : par codec, par
//! fréquence d'échantillonnage, par décennie de sortie et par rôle de crédit.
//!
//! Chaque répartition est une liste de cases `{ value, tracks }` (plus
//! `people` pour les rôles). `value` vaut `null` pour « inconnu » : le client
//! traduit lui-même ce libellé.
//!
//! - codec : `tracks.format`, en minuscules, ramené à son CODEC
//!   (`codec_canonique`) ; vide ou absent → inconnu ;
//! - fréquence : `tracks.sample_rate` en Hz ; absente ou 0 → inconnue ;
//! - décennie : l'année de la piste, à défaut celle de son album ; absente ou
//!   nulle → inconnue ;
//! - rôle : `track_credits.role` en minuscules ; `tracks` compte les pistes
//!   distinctes, `people` les personnes distinctes (fiche d'artiste si elle
//!   existe, nom sinon).

use serde::Serialize;

use super::backend::{DbBackend, SqlValue};
use super::engine::Engine;

/// Une case d'une répartition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Case<V> {
    pub value: Option<V>,
    pub tracks: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub people: Option<i64>,
}

/// Les quatre répartitions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Repartitions {
    pub by_codec: Vec<Case<String>>,
    pub by_sample_rate: Vec<Case<i64>>,
    pub by_decade: Vec<Case<i64>>,
    pub by_credit_role: Vec<Case<String>>,
}

/// Un entier en PostgreSQL même si la colonne d'une ancienne bascule est
/// restée en TEXT ; tel quel en SQLite.
fn entier(engine: Engine, colonne: &str) -> String {
    match engine {
        Engine::Sqlite => colonne.to_string(),
        Engine::Postgres => format!("CAST({colonne} AS BIGINT)"),
    }
}

fn entier_de(v: Option<&SqlValue>) -> i64 {
    v.and_then(|v| v.as_i64()).unwrap_or(0)
}

fn lignes(db: &dyn DbBackend, sql: &str) -> Result<Vec<Vec<SqlValue>>, String> {
    db.query_many(sql, &[])
}

pub fn lire(db: &dyn DbBackend) -> Result<Repartitions, String> {
    let e = db.engine();

    // Recette du 14/11 : le format brut, plus « a-t-elle une profondeur de
    // bits ? » (qui sépare ALAC d'AAC dans un MP4), puis le cumul par codec.
    let profondeur = entier(e, "bit_depth");
    let by_codec = cumul_par_codec(
        lignes(
            db,
            &format!(
                "SELECT NULLIF(LOWER(TRIM(COALESCE(format, ''))), '') AS codec, \
                        CASE WHEN COALESCE({profondeur}, 0) > 0 THEN 1 ELSE 0 END, \
                        COUNT(*) \
                 FROM tracks GROUP BY 1, 2"
            ),
        )?
        .iter()
        .map(|l| {
            (
                l.first().and_then(|v| v.as_string()),
                entier_de(l.get(1)) > 0,
                entier_de(l.get(2)),
            )
        }),
    );

    let frequence = entier(e, "sample_rate");
    let by_sample_rate = lignes(
        db,
        &format!(
            "SELECT NULLIF({frequence}, 0) AS frequence, COUNT(*) \
             FROM tracks GROUP BY 1"
        ),
    )?
    .iter()
    .map(|l| Case {
        value: l.first().and_then(|v| v.as_i64()),
        tracks: entier_de(l.get(1)),
        people: None,
    })
    .collect();

    // Division entière : les deux opérandes sont entiers en SQLite comme en
    // PostgreSQL, `1987 / 10 * 10` vaut 1980 des deux côtés.
    let annee = format!(
        "NULLIF(COALESCE(NULLIF({}, 0), NULLIF({}, 0)), 0)",
        entier(e, "t.year"),
        entier(e, "a.year")
    );
    let by_decade = lignes(
        db,
        &format!(
            "SELECT ({annee}) / 10 * 10 AS decennie, COUNT(*) \
             FROM tracks t LEFT JOIN albums a ON a.id = t.album_id \
             GROUP BY 1"
        ),
    )?
    .iter()
    .map(|l| Case {
        value: l.first().and_then(|v| v.as_i64()),
        tracks: entier_de(l.get(1)),
        people: None,
    })
    .collect();

    // La personne : `id:<fiche>` ou `nom:<nom en minuscules>`, pour qu'un id
    // et un nom ne se confondent jamais.
    let by_credit_role = lignes(
        db,
        "SELECT NULLIF(LOWER(TRIM(COALESCE(role, ''))), '') AS role, \
                COUNT(DISTINCT track_id), \
                COUNT(DISTINCT CASE WHEN artist_id IS NOT NULL \
                    THEN 'id:' || CAST(artist_id AS TEXT) \
                    ELSE 'nom:' || LOWER(TRIM(artist_name)) END) \
         FROM track_credits GROUP BY 1",
    )?
    .iter()
    .map(|l| Case {
        value: l.first().and_then(|v| v.as_string()),
        tracks: entier_de(l.get(1)),
        people: Some(entier_de(l.get(2))),
    })
    .collect();

    let mut r = Repartitions {
        by_codec,
        by_sample_rate,
        by_decade,
        by_credit_role,
    };
    ranger(&mut r);
    Ok(r)
}

/// Recette de Bertrand du 14/11 — la carte « Formats » listait « MPEG » ET
/// « MP3 », « DSF » et « DSD », « MP4 » à côté de « AAC » / « ALAC ». Sur le
/// .18, MPEG, DSD et MP4 viennent tous des pistes UPnP, dont le format est le
/// sous-type MIME du serveur multimédia (`audio/mpeg`, `audio/dsd`,
/// `audio/mp4`), pas une extension.
///
/// Le libellé d'une statistique est le CODEC, pas le conteneur :
/// - `mpeg` / `mpga` → `mp3` ;
/// - `dsf`, `dff`, `dsd` → `dsd` : le même flux DSD ; le `dsd` d'un serveur
///   UPnP ne dit pas son conteneur, et le séparer de `dsf` coupait en deux la
///   même chose ;
/// - `mp4` / `m4a` → `alac` si la piste a une profondeur de bits (codec sans
///   perte lu ; sur le .18, les 111 pistes MP4 sont à 24 bits), `m4a` sinon :
///   le scanner écrit `m4a` quand il n'a PAS lu le codec (fil 2126, #5710 —
///   un ALAC affiché « LOSSY AAC ») ; en faire un AAC referait cette erreur.
///
/// Un sous-type MIME (`audio/x-dsf`) est ramené à `dsf` d'abord. Même règle
/// côté client (`formatCanonique`, `typeDeFichier.ts`), pour que la barre
/// cliquée filtre la Bibliothèque sur le même codec.
pub(crate) fn codec_canonique(format: &str, a_une_profondeur: bool) -> String {
    let f = format.trim().to_lowercase();
    let f = f.rsplit('/').next().unwrap_or("");
    let f = f.strip_prefix("x-").unwrap_or(f);
    match f {
        "mpeg" | "mpga" => "mp3".into(),
        "dsf" | "dff" | "dsdiff" => "dsd".into(),
        "mp4" | "m4a" if a_une_profondeur => "alac".into(),
        "mp4" | "m4a" => "m4a".into(),
        autre => autre.into(),
    }
}

/// Le cumul des pistes par codec (`codec_canonique`) ; l'ordre est posé
/// ensuite par `ranger`.
fn cumul_par_codec(lignes: impl Iterator<Item = (Option<String>, bool, i64)>) -> Vec<Case<String>> {
    let mut cumul: std::collections::BTreeMap<Option<String>, i64> = Default::default();
    for (format, profondeur, n) in lignes {
        let codec = format
            .map(|f| codec_canonique(&f, profondeur))
            .filter(|c| !c.is_empty());
        *cumul.entry(codec).or_default() += n;
    }
    cumul
        .into_iter()
        .map(|(value, tracks)| Case {
            value,
            tracks,
            people: None,
        })
        .collect()
}

/// Le même ordre sur les deux moteurs (SQLite et PostgreSQL ne placent pas
/// NULL au même endroit) : « inconnu » toujours en dernier ; codecs et rôles
/// du plus fréquent au plus rare, fréquences et décennies croissantes.
fn ranger(r: &mut Repartitions) {
    fn par_nombre<V: Ord>(cases: &mut [Case<V>]) {
        cases.sort_by(|a, b| {
            a.value
                .is_none()
                .cmp(&b.value.is_none())
                .then(b.tracks.cmp(&a.tracks))
                .then(a.value.cmp(&b.value))
        });
    }
    fn par_valeur<V: Ord>(cases: &mut [Case<V>]) {
        cases.sort_by(|a, b| {
            a.value
                .is_none()
                .cmp(&b.value.is_none())
                .then(a.value.cmp(&b.value))
        });
    }
    par_nombre(&mut r.by_codec);
    par_nombre(&mut r.by_credit_role);
    par_valeur(&mut r.by_sample_rate);
    par_valeur(&mut r.by_decade);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_codec_et_non_le_conteneur_recette_14_11() {
        assert_eq!(codec_canonique("mpeg", false), "mp3");
        assert_eq!(codec_canonique("MP3", false), "mp3");
        assert_eq!(codec_canonique("audio/mpeg", false), "mp3");
        assert_eq!(codec_canonique("dsf", true), "dsd");
        assert_eq!(codec_canonique("DFF", true), "dsd");
        assert_eq!(codec_canonique("audio/x-dsd", true), "dsd");
        assert_eq!(codec_canonique("mp4", true), "alac");
        assert_eq!(codec_canonique("m4a", true), "alac");
        assert_eq!(codec_canonique("mp4", false), "m4a");
        assert_eq!(codec_canonique("m4a", false), "m4a");
        assert_eq!(codec_canonique("alac", true), "alac");
        assert_eq!(codec_canonique("aac", false), "aac");
        assert_eq!(codec_canonique("flac", true), "flac");
    }

    #[test]
    fn mpeg_et_mp3_ne_font_qu_une_case_recette_14_11() {
        let c = cumul_par_codec(
            vec![
                (Some("mpeg".to_string()), false, 146),
                (Some("mp3".to_string()), false, 143),
                (Some("dsf".to_string()), true, 740),
                (Some("dsd".to_string()), true, 636),
                (Some("mp4".to_string()), true, 111),
                (Some("alac".to_string()), true, 72),
                (None, false, 3),
            ]
            .into_iter(),
        );
        let paires: Vec<(Option<&str>, i64)> =
            c.iter().map(|k| (k.value.as_deref(), k.tracks)).collect();
        assert_eq!(
            paires,
            vec![
                (None, 3),
                (Some("alac"), 183),
                (Some("dsd"), 1376),
                (Some("mp3"), 289),
            ]
        );
    }
}
