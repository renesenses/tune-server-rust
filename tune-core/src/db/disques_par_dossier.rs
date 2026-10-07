//! Le numéro de disque DÉDUIT DU DOSSIER, pour un album réparti en dossiers
//! frères aux noms libres et dont aucune piste ne porte de balise DISCNUMBER.
//!
//! # Le défaut
//!
//! ```text
//! racine/Artiste/Album/Le jour/01 - ….flac   ┐ même balise ALBUM,
//! racine/Artiste/Album/La nuit/01 - ….flac   ┘ aucune balise DISCNUMBER
//! ```
//!
//! Les deux dossiers ne sont pas des noms de disque (`CD1`, `Disc 2`, suffixe
//! numérique) : le scan en fait deux albums, puis la fusion des doublons de
//! fin de scan ([`super::album_doublons`], même titre, même artiste) les
//! réunit en un seul. Le scan range une piste sans DISCNUMBER au disque 1 :
//! l'album fusionné montrait donc toutes ses pistes au disque 1, avec deux
//! pistes nº 1, deux pistes nº 2…
//!
//! # La règle
//!
//! Un numéro de disque est déduit par dossier, dans l'ordre NATUREL des noms
//! de dossier (`Partie 2` avant `Partie 10`), quand TOUT ceci tient
//! ([`deduire`]) :
//!
//! 1. chaque piste est au disque 1, ou DÉJÀ au disque que son dossier
//!    donne — une balise DISCNUMBER, un dossier `CD2` ou un coffret réuni qui
//!    disent autre chose ont parlé, et l'album est laissé ;
//! 2. ses pistes vivent dans au moins deux dossiers, FRÈRES sous un même
//!    parent qui n'est pas la racine du système de fichiers ;
//! 3. dans chaque dossier, les numéros de piste sont uniques ;
//! 4. au moins un numéro de piste se répète d'un dossier à l'autre — sans
//!    répétition, la numérotation est déjà continue (compilation éclatée) ;
//! 5. aucun dossier ne répète le même titre au même numéro qu'un autre : ce
//!    seraient deux COPIES du même disque, pas deux disques.
//!
//! Sont laissés tels quels : un album marqué coffret, un album dont les
//! disques ont été disposés à la main (écran « Modifier »), un album dont une
//! piste a son numéro de disque tenu à la main ([`super::champs_tenus`]).
//!
//! Rien n'est écrit dans les FICHIERS : seul `tracks.disc_number` (et
//! `albums.disc_count`) change, en base. La passe est idempotente — l'album
//! traité n'a plus de couple (disque, numéro) en double et n'est plus examiné
//! — et stable : un fichier relu au disque 1 par un scan suivant est rangé de
//! nouveau au même disque, puisque l'ordre ne dépend que des noms de dossier.
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

use super::backend::{DbBackend, ToSqlValue};
use super::engine::{Engine, PostgresDialect, SqlDialect, SqliteDialect};
use super::track_repo::sql::chemin_ouvrable;
use crate::TuneError;
use crate::library::local_path::{dossier_comparable, dossier_et_nom};

/// Une piste, réduite à ce que la règle regarde.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PisteADisposer {
    pub id: i64,
    /// Le fichier, ou l'image d'une piste CUE.
    pub chemin: String,
    /// `tracks.disc_number`, NULL lu comme 1.
    pub disque: i32,
    /// `tracks.track_number`, NULL lu comme 0.
    pub numero: i32,
    pub titre: String,
}

