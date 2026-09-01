//! Lecture des facettes **à plusieurs valeurs** dans la chaîne de requête
//! (issue #2168).
//!
//! # Le format retenu : la clé RÉPÉTÉE
//!
//! `?format=aiff&format=flac` — la convention HTML des cases à cocher.
//!
//! Elle a été préférée à une liste séparée par des virgules (`format=aiff,flac`)
//! pour une raison de fond : **une valeur de facette peut contenir n'importe
//! quel caractère**. Un genre « Jazz, Blues », un label « Warner, Inc. », un
//! nom de liste de lecture ou un chemin de dossier avec une virgule auraient
//! été coupés en deux par le séparateur — c'est-à-dire une régression
//! silencieuse sur des filtres qui marchent aujourd'hui. La clé répétée n'a
//! aucun caractère réservé.
//!
//! # Rétrocompatibilité
//!
//! Une occurrence unique (`?format=flac`, ce que produisent toutes les URL et
//! tous les états enregistrés d'avant) rend une liste d'UN élément, et le SQL
//! généré est alors exactement celui d'avant (`= ?`, pas `IN (?)`). Rien à
//! migrer.
//!
//! # Pourquoi ne pas passer par `serde`
//!
//! `axum::extract::Query` s'appuie sur `serde_urlencoded`, qui ne sait pas
//! agréger une clé répétée. Pire : la `Deserialize` dérivée d'une structure
//! **REFUSE** une clé en double (`duplicate field`), donc tant qu'un champ
//! `format: Option<String>` existait, `?format=aiff&format=flac` rendait 400.
//!
//! Les structures `Query<T>` des routes ne portent donc plus que la pagination
//! et ce qui ne peut pas se répéter ; toutes les facettes se lisent ici, dans la
//! chaîne BRUTE. La validation de type qui vivait dans `serde` (un `?year=abc`
//! rendait 400) est reprise telle quelle par [`track_filter_from_raw`], qui rend
//! une `AppError` 400 — surtout pas « aucun filtre », qui rendrait la
//! bibliothèque entière.

use tune_core::db::facet_filter::{TrackFilter, normalize, normalize_ints};

use crate::error::AppError;

/// Toutes les valeurs de `key` dans la chaîne de requête, dans l'ordre
/// d'apparition, décodées.
///
/// Le décodage suit `application/x-www-form-urlencoded`, comme
/// `serde_urlencoded` : `+` vaut une espace, `%XX` un octet. Une séquence
/// invalide laisse la valeur brute plutôt que de faire disparaître la facette.
pub(super) fn values_of(raw: Option<&str>, key: &str) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for pair in raw.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        if decode(k) != key {
            continue;
        }
        out.push(decode(v));
    }
    out
}

/// Lit TOUTES les facettes d'Oxygen dans la chaîne de requête brute.
///
/// **Une seule lecture pour les deux étages.** `/library/tracks` (la liste) et
/// `/library/facets` (les effectifs) appellent cette fonction-ci ; ils ne
/// peuvent donc plus lire la requête différemment — et une facette qui
/// compterait autrement que la liste qu'elle filtre serait pire qu'une facette
/// absente. `/library/albums-detailed` et `/library/folder-facet` passent par
/// le même chemin.
///
/// `collection_ids` / `collection_track_ids` restent à la charge de l'appelant :
/// le nom d'une collection se résout en identifiants par deux moteurs distincts
/// (JSON manuel, règles intelligentes) et chaque route le fait déjà à sa façon.
pub(super) fn track_filter_from_raw(raw: Option<&str>) -> Result<TrackFilter, AppError> {
    let texts = |key: &str| normalize(&values_of(raw, key));
    let one = |key: &str| normalize(&values_of(raw, key)).into_iter().next();

    Ok(TrackFilter {
        genres: texts("genre"),
        years: ints(raw, "year")?,
        formats: texts("format"),
        sample_rates: ints(raw, "sample_rate")?,
        bit_depths: ints(raw, "bit_depth")?,
        sources: texts("source"),
        labels: texts("label"),
        composers: texts("composer"),
        artists: texts("artist"),
        countries: texts("country"),
        moods: texts("mood"),
        source_medias: texts("source_media"),
        ratings: ints(raw, "rating")?,
        original_years: ints(raw, "original_year")?,
        // `?dr=14&dr=13&dr=12` — la TRANCHE du ticket #2144, dite dans la
        // convention des cases à cocher : plusieurs valeurs d'une même facette
        // se combinent en OU. Clé courte `dr`, comme l'affichent les taggeurs
        // (DR14) et comme la nomme le testeur.
        dynamic_ranges: ints(raw, "dr")?,
        favorites: texts("favorite"),
        playlists: texts("playlist"),
        untagged: texts("untagged"),
        // Monovalués : voir `TrackFilter`.
        folder: one("folder"),
        collection_ids: None,
        collection_track_ids: None,
        q: one("q"),
    })
}

