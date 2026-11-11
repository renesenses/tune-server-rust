//! tune-server-rust#8264 (demande #7781, go de Bertrand du 04/11, lot rc12) —
//! « combien de titres de Jean-Sébastien Bach ? combien de titres du genre
//! concertos pour violon ? » (jfpaquet, fils 2634 et 2733, 742 000 pistes).
//!
//! `GET /library/stats` sert, sous la clé ADDITIVE `rankings`, deux
//! classements par nombre de PISTES, du plus fourni au moins fourni :
//!
//! * `by_artist` : l'artiste de la piste (`tracks.artist_id` → `artists.name`),
//!   regroupé par nom ; une piste sans fiche d'artiste n'y compte pas ;
//! * `by_genre` : les genres de l'ALBUM de la piste, découpés par
//!   `genres_de_l_album` — LA définition du genre de Tune, celle de l'onglet
//!   Genres et de `GET /library/genres` — et rangés sous le libellé le plus
//!   fréquent. Une piste d'un album « Baroque ; Concerto » compte pour les
//!   deux genres.
//!
//! Chaque liste est plafonnée à [`PLAFOND_CLASSEMENT`] entrées : c'est un
//! classement, pas l'inventaire ; `artists` (déjà servi) et `total` disent
//! combien il y en a en tout. Ordre total et identique sur SQLite et
//! PostgreSQL : nombre décroissant, puis libellé.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;
use serde_json::{Value, json};
use tune_core::db::backend::DbBackend;

use super::genres::genres_de_l_album;

/// Combien d'entrées par classement.
pub(crate) const PLAFOND_CLASSEMENT: usize = 100;

/// Une ligne d'un classement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Rang {
    pub value: String,
    pub tracks: i64,
}

/// Un classement : les premières entrées, et combien il y en a en tout.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Classement {
    pub items: Vec<Rang>,
    pub total: usize,
}

fn classer(mut rangs: Vec<Rang>) -> Classement {
    rangs.sort_by(|a, b| b.tracks.cmp(&a.tracks).then_with(|| a.value.cmp(&b.value)));
    let total = rangs.len();
    rangs.truncate(PLAFOND_CLASSEMENT);
    Classement {
        items: rangs,
        total,
    }
}

/// Les artistes par nombre de pistes.
pub(crate) fn par_artiste(db: &dyn DbBackend) -> Result<Classement, String> {
    let lignes = db.query_many(
        "SELECT TRIM(ar.name), COUNT(*) FROM tracks t \
         JOIN artists ar ON ar.id = t.artist_id \
         WHERE ar.name IS NOT NULL AND TRIM(ar.name) != '' \
         GROUP BY TRIM(ar.name)",
        &[],
    )?;
    let rangs = lignes
        .iter()
        .filter_map(|l| {
            Some(Rang {
                value: l.first()?.as_string()?,
                tracks: l.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
            })
        })
        .collect();
    Ok(classer(rangs))
}

/// Les genres (de l'album) par nombre de pistes.
pub(crate) fn par_genre(db: &dyn DbBackend) -> Result<Classement, String> {
    let lignes = db.query_many(
        "SELECT al.genre, al.genres, COUNT(*) FROM tracks t \
         JOIN albums al ON al.id = t.album_id \
         WHERE (al.genre IS NOT NULL AND al.genre != '') \
            OR (al.genres IS NOT NULL AND al.genres != '') \
         GROUP BY al.genre, al.genres",
        &[],
    )?;
    let tuples: Vec<(Option<String>, Option<String>, i64)> = lignes
        .iter()
        .map(|l| {
            (
                l.first().and_then(|v| v.as_string()),
                l.get(1).and_then(|v| v.as_string()),
                l.get(2).and_then(|v| v.as_i64()).unwrap_or(0),
            )
        })
        .collect();
    Ok(classer(cumul_par_genre(&tuples)))
}

/// Le cumul des pistes par clé de genre, sous le libellé le plus fréquent
/// (à égalité, le plus petit dans l'ordre lexicographique — la règle de
/// `list_genres`). Fonction pure : c'est elle que gardent les témoins.
fn cumul_par_genre(lignes: &[(Option<String>, Option<String>, i64)]) -> Vec<Rang> {
    let mut groupes: HashMap<String, BTreeMap<String, i64>> = HashMap::new();
    for (genre, genres, pistes) in lignes {
        for (cle, libelle) in genres_de_l_album(genre.as_deref(), genres.as_deref()) {
            *groupes.entry(cle).or_default().entry(libelle).or_insert(0) += *pistes;
        }
    }
    groupes
        .into_values()
        .filter_map(|graphies| {
            let tracks = graphies.values().sum();
            let value = graphies
                .iter()
                .max_by(|(an, ac), (bn, bc)| ac.cmp(bc).then_with(|| bn.cmp(an)))
                .map(|(nom, _)| nom.clone())?;
            Some(Rang { value, tracks })
        })
        .collect()
}

/// La valeur de la clé `rankings` : chaque classement à `null` si sa lecture
/// échoue (journalisée), sans faire tomber la route.
pub(crate) fn lire(db: &dyn DbBackend) -> Value {
    let lu = |nom: &str, r: Result<Classement, String>| match r {
        Ok(c) => json!(c),
        Err(e) => {
            tracing::warn!(classement = nom, erreur = %e, "library_stats_rankings");
            Value::Null
        }
    };
    json!({
        "by_artist": lu("by_artist", par_artiste(db)),
        "by_genre": lu("by_genre", par_genre(db)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rang(v: &str, n: i64) -> Rang {
        Rang {
            value: v.into(),
            tracks: n,
        }
    }

    #[test]
    fn le_genre_suit_la_definition_de_l_onglet_genres_8264() {
        let lignes = vec![
            // Deux graphies d'un même genre : une seule entrée, libellé majoritaire.
            (Some("Violin Concerto".to_string()), None, 30),
            (Some("Violin-Concerto".to_string()), None, 2),
            // Un album à deux genres compte pour chacun.
            (
                None,
                Some(r#"["Baroque","Violin Concerto"]"#.to_string()),
                10,
            ),
        ];
        let mut r = cumul_par_genre(&lignes);
        r.sort_by_key(|x| std::cmp::Reverse(x.tracks));
        assert_eq!(r, vec![rang("Violin Concerto", 42), rang("Baroque", 10)]);
    }

    #[test]
    fn le_classement_est_plafonne_et_dit_son_total_8264() {
        let rangs = (0..(PLAFOND_CLASSEMENT as i64 + 5))
            .map(|i| rang(&format!("A{i:03}"), i))
            .collect();
        let c = classer(rangs);
        assert_eq!(c.total, PLAFOND_CLASSEMENT + 5);
        assert_eq!(c.items.len(), PLAFOND_CLASSEMENT);
        assert_eq!(c.items[0].tracks, PLAFOND_CLASSEMENT as i64 + 4);
    }

    #[test]
    fn a_egalite_l_ordre_est_celui_du_libelle_8264() {
        let c = classer(vec![rang("B", 3), rang("A", 3), rang("C", 9)]);
        assert_eq!(c.items, vec![rang("C", 9), rang("A", 3), rang("B", 3)]);
    }
}
