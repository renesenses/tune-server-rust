//! #8077 — le stockage des apports d'un service (Qobuz) à un album LOCAL :
//! `apports_service_albums` (le rapprochement) et `apports_service` (les
//! champs repris). Migrations SQLite 141 / PostgreSQL 107.
//!
//! Rien n'est écrit dans les fichiers ni dans les colonnes de l'album : la
//! fiche lit ces tables à côté des siennes. Les crédits repris vont dans
//! `track_credits` (`source = 'qobuz'`), par
//! [`crate::metadata::credits_release::inserer_credits_piste`].
//!
//! Le retrait ([`retirer`]) efface tous les champs et tous les crédits de la
//! source pour l'album, et garde le rapprochement au statut `retire` : la
//! passe de fond ne remet pas ce que l'utilisateur a retiré.

use std::sync::Arc;

use serde_json::Value;

use super::backend::{DbBackend, SqlValue, ToSqlValue};
use crate::library::apports_service::{Apports, Statut};

/// Un rapprochement rangé.
#[derive(Debug, Clone, PartialEq)]
pub struct Rapprochement {
    pub album_id: i64,
    pub source: String,
    pub service_album_id: Option<String>,
    pub methode: Option<String>,
    pub statut: Statut,
    pub motif: Option<String>,
    pub pistes_locales: i64,
    pub pistes_rapprochees: i64,
    pub verifie_le: String,
}

/// Un champ rangé : `track_id = 0` pour l'album.
#[derive(Debug, Clone, PartialEq)]
pub struct ChampRange {
    pub track_id: i64,
    pub champ: String,
    pub valeur: Value,
}

pub fn maintenant() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn texte(v: Option<&SqlValue>) -> Option<String> {
    v.and_then(SqlValue::as_string)
}

pub fn lire_rapprochement(
    b: &Arc<dyn DbBackend>,
    album_id: i64,
    source: &str,
) -> Result<Option<Rapprochement>, String> {
    let Some(r) = b.query_one(
        "SELECT service_album_id, methode, statut, motif, pistes_locales, pistes_rapprochees, verifie_le \
         FROM apports_service_albums WHERE album_id = ? AND source = ?",
        &[&album_id as &dyn ToSqlValue, &source as &dyn ToSqlValue],
    )?
    else {
        return Ok(None);
    };
    let Some(statut) = texte(r.get(2)).as_deref().and_then(Statut::depuis) else {
        return Ok(None);
    };
    Ok(Some(Rapprochement {
        album_id,
        source: source.to_string(),
        service_album_id: texte(r.first()),
        methode: texte(r.get(1)),
        statut,
        motif: texte(r.get(3)),
        pistes_locales: r.get(4).and_then(SqlValue::as_i64).unwrap_or(0),
        pistes_rapprochees: r.get(5).and_then(SqlValue::as_i64).unwrap_or(0),
        verifie_le: texte(r.get(6)).unwrap_or_default(),
    }))
}

const UPSERT_RAPPROCHEMENT: &str = "INSERT INTO apports_service_albums \
     (album_id, source, service_album_id, methode, statut, motif, pistes_locales, pistes_rapprochees, verifie_le) \
     VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
     ON CONFLICT (album_id, source) DO UPDATE SET service_album_id = excluded.service_album_id, \
     methode = excluded.methode, statut = excluded.statut, motif = excluded.motif, \
     pistes_locales = excluded.pistes_locales, pistes_rapprochees = excluded.pistes_rapprochees, \
     verifie_le = excluded.verifie_le";

/// L'écriture du rapprochement, par `execute` d'un backend ou d'une
/// transaction : un seul texte de paramètres pour les deux.
fn upsert_rapprochement(
    execute: &dyn Fn(&str, &[&dyn ToSqlValue]) -> Result<usize, String>,
    r: &Rapprochement,
) -> Result<(), String> {
    let statut = r.statut.as_str();
    execute(
        UPSERT_RAPPROCHEMENT,
        &[
            &r.album_id as &dyn ToSqlValue,
            &r.source.as_str(),
            &r.service_album_id,
            &r.methode,
            &statut,
            &r.motif,
            &r.pistes_locales,
            &r.pistes_rapprochees,
            &r.verifie_le,
        ],
    )
    .map(|_| ())
}

/// Range (ou remplace) le rapprochement, SANS toucher aux apports.
pub fn ranger_rapprochement(b: &Arc<dyn DbBackend>, r: &Rapprochement) -> Result<(), String> {
    upsert_rapprochement(&|sql, p| b.execute(sql, p), r)
}

