//! « Joue avec » de la page artiste (#7768, lot 2 / PR 5) : les artistes
//! AUDIBLES sur les mêmes pistes que lui, pondérés par le nombre de pistes
//! partagées.
//!
//! Contrat (`coordination/seb-lot2/contrat-route-artiste.md`, § 3.3), clé
//! additive `played_with` de `GET /library/artists/{id}/albums?sections=1` :
//!
//! * ses pistes : celles où il est AUDIBLE — artiste de la piste, ou crédité
//!   avec un rôle de la table unique [`ROLES_AUDIBLES`] (remixeur compris),
//!   reconnu par sa fiche, son nom exact ou son MBID ;
//! * sur ces pistes, les autres artistes audibles : l'artiste de la piste et
//!   les crédits de [`ROLES_AUDIBLES`]. Un compositeur, un producteur, un
//!   arrangeur partagés ne comptent pas ;
//! * jamais lui-même (fiche, nom replié ou MBID), jamais « Various Artists »
//!   ([`est_artistes_divers`]) ;
//! * `shared_tracks` = pistes DISTINCTES partagées (les doublons de sources
//!   de #7766 comptent une fois) ; ordre : `shared_tracks` décroissant, puis
//!   nom sans casse ; [`TAILLE_JOUE_AVEC`] au plus ;
//! * `artist_id` = la fiche quand elle existe, sinon `null`. Un crédit sans
//!   fiche est rattaché à la fiche de même nom replié (ou de même MBID) vue
//!   sur ces pistes, puis, pour les retenus, à la fiche de même nom en base.
//!
//! Albums masqués exclus, comme partout sur la page artiste.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use serde_json::{Value, json};
use tracing::warn;

use super::credits_release::{
    ROLES_AUDIBLES, credits_de_l_artiste_sql, est_artistes_divers, liste_sql,
};
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::db::engine::Engine;

/// Taille de la liste (contrat § 3.3).
pub const TAILLE_JOUE_AVEC: usize = 20;

/// Un élément de `played_with`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoueAvec {
    pub artist_id: Option<i64>,
    pub artist_name: String,
    pub shared_tracks: i64,
}

impl JoueAvec {
    pub fn to_json(&self) -> Value {
        json!({
            "artist_id": self.artist_id,
            "artist_name": self.artist_name,
            "shared_tracks": self.shared_tracks,
        })
    }
}

/// Un artiste audible sur une piste où l'artiste de la page est audible :
/// l'artiste de la piste ou un crédit audible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Voisin {
    pub track_id: i64,
    pub fiche: Option<i64>,
    pub nom: String,
    pub mbid: Option<String>,
}

/// L'artiste de la page : ce qui l'écarte de sa propre liste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Soi {
    pub artist_id: i64,
    pub nom: String,
    pub mbid: Option<String>,
}

