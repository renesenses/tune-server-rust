//! Le MBID des artistes, tiré du pressage identifié (#4805, étape B).
//!
//! Quand un album est identifié, la réponse `/release/{id}` déjà reçue
//! (`inc=recordings+artist-credits+labels`) porte les `artist-credit` du
//! pressage et de chaque piste, **avec le MBID de chaque artiste**. Ce module
//! les rattache aux fiches `artists` de la bibliothèque, sans aucune requête
//! de plus.
//!
//! # Règles
//!
//! - **Par rôle.** L'artiste de l'album (`albums.artist_id`) se compare aux
//!   crédits du pressage ; l'artiste d'une piste (`tracks.artist_id`), aux
//!   crédits de LA piste du pressage qui lui a été appariée
//!   ([`super::reidentify::map_recording_ids`]). Une piste non appariée
//!   n'apporte rien.
//! - **Par nom normalisé** : [`crate::db::artist_repo::cle_artiste`], la clé
//!   qui sert déjà à dédoublonner les fiches (accents, casse, « The »,
//!   ponctuation). Elle est comparée au nom crédité, au nom de la fiche
//!   MusicBrainz et à son nom de tri.
//! - **Jamais d'écrasement.** Une fiche qui porte déjà un MBID le garde, même
//!   différent ; le désaccord est compté. La garde est aussi dans l'`UPDATE`.
//! - **Rien sur une ambiguïté**, comptée : deux crédits homonymes de MBID
//!   différents, une fiche à qui deux crédits proposent deux MBID, deux fiches
//!   à qui l'album propose le même MBID, ou un MBID déjà porté par une autre
//!   fiche de la base.
//! - Les pseudo-artistes de MusicBrainz (`Various Artists`, `[unknown]`…) et
//!   les artistes fictifs locaux ne sont jamais rattachés.
//!
//! Aucune écriture dans les fichiers audio : seule la colonne
//! `artists.musicbrainz_id` bouge, et seulement là où elle était vide.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use crate::db::artist_repo::cle_artiste;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::metadata::musicbrainz_release::{CreditArtiste, MBReleaseDetail, est_un_artiste_fictif};

/// Les artistes « à usage spécial » de MusicBrainz : ce ne sont pas des
/// personnes ni des groupes, et poser leur MBID sur une fiche locale
/// l'enverrait chercher la photo et la biographie de « Various Artists ».
pub const MBID_PSEUDO_ARTISTES: &[&str] = &[
    "89ad4ac3-39f7-470e-963a-56509c546377", // Various Artists
    "125ec42a-7229-4250-afc5-e057484327fe", // [unknown]
    "f731ccc4-e22a-43af-a747-64213329e088", // [anonymous]
    "33cf029c-63b0-41a0-9855-be2a3665fb3b", // [data]
    "314e1c25-dde7-4e4d-b2f4-0a7b9f7c56dc", // [dialogue]
    "eec63d3c-3b81-4ad4-b1e4-7c147d4d2b61", // [no artist]
    "9be7f096-97ec-4615-8957-8d40b5dcbc41", // [traditional]
    "66ea0139-149f-4a0c-8fbf-5ea9ec4a6e49", // Disney
];

/// Le rôle sous lequel une fiche locale est confrontée aux crédits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// `albums.artist_id`, face aux crédits du pressage.
    Album,
    /// `tracks.artist_id`, face aux crédits de la piste appariée.
    Piste,
}

/// Une fiche locale, sous un rôle, face à une liste de crédits.
#[derive(Debug, Clone)]
pub struct Confrontation<'a> {
    pub artiste_id: i64,
    pub nom: &'a str,
    /// `artists.musicbrainz_id` actuel, `None` si vide.
    pub mbid_actuel: Option<&'a str>,
    pub role: Role,
    pub credits: &'a [CreditArtiste],
}