/// Efface les crédits de `source` des pistes de l'album.
fn effacer_credits(b: &Arc<dyn DbBackend>, source: &str, pistes: &[i64]) -> Result<usize, String> {
    let mut n = 0;
    for id in pistes {
        // L'entier, comme `ecrire_credits_piste` (#4984).
        n += b.execute(
            "DELETE FROM track_credits WHERE track_id = ? AND source = ?",
            &[id as &dyn ToSqlValue, &source as &dyn ToSqlValue],
        )?;
    }
    Ok(n)
}

/// Range un rapprochement SÛR et ses apports, en remplaçant ceux de la même
/// source pour cet album. `pistes` : toutes les pistes de l'album.
pub fn ranger_apports(
    b: &Arc<dyn DbBackend>,
    r: &Rapprochement,
    apports: &Apports,
    pistes: &[i64],
) -> Result<(), String> {
    let cree_le = maintenant();
    b.write_tx(&mut |tx| {
        tx.execute(
            "DELETE FROM apports_service WHERE album_id = ? AND source = ?",
            &[&r.album_id as &dyn ToSqlValue, &r.source.as_str()],
        )?;
        for a in &apports.champs {
            let valeur = a.valeur.to_string();
            tx.execute(
                "INSERT INTO apports_service (album_id, track_id, champ, source, valeur, cree_le) \
                 VALUES (?, ?, ?, ?, ?, ?)",
                &[
                    &r.album_id as &dyn ToSqlValue,
                    &a.track_id,
                    &a.champ,
                    &r.source.as_str(),
                    &valeur,
                    &cree_le,
                ],
            )?;
        }
        upsert_rapprochement(&|sql, p| tx.execute(sql, p), r)
    })?;
    effacer_credits(b, &r.source, pistes)?;
    for (track_id, lignes) in &apports.credits {
        crate::metadata::credits_release::inserer_credits_piste(b, *track_id, &r.source, lignes);
    }
    Ok(())
}

/// Ce que [`retirer`] a effacé.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Retrait {
    pub champs: usize,
    pub credits: usize,
}

/// Retire TOUS les apports de `source` pour l'album (champs et crédits), et
/// marque le rapprochement `retire`. Sans rapprochement rangé, il en crée
/// un : un album retiré avant toute recherche n'est pas recherché ensuite.
pub fn retirer(
    b: &Arc<dyn DbBackend>,
    album_id: i64,
    source: &str,
    pistes: &[i64],
) -> Result<Retrait, String> {
    let champs = b.execute(
        "DELETE FROM apports_service WHERE album_id = ? AND source = ?",
        &[&album_id as &dyn ToSqlValue, &source as &dyn ToSqlValue],
    )?;
    let credits = effacer_credits(b, source, pistes)?;
    let mut r = lire_rapprochement(b, album_id, source)?.unwrap_or(Rapprochement {
        album_id,
        source: source.to_string(),
        service_album_id: None,
        methode: None,
        statut: Statut::Retire,
        motif: None,
        pistes_locales: pistes.len() as i64,
        pistes_rapprochees: 0,
        verifie_le: String::new(),
    });
    r.statut = Statut::Retire;
    r.motif = Some("retire_par_l_utilisateur".into());
    r.verifie_le = maintenant();
    ranger_rapprochement(b, &r)?;
    Ok(Retrait { champs, credits })
}

/// Les champs rangés pour l'album (et ses pistes), dans l'ordre d'écriture.
pub fn champs(
    b: &Arc<dyn DbBackend>,
    album_id: i64,
    source: &str,
) -> Result<Vec<ChampRange>, String> {
    Ok(b.query_many(
        "SELECT track_id, champ, valeur FROM apports_service \
             WHERE album_id = ? AND source = ? ORDER BY track_id, champ",
        &[&album_id as &dyn ToSqlValue, &source as &dyn ToSqlValue],
    )?
    .into_iter()
    .filter_map(|r| {
        Some(ChampRange {
            track_id: r.first()?.as_i64()?,
            champ: texte(r.get(1))?,
            valeur: serde_json::from_str(&texte(r.get(2))?).ok()?,
        })
    })
    .collect())
}

/// Les albums locaux jamais examinés pour `source`, du plus ancien id au
/// plus récent : la file de la passe de fond.
pub fn albums_a_examiner(
    b: &Arc<dyn DbBackend>,
    source: &str,
    limite: i64,
) -> Result<Vec<i64>, String> {
    Ok(b.query_many(
        "SELECT a.id FROM albums a WHERE (a.source IS NULL OR a.source = 'local') \
             AND NOT EXISTS (SELECT 1 FROM apports_service_albums r \
                 WHERE r.album_id = a.id AND r.source = ?) \
             ORDER BY a.id LIMIT ?",
        &[&source as &dyn ToSqlValue, &limite as &dyn ToSqlValue],
    )?
    .into_iter()
    .filter_map(|r| r.first()?.as_i64())
    .collect())
}