/// Valeurs numériques d'une facette, ou 400.
///
/// ⚠️ Une valeur non numérique doit REFUSER la requête, jamais être ignorée :
/// ignorée, elle rendrait la facette inactive — c'est-à-dire un filtre annoncé
/// qui laisse tout passer. C'est le comportement que `serde` assurait avant
/// #2168, repris ici mot pour mot.
fn ints(raw: Option<&str>, key: &str) -> Result<Vec<i64>, AppError> {
    let mut out = Vec::new();
    for brut in values_of(raw, key) {
        let v = brut.trim();
        // Une valeur vide vaut « facette non sélectionnée » — c'est ce que
        // le client envoie parfois en désélectionnant.
        if v.is_empty() {
            continue;
        }
        match v.parse::<i64>() {
            Ok(n) => out.push(n),
            Err(_) => {
                return Err(AppError::bad_request(format!(
                    "parametre `{key}` : valeur numerique attendue"
                )));
            }
        }
    }
    Ok(normalize_ints(&out))
}

/// Décodage `application/x-www-form-urlencoded` d'un fragment.
fn decode(s: &str) -> String {
    let plus_as_space = s.replace('+', " ");
    match urlencoding::decode(&plus_as_space) {
        Ok(v) => v.into_owned(),
        // Pourcentage mal formé : on garde le texte tel quel. Perdre la valeur
        // ferait disparaître la facette en silence, ce qui est exactement le
        // travers qu'on chasse ici.
        Err(_) => plus_as_space,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// La lecture telle que la font les routes, quand elle doit réussir.
    fn lu(raw: &str) -> TrackFilter {
        match track_filter_from_raw(Some(raw)) {
            Ok(f) => f,
            Err(_) => panic!("requête acceptable : {raw}"),
        }
    }

    /// Rétrocompatibilité : une URL d'avant #2168 porte une seule occurrence.
    #[test]
    fn une_seule_occurrence_rend_une_liste_dun_element() {
        assert_eq!(
            values_of(Some("format=flac&limit=50"), "format"),
            vec!["flac".to_string()]
        );
        assert_eq!(lu("year=1971").years, vec![1971]);
    }

    /// Le cas de Cyrille : aiff ET flac cochés ensemble.
    #[test]
    fn la_cle_repetee_rend_toutes_ses_valeurs_dans_lordre() {
        assert_eq!(
            values_of(Some("format=aiff&genre=jazz&format=flac"), "format"),
            vec!["aiff".to_string(), "flac".to_string()]
        );
        assert_eq!(
            lu("sample_rate=44100&sample_rate=96000").sample_rates,
            vec![44100, 96000]
        );
    }

    /// La raison d'être de la clé répétée : une valeur peut contenir une
    /// virgule, une espace, une barre oblique… et ne doit JAMAIS être coupée.
    #[test]
    fn une_valeur_a_virgule_nest_pas_coupee_en_deux() {
        let raw = "genre=Jazz%2C+Blues&label=Warner%2C%20Inc.";
        assert_eq!(
            values_of(Some(raw), "genre"),
            vec!["Jazz, Blues".to_string()]
        );
        assert_eq!(
            values_of(Some(raw), "label"),
            vec!["Warner, Inc.".to_string()]
        );
        // Un chemin Windows, tel que le porte la facette Répertoires.
        let raw = "folder=G%3A%5CBlues+2%5CSous-dossier";
        assert_eq!(
            values_of(Some(raw), "folder"),
            vec!["G:\\Blues 2\\Sous-dossier".to_string()]
        );
    }

    /// Une clé absente ou vide ne rend rien du tout — c'est `normalize` qui
    /// écartera ensuite les valeurs vides, et l'absence de valeur qui empêche
    /// tout prédicat d'être émis.
    #[test]
    fn absence_et_valeur_vide() {
        assert!(values_of(None, "format").is_empty());
        assert!(values_of(Some("genre=jazz"), "format").is_empty());
        assert_eq!(values_of(Some("format="), "format"), vec!["".to_string()]);
        // ⚠️ Une valeur numérique invalide REFUSE la requête : l'ignorer
        // rendrait la facette inactive, donc le filtre annoncé laisserait tout
        // passer. C'est ce que `serde` assurait avant #2168.
        assert!(track_filter_from_raw(Some("year=abc")).is_err());
        assert!(track_filter_from_raw(Some("sample_rate=44100&sample_rate=x")).is_err());
        // Une valeur numérique VIDE reste une facette non sélectionnée.
        assert!(lu("year=").years.is_empty());
    }

    /// Une clé encodée doit être reconnue, et une clé qui *commence* par le nom
    /// cherché ne doit pas l'être (`sample_rate` ≠ `sample_rate_max`).
    #[test]
    fn la_cle_est_comparee_entiere() {
        assert!(values_of(Some("sample_rate_max=96000"), "sample_rate").is_empty());
        assert_eq!(
            values_of(Some("source%5Fmedia=CD"), "source_media"),
            vec!["CD".to_string()]
        );
    }

    /// La demande de Cyrille, bout en bout : aiff + flac, et deux fréquences.
    #[test]
    fn la_demande_du_fil_1513() {
        let f = lu("format=aiff&format=flac&sample_rate=44100&sample_rate=96000");
        assert_eq!(f.formats, vec!["aiff".to_string(), "flac".to_string()]);
        assert_eq!(f.sample_rates, vec![44100, 96000]);
        assert!(f.is_active());
    }

    /// Rétrocompatibilité : une URL d'avant #2168 donne exactement le même
    /// filtre qu'avant — une valeur par facette, les facettes en ET.
    #[test]
    fn une_url_dune_seule_valeur_reste_lue_comme_avant() {
        let f = lu("genre=Jazz&format=flac&year=1971&limit=3000");
        assert_eq!(f.genres, vec!["Jazz".to_string()]);
        assert_eq!(f.formats, vec!["flac".to_string()]);
        assert_eq!(f.years, vec![1971]);
        assert!(f.is_active());
    }

    /// Le piège n°1 à l'entrée : une facette sans valeur ne doit RIEN activer.
    /// Sans ce garde-fou, `?format=` emprunte le chemin filtré, n'y produit
    /// aucun prédicat, et rend la bibliothèque entière.
    #[test]
    fn une_facette_vide_nactive_aucun_filtre() {
        for raw in ["", "format=", "genre=&label=&folder=", "limit=50&offset=0"] {
            let f = lu(raw);
            assert!(!f.is_active(), "{raw:?} ne doit activer aucun filtre");
        }
        let Ok(vide) = track_filter_from_raw(None) else {
            panic!("une requête sans chaîne est acceptable");
        };
        assert!(!vide.is_active());
    }

    /// Vocabulaire FERMÉ : une valeur inconnue ne produit aucun prédicat, donc
    /// elle ne doit pas non plus activer le chemin filtré.
    ///
    /// ⚠️ C'est un défaut RÉEL corrigé au passage : avant #2168, `?favorite=1`
    /// comptait comme un filtre, ne produisait aucune condition, et rendait la
    /// bibliothèque ENTIÈRE — avec un total qui la confirmait.
    #[test]
    fn une_valeur_hors_vocabulaire_ne_rend_pas_toute_la_bibliotheque() {
        assert!(!lu("favorite=1").is_active());
        assert!(!lu("untagged=mbid").is_active());
        assert!(lu("favorite=album").is_active());
        assert!(lu("untagged=cover").is_active());
    }

    /// La tranche de Dynamic Range se dit en clés répétées (#2144).
    ///
    /// Trois pastilles cochées = trois occurrences de `dr` = la tranche
    /// « DR12 à DR14 », en OU comme toute facette. Une seule occurrence reste
    /// un filtre exact, et l'ordre de la requête est conservé.
    #[test]
    fn la_tranche_de_dr_se_lit_en_cles_repetees() {
        assert_eq!(lu("dr=14").dynamic_ranges, vec![14]);
        assert_eq!(lu("dr=14&dr=13&dr=12").dynamic_ranges, vec![14, 13, 12]);
        // DR0 est une MESURE (un master entièrement écrasé), pas une absence :
        // il doit rester cochable.
        assert_eq!(lu("dr=0").dynamic_ranges, vec![0]);
        assert!(lu("dr=14").is_active());
        // Décochée, la facette ne filtre rien — et surtout n'active pas le
        // chemin filtré, qui rendrait la bibliothèque entière.
        assert!(lu("dr=").dynamic_ranges.is_empty());
        assert!(!lu("dr=").is_active());
        // Non numérique : 400, jamais « aucun filtre ».
        assert!(track_filter_from_raw(Some("dr=DR14")).is_err());
        // `dr` est comparée ENTIÈRE : `dr_min` appartient à la grille
        // d'albums, pas au rail, et ne doit pas être avalée ici.
        assert!(lu("dr_min=14").dynamic_ranges.is_empty());
        assert!(lu("dr_max=7").dynamic_ranges.is_empty());
    }

    /// Doublons écartés, ordre conservé : deux clics sur la même valeur ne
    /// doivent pas doubler un marqueur.
    #[test]
    fn les_doublons_sont_ecartes() {
        let f = lu("format=flac&format=flac&format=aiff");
        assert_eq!(f.formats, vec!["flac".to_string(), "aiff".to_string()]);
    }
}