/// Le pli d'un nom : sans accents, sans casse, espaces réduits.
pub fn replier(nom: &str) -> String {
    crate::db::engine::fold_diacritics(nom)
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn mbid_utile(m: &Option<String>) -> Option<&str> {
    m.as_deref().map(str::trim).filter(|m| !m.is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Cle {
    Fiche(i64),
    Nom(String),
}

/// Un groupe avant résolution des noms : la clé, un nom à montrer, ses
/// pistes, et un MBID vu (pour retrouver la fiche d'un crédit sans fiche).
#[derive(Debug, Clone)]
pub struct Groupe {
    pub fiche: Option<i64>,
    pub nom: String,
    pub mbid: Option<String>,
    pub pistes: HashSet<i64>,
}

/// Le cœur, sans base : écarte lui-même et « Various Artists », groupe par
/// fiche (un crédit sans fiche rejoint la fiche de même nom replié ou de même
/// MBID vue dans `voisins`), compte les pistes distinctes et trie. Rend TOUS
/// les groupes triés ; l'appelant coupe.
pub fn grouper(voisins: &[Voisin], soi: &Soi) -> Vec<Groupe> {
    let soi_nom = replier(&soi.nom);
    let soi_mbid = mbid_utile(&soi.mbid).map(str::to_string);
    let est_soi = |v: &Voisin| {
        v.fiche == Some(soi.artist_id)
            || (!soi_nom.is_empty() && replier(&v.nom) == soi_nom)
            || (soi_mbid.is_some() && mbid_utile(&v.mbid) == soi_mbid.as_deref())
    };
    let gardes: Vec<&Voisin> = voisins
        .iter()
        .filter(|v| !v.nom.trim().is_empty() || v.fiche.is_some())
        .filter(|v| !est_soi(v) && !est_artistes_divers(&v.nom))
        .collect();

    // Les fiches vues, par nom replié et par MBID.
    let mut fiche_par_nom: HashMap<String, i64> = HashMap::new();
    let mut fiche_par_mbid: HashMap<String, i64> = HashMap::new();
    for v in &gardes {
        if let Some(id) = v.fiche {
            fiche_par_nom.entry(replier(&v.nom)).or_insert(id);
            if let Some(m) = mbid_utile(&v.mbid) {
                fiche_par_mbid.entry(m.to_string()).or_insert(id);
            }
        }
    }

    let mut ordre: Vec<Cle> = Vec::new();
    let mut groupes: HashMap<Cle, Groupe> = HashMap::new();
    for v in gardes {
        let fiche = v
            .fiche
            .or_else(|| mbid_utile(&v.mbid).and_then(|m| fiche_par_mbid.get(m).copied()))
            .or_else(|| fiche_par_nom.get(&replier(&v.nom)).copied());
        let cle = match fiche {
            Some(id) => Cle::Fiche(id),
            None => Cle::Nom(replier(&v.nom)),
        };
        let g = groupes.entry(cle.clone()).or_insert_with(|| {
            ordre.push(cle);
            Groupe {
                fiche,
                nom: v.nom.trim().to_string(),
                mbid: None,
                pistes: HashSet::new(),
            }
        });
        // Le nom de la fiche elle-même l'emporte sur celui d'un crédit
        // rattaché par son nom ou son MBID.
        if v.fiche.is_some() && g.nom.is_empty() {
            g.nom = v.nom.trim().to_string();
        }
        if g.mbid.is_none() {
            g.mbid = mbid_utile(&v.mbid).map(str::to_string);
        }
        g.pistes.insert(v.track_id);
    }
    let mut out: Vec<Groupe> = ordre
        .into_iter()
        .filter_map(|c| groupes.remove(&c))
        .collect();
    trier(&mut out);
    out
}

fn trier(groupes: &mut [Groupe]) {
    groupes.sort_by(|a, b| {
        b.pistes
            .len()
            .cmp(&a.pistes.len())
            .then_with(|| a.nom.to_lowercase().cmp(&b.nom.to_lowercase()))
            .then_with(|| a.fiche.cmp(&b.fiche))
    });
}

fn lire_fiche(v: Option<&crate::db::backend::SqlValue>) -> Option<i64> {
    // `track_credits.artist_id` est du TEXT sur une base PostgreSQL migrée
    // depuis SQLite : `as_i64` lit l'entier comme le texte.
    v.and_then(|v| v.as_i64())
}

/// `played_with` d'un artiste : vide quand la fiche n'existe pas ou que
/// personne n'est audible avec lui.
pub fn joue_avec(backend: &Arc<dyn DbBackend>, artist_id: i64) -> Vec<JoueAvec> {
    let Ok(Some(fiche)) = backend.query_one(
        "SELECT name, musicbrainz_id FROM artists WHERE id = ?",
        &[&artist_id as &dyn ToSqlValue],
    ) else {
        return Vec::new();
    };
    let soi = Soi {
        artist_id,
        nom: fiche
            .first()
            .and_then(|v| v.as_string())
            .unwrap_or_default(),
        mbid: fiche.get(1).and_then(|v| v.as_string()),
    };
    let id_str = artist_id.to_string();
    let mbid_lie = soi.mbid.clone().unwrap_or_default();
    let audibles = liste_sql(ROLES_AUDIBLES);
    // Ses pistes audibles : même sous-requête que `credit_counts.performing`.
    let ses_pistes = format!(
        "SELECT id FROM tracks WHERE artist_id = ? \
         UNION SELECT track_id FROM track_credits WHERE id IN ({})",
        credits_de_l_artiste_sql(backend, &format!(" AND role IN ({audibles})"))
    );
    let masques = crate::db::facet_filter::hidden_albums_excluded();
    let params: [&dyn ToSqlValue; 5] = [
        &artist_id as &dyn ToSqlValue,
        &id_str as &dyn ToSqlValue,
        &soi.nom as &dyn ToSqlValue,
        &mbid_lie as &dyn ToSqlValue,
        &artist_id as &dyn ToSqlValue,
    ];
    // Deux requêtes plutôt qu'un `UNION ALL` : `track_credits.artist_id` peut
    // être du TEXT sur PostgreSQL, `tracks.artist_id` est un entier.
    let artistes_de_piste = format!(
        "SELECT t.id, t.artist_id, ar.name, ar.musicbrainz_id \
         FROM tracks t \
         JOIN albums a ON a.id = t.album_id \
         JOIN artists ar ON ar.id = t.artist_id \
         WHERE t.id IN ({ses_pistes}) AND t.artist_id <> ? AND {masques}"
    );
    let credits_audibles = format!(
        "SELECT tc.track_id, tc.artist_id, tc.artist_name, tc.artist_mbid \
         FROM track_credits tc \
         JOIN tracks t ON t.id = tc.track_id \
         JOIN albums a ON a.id = t.album_id \
         WHERE tc.track_id IN ({ses_pistes}) AND tc.role IN ({audibles}) \
           AND (tc.artist_id IS NULL OR {}) AND {masques}",
        match backend.engine() {
            Engine::Postgres => "CAST(tc.artist_id AS TEXT) <> ?",
            Engine::Sqlite => "tc.artist_id <> ?",
        }
    );
    let mut voisins: Vec<Voisin> = Vec::new();
    for (sql, params) in [
        (&artistes_de_piste, &params[..]),
        (
            &credits_audibles,
            &[
                params[0],
                params[1],
                params[2],
                params[3],
                &id_str as &dyn ToSqlValue,
            ][..],
        ),
    ] {
        let lignes = backend.query_many(sql, params).unwrap_or_else(|e| {
            warn!(artist_id, erreur = %e, "joue_avec_lecture_impossible");
            Vec::new()
        });
        for r in lignes {
            let Some(track_id) = r.first().and_then(|v| v.as_i64()) else {
                continue;
            };
            voisins.push(Voisin {
                track_id,
                fiche: lire_fiche(r.get(1)),
                nom: r.get(2).and_then(|v| v.as_string()).unwrap_or_default(),
                mbid: r.get(3).and_then(|v| v.as_string()),
            });
        }
    }

    let mut groupes = grouper(&voisins, &soi);
    // Les retenus sans fiche : la fiche de même MBID, sinon de même nom (sans
    // casse), quand elle existe en base. Au plus `TAILLE_JOUE_AVEC` lectures.
    let mut tete = groupes.len().min(TAILLE_JOUE_AVEC);
    let mut i = 0;
    while i < tete {
        if groupes[i].fiche.is_none()
            && let Some(id) = retrouver_fiche(backend, &groupes[i])
        {
            if id == artist_id {
                groupes.remove(i);
                tete = groupes.len().min(TAILLE_JOUE_AVEC);
                continue;
            }
            if let Some(j) = groupes.iter().position(|g| g.fiche == Some(id)) {
                let pistes = std::mem::take(&mut groupes[i].pistes);
                groupes[j].pistes.extend(pistes);
                groupes.remove(i);
                trier(&mut groupes);
                tete = groupes.len().min(TAILLE_JOUE_AVEC);
                i = 0;
                continue;
            }
            groupes[i].fiche = Some(id);
        }
        i += 1;
    }
    groupes.truncate(TAILLE_JOUE_AVEC);
    // Le nom d'une fiche est celui de la fiche.
    for g in groupes.iter_mut() {
        if let Some(id) = g.fiche
            && let Ok(Some(r)) = backend.query_one(
                "SELECT name FROM artists WHERE id = ?",
                &[&id as &dyn ToSqlValue],
            )
            && let Some(nom) = r.first().and_then(|v| v.as_string())
        {
            g.nom = nom;
        }
    }
    trier(&mut groupes);
    groupes
        .into_iter()
        .map(|g| JoueAvec {
            artist_id: g.fiche,
            artist_name: g.nom,
            shared_tracks: g.pistes.len() as i64,
        })
        .collect()
}

fn retrouver_fiche(backend: &Arc<dyn DbBackend>, g: &Groupe) -> Option<i64> {
    let lire = |sql: &str, cle: &str| -> Option<i64> {
        backend
            .query_one(sql, &[&cle as &dyn ToSqlValue])
            .ok()
            .flatten()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
    };
    if let Some(m) = mbid_utile(&g.mbid)
        && let Some(id) = lire("SELECT MIN(id) FROM artists WHERE musicbrainz_id = ?", m)
    {
        return Some(id);
    }
    if g.nom.trim().is_empty() {
        return None;
    }
    let sql = match backend.engine() {
        Engine::Sqlite => "SELECT MIN(id) FROM artists WHERE name = ? COLLATE NOCASE",
        Engine::Postgres => "SELECT MIN(id) FROM artists WHERE LOWER(name) = LOWER(?)",
    };
    lire(sql, g.nom.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(track_id: i64, fiche: Option<i64>, nom: &str) -> Voisin {
        Voisin {
            track_id,
            fiche,
            nom: nom.into(),
            mbid: None,
        }
    }

    fn soi() -> Soi {
        Soi {
            artist_id: 1,
            nom: "Ilse Brandt".into(),
            mbid: Some("mbid-ilse".into()),
        }
    }

    fn resume(g: &[Groupe]) -> Vec<(Option<i64>, String, usize)> {
        g.iter()
            .map(|g| (g.fiche, g.nom.clone(), g.pistes.len()))
            .collect()
    }

    #[test]
    fn ni_lui_meme_ni_various_artists_7768() {
        let mut lui_par_mbid = v(13, None, "I. Brandt");
        lui_par_mbid.mbid = Some("mbid-ilse".into());
        let voisins = vec![
            v(10, Some(1), "Ilse Brandt"),
            v(11, None, "ilse  BRANDT"),
            v(12, None, "Ilsé Brandt"),
            lui_par_mbid,
            v(14, Some(3), "Various Artists"),
            v(15, None, "Various"),
            v(16, Some(2), "Nora Vidal"),
        ];
        assert_eq!(
            resume(&grouper(&voisins, &soi())),
            vec![(Some(2), "Nora Vidal".into(), 1)]
        );
    }

    #[test]
    fn pistes_distinctes_et_ordre_par_pistes_partagees_puis_nom_7768() {
        let voisins = vec![
            v(1, None, "Tomas Reyes"),
            v(1, Some(5), "Zoe"),
            v(2, Some(5), "Zoe"),
            // Doublon de source (#7766) : la piste 2 ne compte qu'une fois.
            v(2, Some(5), "Zoe"),
            v(1, Some(4), "anna"),
            v(2, Some(4), "anna"),
            v(3, None, "Bob"),
        ];
        assert_eq!(
            resume(&grouper(&voisins, &soi())),
            vec![
                (Some(4), "anna".into(), 2),
                (Some(5), "Zoe".into(), 2),
                (None, "Bob".into(), 1),
                (None, "Tomas Reyes".into(), 1),
            ]
        );
    }

    #[test]
    fn un_credit_sans_fiche_rejoint_la_fiche_de_meme_nom_ou_mbid_7768() {
        let mut par_mbid = v(3, None, "S. Okafor");
        par_mbid.mbid = Some("mbid-sam".into());
        let mut fiche = v(1, Some(403), "Sam Okafor");
        fiche.mbid = Some("mbid-sam".into());
        let voisins = vec![v(2, None, "sam okafor"), fiche, par_mbid];
        assert_eq!(
            resume(&grouper(&voisins, &soi())),
            vec![(Some(403), "sam okafor".into(), 3)]
        );
    }
}