/// Ce que le rattachement a décidé, fiche par fiche.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BilanArtistes {
    /// `(artiste_id, mbid)` à poser.
    pub a_poser: Vec<(i64, String)>,
    /// Fiches déjà munies du MBID que le pressage propose : confirmées.
    pub deja_poses: usize,
    /// Fiches munies d'un AUTRE MBID que celui proposé : laissées telles
    /// quelles, comptées.
    pub desaccords: usize,
    /// Fiches pour lesquelles le pressage est ambigu : rien n'est écrit.
    pub ambigus: usize,
    /// Fiches qu'aucun crédit du pressage ne nomme.
    pub sans_correspondance: usize,
    /// Fiches écartées d'office : artiste fictif local (`Unknown Artist`…),
    /// ou seul un pseudo-artiste MusicBrainz les nomme.
    pub ecartes: usize,
    /// Fiches dont le MBID est effectivement écrit en base
    /// ([`poser_les_mbid`]). Plus petit que `a_poser` quand la base refuse :
    /// MBID déjà porté par une autre fiche, ou fiche remplie entre-temps.
    pub ecrits: usize,
    /// `a_poser` que la base a refusés (voir `ecrits`).
    pub refuses_par_la_base: usize,
}

fn est_pseudo(mbid: &str) -> bool {
    MBID_PSEUDO_ARTISTES.contains(&mbid.trim())
}

/// Les MBID distincts des crédits qui nomment `cle`.
fn mbid_nommant(cle: &str, credits: &[CreditArtiste]) -> (BTreeSet<String>, bool) {
    let mut trouves = BTreeSet::new();
    let mut pseudo = false;
    for c in credits {
        let nomme = [&c.nom_credite, &c.nom, &c.nom_de_tri]
            .iter()
            .any(|n| !n.trim().is_empty() && cle_artiste(n) == cle);
        if !nomme {
            continue;
        }
        if est_pseudo(&c.mbid) {
            pseudo = true;
        } else {
            trouves.insert(c.mbid.trim().to_lowercase());
        }
    }
    (trouves, pseudo)
}

/// Décide, sans toucher à la base, quel MBID poser sur quelle fiche.
///
/// Une fiche peut apparaître plusieurs fois (artiste de l'album ET de ses
/// pistes, ou de plusieurs pistes) : toutes ses confrontations doivent
/// désigner le MÊME MBID, sinon c'est une ambiguïté.
pub fn rattacher(confrontations: &[Confrontation<'_>]) -> BilanArtistes {
    #[derive(Default)]
    struct Etat {
        mbid_actuel: Option<String>,
        proposes: BTreeSet<String>,
        homonymes: bool,
        fictif: bool,
        pseudo: bool,
    }
    let mut par_fiche: BTreeMap<i64, Etat> = BTreeMap::new();
    for c in confrontations {
        let e = par_fiche.entry(c.artiste_id).or_default();
        e.mbid_actuel = c
            .mbid_actuel
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_lowercase);
        let cle = cle_artiste(c.nom);
        if cle.is_empty() || est_un_artiste_fictif(c.nom) {
            e.fictif = true;
            continue;
        }
        let (trouves, pseudo) = mbid_nommant(&cle, c.credits);
        e.pseudo |= pseudo;
        if trouves.len() > 1 {
            // Deux crédits du même nom, deux MBID : des homonymes.
            e.homonymes = true;
        }
        e.proposes.extend(trouves);
    }

    // Un même MBID proposé à deux fiches de l'album : on ne sait pas laquelle
    // est la bonne (ou ce sont des doublons à fusionner, ce qui n'est pas le
    // rôle de cette passe).
    let mut fiches_par_mbid: BTreeMap<&str, usize> = BTreeMap::new();
    for e in par_fiche.values() {
        if !e.fictif && !e.homonymes && e.proposes.len() == 1 {
            let m = e.proposes.iter().next().expect("un MBID");
            *fiches_par_mbid.entry(m.as_str()).or_default() += 1;
        }
    }

    let mut bilan = BilanArtistes::default();
    for (&id, e) in &par_fiche {
        if e.fictif {
            bilan.ecartes += 1;
            continue;
        }
        if e.homonymes || e.proposes.len() > 1 {
            bilan.ambigus += 1;
            continue;
        }
        let Some(mbid) = e.proposes.iter().next() else {
            if e.pseudo {
                bilan.ecartes += 1;
            } else {
                bilan.sans_correspondance += 1;
            }
            continue;
        };
        match e.mbid_actuel.as_deref() {
            Some(actuel) if actuel == mbid => bilan.deja_poses += 1,
            Some(_) => bilan.desaccords += 1,
            None if fiches_par_mbid.get(mbid.as_str()).copied().unwrap_or(0) > 1 => {
                bilan.ambigus += 1
            }
            None => bilan.a_poser.push((id, mbid.clone())),
        }
    }
    bilan
}