/// Compare deux noms dans l'ordre naturel : les suites de chiffres comme des
/// nombres, le reste sans casse ni accents. À égalité, l'ordre des octets
/// départage — le résultat ne dépend jamais de l'ordre de lecture.
pub fn ordre_naturel(a: &str, b: &str) -> Ordering {
    fn morceaux(s: &str) -> Vec<(bool, String)> {
        let mut v: Vec<(bool, String)> = Vec::new();
        for c in s.chars() {
            let chiffre = c.is_ascii_digit();
            match v.last_mut() {
                Some((d, m)) if *d == chiffre => m.push(c),
                _ => v.push((chiffre, c.to_string())),
            }
        }
        v
    }
    let (ma, mb) = (morceaux(a), morceaux(b));
    for ((da, xa), (db, xb)) in ma.iter().zip(mb.iter()) {
        let o = match (da, db) {
            (true, true) => {
                let (na, nb) = (xa.trim_start_matches('0'), xb.trim_start_matches('0'));
                na.len().cmp(&nb.len()).then_with(|| na.cmp(nb))
            }
            _ => {
                let cle = |s: &str| crate::db::engine::fold_diacritics(s).to_lowercase();
                cle(xa).cmp(&cle(xb))
            }
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    ma.len().cmp(&mb.len()).then_with(|| a.cmp(b))
}

fn titre_normalise(t: &str) -> String {
    crate::db::engine::fold_diacritics(t)
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Le numéro de disque de chaque piste, déduit de son dossier — ou `None`
/// quand la règle du module ne tient pas. Rend `(id de piste, disque)` pour
/// TOUTES les pistes de l'album.
pub fn deduire(pistes: &[PisteADisposer]) -> Option<Vec<(i64, i32)>> {
    if pistes.is_empty() {
        return None;
    }
    // 2. Plusieurs dossiers, frères.
    let mut par_dossier: BTreeMap<String, (String, Vec<&PisteADisposer>)> = BTreeMap::new();
    let mut parents: HashSet<String> = HashSet::new();
    for p in pistes {
        let dossier = dossier_et_nom(&p.chemin)?.0;
        let (parent, feuille) = dossier_et_nom(dossier)?;
        if feuille.is_empty() || parent.is_empty() || dossier_et_nom(parent).is_none() {
            return None;
        }
        parents.insert(dossier_comparable(parent).into_owned());
        par_dossier
            .entry(dossier_comparable(dossier).into_owned())
            .or_insert_with(|| (feuille.to_string(), Vec::new()))
            .1
            .push(p);
    }
    if par_dossier.len() < 2 || parents.len() != 1 {
        return None;
    }
    // 3. Numéros uniques dans chaque dossier ; 4. répétés d'un dossier à
    // l'autre ; 5. pas deux fois le même titre au même numéro.
    let mut dossiers_du_numero: HashMap<i32, usize> = HashMap::new();
    let mut titres_vus: HashSet<(i32, String)> = HashSet::new();
    for (_, pistes_du_dossier) in par_dossier.values() {
        let mut numeros = HashSet::new();
        for p in pistes_du_dossier.iter().filter(|p| p.numero > 0) {
            if !numeros.insert(p.numero) {
                return None;
            }
            let titre = titre_normalise(&p.titre);
            if !titre.is_empty() && !titres_vus.insert((p.numero, titre)) {
                return None;
            }
        }
        for n in numeros {
            *dossiers_du_numero.entry(n).or_default() += 1;
        }
    }
    if !dossiers_du_numero.values().any(|&k| k > 1) {
        return None;
    }
    let mut ordre: Vec<(&String, &(String, Vec<&PisteADisposer>))> = par_dossier.iter().collect();
    ordre.sort_by(|(ca, (fa, _)), (cb, (fb, _))| ordre_naturel(fa, fb).then_with(|| ca.cmp(cb)));
    let mut rendu = Vec::with_capacity(pistes.len());
    for (i, (_, (_, pistes_du_dossier))) in ordre.iter().enumerate() {
        let disque = i as i32 + 1;
        for p in pistes_du_dossier {
            // 1. Un autre disque que celui du dossier : quelqu'un a parlé.
            if p.disque > 1 && p.disque != disque {
                return None;
            }
            rendu.push((p.id, disque));
        }
    }
    Some(rendu)
}

/// Le bilan d'une passe.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bilan {
    /// Albums examinés (couple disque + numéro de piste en double).
    pub examines: usize,
    /// Albums dont les disques ont été déduits.
    pub albums: usize,
    /// Pistes dont le numéro de disque a changé.
    pub pistes: usize,
}

fn marque(db: &Arc<dyn DbBackend>, n: usize) -> String {
    match db.engine() {
        Engine::Sqlite => SqliteDialect.placeholder(n),
        Engine::Postgres => PostgresDialect.placeholder(n),
    }
}

/// Les albums locaux où un même couple (disque, numéro de piste) se répète,
/// hors coffrets et dispositions tenues à la main.
fn sql_candidats() -> String {
    format!(
        "SELECT t.album_id FROM tracks t \
         WHERE {piste_locale} AND t.album_id IS NOT NULL AND {c} IS NOT NULL \
         AND COALESCE(t.track_number, 0) > 0 \
         AND NOT EXISTS (SELECT 1 FROM album_metadata m WHERE m.album_id = t.album_id \
             AND m.key IN ('{coffret}', '{edition}')) \
         GROUP BY t.album_id \
         HAVING COUNT(*) > \
             COUNT(DISTINCT COALESCE(t.disc_number, 1) * 100000 + t.track_number) \
         ORDER BY t.album_id",
        piste_locale = crate::db::track_repo::sql::PISTE_LOCALE,
        c = chemin_ouvrable!(),
        coffret = super::coffrets_auto::CLE_COFFRET,
        edition = super::edition_album::CLE_EDITION_PISTES,
    )
}

fn pistes_de(db: &Arc<dyn DbBackend>, album_id: i64) -> Result<Vec<PisteADisposer>, TuneError> {
    let sql = format!(
        "SELECT t.id, {c}, COALESCE(t.disc_number, 1), COALESCE(t.track_number, 0), t.title \
         FROM tracks t WHERE t.album_id = {p1} ORDER BY t.id",
        c = chemin_ouvrable!(),
        p1 = marque(db, 1),
    );
    let mut v = Vec::new();
    for r in db.query_many(&sql, &[&album_id as &dyn ToSqlValue])? {
        let (Some(id), Some(chemin)) = (
            r.first().and_then(|x| x.as_i64()),
            r.get(1).and_then(|x| x.as_string()),
        ) else {
            // Une piste sans fichier (distante, fantôme) : on ne sait pas
            // où elle range, l'album est laissé tel quel.
            return Ok(Vec::new());
        };
        v.push(PisteADisposer {
            id,
            chemin,
            disque: r.get(2).and_then(|x| x.as_i64()).unwrap_or(1) as i32,
            numero: r.get(3).and_then(|x| x.as_i64()).unwrap_or(0) as i32,
            titre: r.get(4).and_then(|x| x.as_string()).unwrap_or_default(),
        });
    }
    Ok(v)
}

/// Une piste dont le numéro de disque est tenu à la main.
fn disque_tenu(db: &Arc<dyn DbBackend>, track_ids: &[i64]) -> Result<bool, TuneError> {
    let sql = format!(
        "SELECT value FROM track_metadata WHERE track_id = {} AND key = {}",
        marque(db, 1),
        marque(db, 2)
    );
    for id in track_ids {
        let cle = super::champs_tenus::CLE;
        if let Some(r) = db.query_one(&sql, &[id as &dyn ToSqlValue, &cle])?
            && let Some(v) = r.first().and_then(|x| x.as_string())
            && serde_json::from_str::<super::champs_tenus::ChampsTenus>(&v)
                .is_ok_and(|t| t.disc_number.is_some())
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Déduit les disques de tous les albums qui répondent à la règle du module.
pub fn passe(db: &Arc<dyn DbBackend>) -> Result<Bilan, TuneError> {
    let mut bilan = Bilan::default();
    let candidats: Vec<i64> = db
        .query_many(&sql_candidats(), &[])?
        .into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect();
    let (p1, p2, p3) = (marque(db, 1), marque(db, 2), marque(db, 3));
    let maj_piste = format!(
        "UPDATE tracks SET disc_number = {p1} WHERE id = {p2} \
         AND COALESCE(disc_number, 1) <= 1"
    );
    let maj_album = format!(
        "UPDATE albums SET disc_count = {p1} WHERE id = {p2} \
         AND COALESCE(disc_count, 1) < {p3}"
    );
    for album_id in candidats {
        bilan.examines += 1;
        let pistes = pistes_de(db, album_id)?;
        let Some(disques) = deduire(&pistes) else {
            continue;
        };
        let ids: Vec<i64> = pistes.iter().map(|p| p.id).collect();
        if disque_tenu(db, &ids)? {
            continue;
        }
        let avant: HashMap<i64, i32> = pistes.iter().map(|p| (p.id, p.disque)).collect();
        let nombre = disques.iter().map(|(_, d)| *d).max().unwrap_or(1) as i64;
        let mut changees = 0usize;
        db.write_tx(&mut |tx| {
            changees = 0;
            for &(id, d) in &disques {
                if avant.get(&id) == Some(&d) {
                    continue;
                }
                let d = d as i64;
                changees += tx.execute(&maj_piste, &[&d as &dyn ToSqlValue, &id])?;
            }
            tx.execute(
                &maj_album,
                &[&nombre as &dyn ToSqlValue, &album_id, &nombre],
            )?;
            Ok(())
        })?;
        if changees > 0 {
            bilan.albums += 1;
            bilan.pistes += changees;
            tracing::info!(
                album_id,
                disques = nombre,
                pistes = changees,
                "disques_deduits_des_dossiers — album sans DISCNUMBER réparti en dossiers \
                 frères : un disque par dossier, en base seulement"
            );
        }
    }
    Ok(bilan)
}

/// [`passe`], dont l'erreur est journalisée plutôt que rendue : un scan ou un
/// nettoyage ne s'arrête pas sur elle.
pub fn passe_journalisee(db: &Arc<dyn DbBackend>, moment: &str) -> Bilan {
    match passe(db) {
        Ok(b) => {
            if b.albums > 0 {
                tracing::info!(
                    moment,
                    examines = b.examines,
                    albums = b.albums,
                    pistes = b.pistes,
                    "disques_deduits_des_dossiers_bilan"
                );
            }
            b
        }
        Err(e) => {
            tracing::warn!(moment, erreur = %e, "disques_deduits_des_dossiers_echec");
            Bilan::default()
        }
    }
}

#[cfg(test)]
#[path = "disques_par_dossier_tests.rs"]
pub(crate) mod tests;