/// Écrit les MBID décidés par [`rattacher`].
///
/// Un `UPDATE` par fiche, gardé deux fois **dans la requête elle-même** :
/// la colonne doit être vide (rien n'est écrasé, même si une autre passe l'a
/// remplie entre-temps), et aucune AUTRE fiche ne doit déjà porter ce MBID.
/// Rend le nombre de fiches effectivement écrites.
pub fn poser_les_mbid(
    backend: &Arc<dyn DbBackend>,
    a_poser: &[(i64, String)],
) -> Result<usize, String> {
    let mut ecrits = 0;
    for (id, mbid) in a_poser {
        let valeur: Option<String> = Some(mbid.clone());
        let n = backend.execute(
            "UPDATE artists SET musicbrainz_id = ? \
             WHERE id = ? AND TRIM(COALESCE(musicbrainz_id, '')) = '' \
             AND NOT EXISTS (SELECT 1 FROM artists autre \
                             WHERE autre.id <> ? AND LOWER(TRIM(autre.musicbrainz_id)) = ?)",
            &[
                &valeur as &dyn ToSqlValue,
                id as &dyn ToSqlValue,
                id as &dyn ToSqlValue,
                &valeur as &dyn ToSqlValue,
            ],
        )?;
        ecrits += n;
    }
    Ok(ecrits)
}

/// Lit une fiche : `(nom, mbid)`.
fn fiche(
    backend: &Arc<dyn DbBackend>,
    id: i64,
) -> Result<Option<(String, Option<String>)>, String> {
    let ligne = backend.query_one(
        "SELECT name, musicbrainz_id FROM artists WHERE id = ?",
        &[&id as &dyn ToSqlValue],
    )?;
    Ok(ligne.map(|l| {
        let nom = l
            .first()
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let mbid = l
            .get(1)
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        (nom, mbid)
    }))
}

/// La passe complète pour UN album identifié : relève l'artiste de l'album et
/// ceux de ses pistes appariées, décide, écrit.
///
/// `recordings` est l'appariement `(track_id, recording_id)` qu'a produit
/// [`super::reidentify::map_recording_ids`] et qu'a posé
/// [`super::reidentify::apply_album_identification`] : la piste du pressage
/// d'une piste locale se retrouve par son enregistrement.
pub fn rattacher_les_artistes_de_l_album(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    detail: &MBReleaseDetail,
    recordings: &[(i64, String)],
) -> Result<BilanArtistes, String> {
    // (artiste_id, rôle, crédits) avant lecture des fiches.
    let mut a_confronter: Vec<(i64, Role, &[CreditArtiste])> = Vec::new();

    let album = backend.query_one(
        "SELECT artist_id FROM albums WHERE id = ?",
        &[&album_id as &dyn ToSqlValue],
    )?;
    if let Some(aid) = album.and_then(|l| l.first().and_then(|v| v.as_i64())) {
        a_confronter.push((aid, Role::Album, &detail.artist_credits));
    }

    let pistes = backend.query_many(
        "SELECT id, artist_id FROM tracks WHERE album_id = ? ORDER BY id",
        &[&album_id as &dyn ToSqlValue],
    )?;
    for ligne in &pistes {
        let (Some(tid), Some(aid)) = (
            ligne.first().and_then(|v| v.as_i64()),
            ligne.get(1).and_then(|v| v.as_i64()),
        ) else {
            continue;
        };
        let Some((_, rid)) = recordings.iter().find(|(t, _)| *t == tid) else {
            continue;
        };
        let Some(piste_mb) = detail
            .tracks
            .iter()
            .find(|t| t.recording_id.as_deref() == Some(rid.as_str()))
        else {
            continue;
        };
        a_confronter.push((aid, Role::Piste, &piste_mb.artist_credits));
    }

    let mut fiches: BTreeMap<i64, (String, Option<String>)> = BTreeMap::new();
    for (aid, _, _) in &a_confronter {
        if !fiches.contains_key(aid)
            && let Some(f) = fiche(backend, *aid)?
        {
            fiches.insert(*aid, f);
        }
    }

    let confrontations: Vec<Confrontation<'_>> = a_confronter
        .iter()
        .filter_map(|(aid, role, credits)| {
            let (nom, mbid) = fiches.get(aid)?;
            Some(Confrontation {
                artiste_id: *aid,
                nom,
                mbid_actuel: mbid.as_deref(),
                role: *role,
                credits,
            })
        })
        .collect();

    let mut bilan = rattacher(&confrontations);
    bilan.ecrits = poser_les_mbid(backend, &bilan.a_poser)?;
    bilan.refuses_par_la_base = bilan.a_poser.len().saturating_sub(bilan.ecrits);
    Ok(bilan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::metadata::musicbrainz_release::MBTrack;

    fn credit(mbid: &str, nom: &str) -> CreditArtiste {
        CreditArtiste {
            mbid: mbid.into(),
            nom: nom.into(),
            nom_credite: nom.into(),
            nom_de_tri: String::new(),
        }
    }

    fn conf<'a>(
        id: i64,
        nom: &'a str,
        role: Role,
        credits: &'a [CreditArtiste],
    ) -> Confrontation<'a> {
        Confrontation {
            artiste_id: id,
            nom,
            mbid_actuel: None,
            role,
            credits,
        }
    }

    #[test]
    fn un_credit_qui_nomme_la_fiche_donne_son_mbid() {
        let credits = [credit("mb-air", "Air")];
        let b = rattacher(&[conf(1, "AIR", Role::Album, &credits)]);
        assert_eq!(b.a_poser, vec![(1, "mb-air".to_string())]);
    }

    #[test]
    fn le_nom_normalise_suffit_accents_article_ponctuation() {
        let credits = [
            credit("mb-stones", "The Rolling Stones"),
            credit("mb-jj", "J.J. Cale"),
        ];
        let b = rattacher(&[
            conf(1, "Rolling Stones", Role::Album, &credits),
            conf(2, "JJ Cale", Role::Piste, &credits),
        ]);
        assert_eq!(
            b.a_poser,
            vec![(1, "mb-stones".to_string()), (2, "mb-jj".to_string())]
        );
    }

    #[test]
    fn un_credit_multiple_rattache_chacun_des_siens() {
        // « Beethoven; Berliner Philharmoniker, Herbert von Karajan ».
        let credits = [
            credit("mb-lvb", "Ludwig van Beethoven"),
            credit("mb-bp", "Berliner Philharmoniker"),
            credit("mb-hvk", "Herbert von Karajan"),
        ];
        let b = rattacher(&[conf(7, "Herbert von Karajan", Role::Album, &credits)]);
        assert_eq!(b.a_poser, vec![(7, "mb-hvk".to_string())]);
    }

    #[test]
    fn le_nom_credite_et_le_nom_de_tri_comptent_aussi() {
        let credits = [CreditArtiste {
            mbid: "mb-beatles".into(),
            nom: "The Beatles".into(),
            nom_credite: "Beatles".into(),
            nom_de_tri: "Beatles, The".into(),
        }];
        let b = rattacher(&[conf(1, "Beatles, The", Role::Album, &credits)]);
        assert_eq!(b.a_poser, vec![(1, "mb-beatles".to_string())]);
    }

    #[test]
    fn un_mbid_deja_present_n_est_jamais_ecrase() {
        let credits = [credit("mb-nouveau", "Air")];
        let mut c = conf(1, "Air", Role::Album, &credits);
        c.mbid_actuel = Some("mb-ancien");
        let b = rattacher(&[c.clone()]);
        assert!(b.a_poser.is_empty());
        assert_eq!(b.desaccords, 1);

        c.mbid_actuel = Some("MB-NOUVEAU");
        let b = rattacher(&[c]);
        assert!(b.a_poser.is_empty());
        assert_eq!(b.deja_poses, 1);
    }

    #[test]
    fn deux_homonymes_dans_le_credit_rien_n_est_ecrit() {
        let credits = [
            credit("mb-john-1", "John Williams"),
            credit("mb-john-2", "John Williams"),
        ];
        let b = rattacher(&[conf(1, "John Williams", Role::Album, &credits)]);
        assert!(b.a_poser.is_empty());
        assert_eq!(b.ambigus, 1);
    }

    #[test]
    fn deux_roles_deux_mbid_rien_n_est_ecrit() {
        let album = [credit("mb-a", "Nirvana")];
        let piste = [credit("mb-b", "Nirvana")];
        let b = rattacher(&[
            conf(1, "Nirvana", Role::Album, &album),
            conf(1, "Nirvana", Role::Piste, &piste),
        ]);
        assert!(b.a_poser.is_empty());
        assert_eq!(b.ambigus, 1);
    }

    #[test]
    fn deux_roles_le_meme_mbid_une_seule_ecriture() {
        let credits = [credit("mb-a", "Nirvana")];
        let b = rattacher(&[
            conf(1, "Nirvana", Role::Album, &credits),
            conf(1, "Nirvana", Role::Piste, &credits),
            conf(1, "Nirvana", Role::Piste, &credits),
        ]);
        assert_eq!(b.a_poser, vec![(1, "mb-a".to_string())]);
    }

    #[test]
    fn deux_fiches_pour_un_meme_mbid_rien_n_est_ecrit() {
        let credits = [credit("mb-a", "Etienne Daho")];
        let b = rattacher(&[
            conf(1, "Etienne Daho", Role::Album, &credits),
            conf(2, "Étienne Daho", Role::Piste, &credits),
        ]);
        assert!(b.a_poser.is_empty());
        assert_eq!(b.ambigus, 2);
    }

    #[test]
    fn pseudo_artistes_et_artistes_fictifs_sont_ecartes() {
        let va = [credit(
            "89ad4ac3-39f7-470e-963a-56509c546377",
            "Various Artists",
        )];
        let inconnu = [credit("mb-x", "Unknown Artist")];
        let b = rattacher(&[
            conf(1, "Various Artists", Role::Album, &va),
            conf(2, "Unknown Artist", Role::Piste, &inconnu),
        ]);
        assert!(b.a_poser.is_empty());
        assert_eq!(b.ecartes, 2);
    }

    #[test]
    fn la_fiche_que_rien_ne_nomme_reste_sans_mbid() {
        let credits = [credit("mb-a", "Air")];
        let b = rattacher(&[conf(1, "VA", Role::Album, &credits)]);
        assert!(b.a_poser.is_empty());
        assert_eq!(b.sans_correspondance, 1);
    }

    // ---- en base ---------------------------------------------------------

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        let backend: Arc<dyn DbBackend> = Arc::new(db);
        backend
            .execute_batch(
                "INSERT INTO artists (id, name) VALUES (1, 'Air'); \
                 INSERT INTO artists (id, name) VALUES (2, 'Beth Hirsch'); \
                 INSERT INTO artists (id, name, musicbrainz_id) VALUES (3, 'Gainsbourg', 'mb-sg-local'); \
                 INSERT INTO artists (id, name, musicbrainz_id) VALUES (4, 'Autre', 'mb-pris'); \
                 INSERT INTO artists (id, name) VALUES (5, 'Pris Ailleurs'); \
                 INSERT INTO albums (id, title, artist_id) VALUES (1, 'Moon Safari', 1); \
                 INSERT INTO tracks (id, title, album_id, artist_id, track_number, disc_number) \
                   VALUES (10, 'La femme d''argent', 1, 1, 1, 1); \
                 INSERT INTO tracks (id, title, album_id, artist_id, track_number, disc_number) \
                   VALUES (11, 'All I Need', 1, 2, 2, 1); \
                 INSERT INTO tracks (id, title, album_id, artist_id, track_number, disc_number) \
                   VALUES (12, 'Sans appariement', 1, 3, 3, 1); \
                 INSERT INTO tracks (id, title, album_id, artist_id, track_number, disc_number) \
                   VALUES (13, 'Gainsbourg', 1, 3, 4, 1); \
                 INSERT INTO tracks (id, title, album_id, artist_id, track_number, disc_number) \
                   VALUES (14, 'Pris', 1, 5, 5, 1);",
            )
            .unwrap();
        backend
    }

    fn piste(pos: u32, rid: &str, credits: Vec<CreditArtiste>) -> MBTrack {
        MBTrack {
            position: pos,
            disc: 1,
            title: format!("t{pos}"),
            recording_id: Some(rid.into()),
            artist_credits: credits,
            ..Default::default()
        }
    }

    fn mbid(backend: &Arc<dyn DbBackend>, id: i64) -> Option<String> {
        fiche(backend, id).unwrap().and_then(|(_, m)| m)
    }

    #[test]
    fn en_base_pose_par_role_sans_ecraser_ni_doubler() {
        let backend = base();
        let detail = MBReleaseDetail {
            release_id: "rel".into(),
            artist_credits: vec![credit("mb-air", "Air")],
            tracks: vec![
                piste(1, "rec-1", vec![credit("mb-air", "Air")]),
                piste(
                    2,
                    "rec-2",
                    vec![credit("mb-air", "Air"), credit("mb-beth", "Beth Hirsch")],
                ),
                piste(3, "rec-3", vec![credit("mb-sg-nouveau", "Gainsbourg")]),
                piste(4, "rec-4", vec![credit("mb-sg-nouveau", "Gainsbourg")]),
                piste(5, "rec-5", vec![credit("mb-pris", "Pris Ailleurs")]),
            ],
            ..Default::default()
        };
        // La piste 12 n'est pas appariée : elle n'apporte rien.
        let recordings = vec![
            (10, "rec-1".to_string()),
            (11, "rec-2".to_string()),
            (13, "rec-4".to_string()),
            (14, "rec-5".to_string()),
        ];
        let b = rattacher_les_artistes_de_l_album(&backend, 1, &detail, &recordings).unwrap();

        assert_eq!(
            mbid(&backend, 1).as_deref(),
            Some("mb-air"),
            "artiste d'album"
        );
        assert_eq!(
            mbid(&backend, 2).as_deref(),
            Some("mb-beth"),
            "artiste de piste"
        );
        assert_eq!(
            mbid(&backend, 3).as_deref(),
            Some("mb-sg-local"),
            "un MBID présent n'est jamais écrasé"
        );
        assert_eq!(
            mbid(&backend, 5),
            None,
            "MBID déjà porté par une autre fiche"
        );
        assert_eq!(b.desaccords, 1);
        assert_eq!(b.a_poser.len(), 3);
        assert_eq!(b.ecrits, 2);
        assert_eq!(b.refuses_par_la_base, 1);
    }

    #[test]
    fn la_garde_de_l_update_protege_une_fiche_remplie_entre_temps() {
        let backend = base();
        let n = poser_les_mbid(&backend, &[(3, "mb-autre".to_string())]).unwrap();
        assert_eq!(n, 0);
        assert_eq!(mbid(&backend, 3).as_deref(), Some("mb-sg-local"));
    }
}
