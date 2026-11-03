//! D'après MetaRust, de Xavier Joly (code offert à Tune le 05/10/2026).
//!
//! La COMPLÉTUDE d'un album identifié : « il manque les pistes 7 et 8
//! (disque 2) » par rapport à l'édition MusicBrainz retenue (#4805, idée 6 de
//! l'analyse de MetaRust).
//!
//! # Ce qui est repris, ce qui change
//!
//! `completeness_against_release` (MetaRust, `completeness.rs`) est reprise :
//! même clé `(disque, position)` avec un disque ramené à 1 au minimum, mêmes
//! listes de pistes manquantes et de numéros en double, même décompte
//! « présentes / attendues ». Elle travaille ici sur les types de Tune :
//! [`MBTrack`] côté MusicBrainz, [`LocalTrack`] côté bibliothèque, au lieu des
//! `ReleaseInfo` et de la carte d'appariement de MetaRust. Deux ajouts :
//!
//! * l'appariement est fait ICI, et avec la prudence de
//!   [`super::reidentify::map_recording_ids`] : la place d'abord, puis le
//!   titre, mais seulement s'il ne désigne qu'une piste de chaque côté et que
//!   la piste locale n'est pas déjà à sa place sur la release. Un fichier mal
//!   numéroté n'est donc pas compté manquant s'il porte un titre sans
//!   ambiguïté ;
//! * les disques VIDÉO d'un coffret (DVD-Video, Blu-ray) sont écartés : une
//!   bibliothèque audio ne les porte pas, et les annoncer « manquants »
//!   rendrait incomplet tout CD+DVD.
//!
//! # Hors réseau
//!
//! Le calcul lit la release gardée en base (`musicbrainz_release_cache`) :
//! afficher la fiche d'un album ne coûte AUCUNE requête MusicBrainz. Le
//! résultat est gardé dans `album_completude`, qui sert le filtre « albums
//! incomplets » de la Bibliothèque. La passe des crédits (#4805 E), qui a la
//! release en main, le recalcule à chaque disque.
//!
//! Rien n'est écrit dans les fichiers ; rien de ce que MusicBrainz ou
//! l'utilisateur a posé n'est modifié : la complétude est un CONSTAT.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, warn};

use super::musicbrainz_release::{MBTrack, parse_release_detail};
use super::reidentify::LocalTrack;
use crate::db::backend::{DbBackend, ToSqlValue};

/// Une piste de la release qu'aucun fichier ne porte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PisteManquante {
    pub disque: u32,
    pub position: u32,
    /// Le numéro imprimé quand il diffère (vinyle : `B3`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub numero: Option<String>,
    pub titre: String,
}

/// Une place de la release occupée par plusieurs fichiers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Doublon {
    pub disque: u32,
    pub position: u32,
    pub fichiers: u32,
}

/// Le constat de complétude d'un album.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Completude {
    /// Pistes de la release (disques audio seulement).
    pub attendues: usize,
    /// Pistes de la release qu'un fichier porte.
    pub presentes: usize,
    pub manquantes: Vec<PisteManquante>,
    pub doublons: Vec<Doublon>,
    /// Fichiers locaux qui ne correspondent à aucune piste de la release
    /// (bonus d'une autre édition, numérotation hors tracklist).
    pub hors_release: usize,
}

impl Completude {
    /// MetaRust : `has_issues`.
    pub fn a_des_ecarts(&self) -> bool {
        !self.manquantes.is_empty() || !self.doublons.is_empty()
    }

    pub fn incomplet(&self) -> bool {
        !self.manquantes.is_empty()
    }
}

fn normaliser_titre(s: &str) -> String {
    super::musicbrainz_release::normalize(s)
}

/// Compare la tracklist MusicBrainz aux pistes locales.
///
/// Reprise de `completeness_against_release` (MetaRust), adaptée à
/// [`MBTrack`] / [`LocalTrack`].
pub fn completude_contre_release(mb: &[MBTrack], locales: &[LocalTrack]) -> Completude {
    let cle_mb = |t: &MBTrack| (t.disc.max(1), t.position);
    let cle_locale = |t: &LocalTrack| -> Option<(u32, u32)> {
        if t.position <= 0 {
            return None;
        }
        Some((t.disc.max(1) as u32, t.position as u32))
    };

    // Combien de fichiers occupent chaque place.
    let mut par_place: HashMap<(u32, u32), u32> = HashMap::new();
    for lt in locales {
        if let Some(k) = cle_locale(lt) {
            *par_place.entry(k).or_insert(0) += 1;
        }
    }
    let places_mb: std::collections::HashSet<(u32, u32)> = mb.iter().map(cle_mb).collect();

    // Fichiers qui ne sont à aucune place de la release : candidats au
    // repli par titre, et sinon « hors release ».
    let egares: Vec<&LocalTrack> = locales
        .iter()
        .filter(|lt| cle_locale(lt).is_none_or(|k| !places_mb.contains(&k)))
        .collect();
    let mut egares_utilises = vec![false; egares.len()];

    let mut manquantes = Vec::new();
    let mut presentes = 0usize;
    for t in mb {
        let k = cle_mb(t);
        if par_place.get(&k).copied().unwrap_or(0) > 0 {
            presentes += 1;
            continue;
        }
        // Repli par titre : unique des deux côtés, parmi les égarés.
        let titre = normaliser_titre(&t.title);
        let unique_cote_mb = !titre.is_empty()
            && mb
                .iter()
                .filter(|o| normaliser_titre(&o.title) == titre)
                .count()
                == 1;
        let candidats: Vec<usize> = if unique_cote_mb {
            egares
                .iter()
                .enumerate()
                .filter(|(i, lt)| !egares_utilises[*i] && normaliser_titre(&lt.title) == titre)
                .map(|(i, _)| i)
                .collect()
        } else {
            Vec::new()
        };
        if candidats.len() == 1 {
            egares_utilises[candidats[0]] = true;
            presentes += 1;
            continue;
        }
        manquantes.push(PisteManquante {
            disque: k.0,
            position: t.position,
            numero: t
                .number
                .clone()
                .filter(|n| n.trim() != t.position.to_string()),
            titre: t.title.clone(),
        });
    }
    manquantes.sort_by_key(|m| (m.disque, m.position));

    let mut doublons: Vec<Doublon> = par_place
        .iter()
        .filter(|(k, n)| **n > 1 && places_mb.contains(k))
        .map(|((d, p), n)| Doublon {
            disque: *d,
            position: *p,
            fichiers: *n,
        })
        .collect();
    doublons.sort_by_key(|d| (d.disque, d.position));

    Completude {
        attendues: mb.len(),
        presentes,
        manquantes,
        doublons,
        hors_release: egares_utilises.iter().filter(|u| !**u).count(),
    }
}

/// Les formats de support qui ne portent pas d'audio que la bibliothèque
/// aurait (vidéo d'un coffret CD+DVD).
fn support_video(format: &str) -> bool {
    let f = format.to_ascii_lowercase();
    f == "dvd"
        || f.contains("dvd-video")
        || f.contains("blu-ray")
        || f == "vhs"
        || f.contains("vcd")
}

/// Les pistes AUDIO d'une réponse `release?inc=recordings…`.
pub fn pistes_audio(release: &Value) -> Vec<MBTrack> {
    let mut copie = release.clone();
    if let Some(media) = copie.get_mut("media").and_then(Value::as_array_mut) {
        // On garde la position déclarée de chaque disque : retirer le DVD
        // d'un coffret ne doit pas renuméroter le disque suivant.
        for (idx, m) in media.iter_mut().enumerate() {
            if m.get("position").is_none()
                && let Some(o) = m.as_object_mut()
            {
                o.insert("position".into(), Value::from(idx as u64 + 1));
            }
        }
        media.retain(|m| {
            !m.get("format")
                .and_then(Value::as_str)
                .is_some_and(support_video)
        });
    }
    parse_release_detail(&copie)
        .map(|d| d.tracks)
        .unwrap_or_default()
}

// ── En base ─────────────────────────────────────────────────────────────────

/// Les pistes locales d'un album.
pub fn pistes_locales(backend: &Arc<dyn DbBackend>, album_id: i64) -> Vec<LocalTrack> {
    backend
        .query_many(
            "SELECT id, disc_number, track_number, title FROM tracks \
             WHERE album_id = ? ORDER BY disc_number, track_number, id",
            &[&album_id as &dyn ToSqlValue],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            Some(LocalTrack {
                id: r.first()?.as_i64()?,
                disc: r.get(1).and_then(|v| v.as_i64()).unwrap_or(1) as i32,
                position: r.get(2).and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                title: r.get(3).and_then(|v| v.as_string()).unwrap_or_default(),
            })
        })
        .collect()
}

/// Le pressage MusicBrainz d'un album, s'il en a un.
pub fn pressage_de_l_album(backend: &Arc<dyn DbBackend>, album_id: i64) -> Option<String> {
    backend
        .query_one(
            "SELECT musicbrainz_release_id FROM albums WHERE id = ?",
            &[&album_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()
        .and_then(|r| r.first().and_then(|v| v.as_string()))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Garde le constat d'un album.
pub fn garder(backend: &Arc<dyn DbBackend>, album_id: i64, release_mbid: &str, c: &Completude) {
    let detail = serde_json::to_string(c).unwrap_or_default();
    let attendues = c.attendues as i64;
    let presentes = c.presentes as i64;
    let manquantes = c.manquantes.len() as i64;
    let doublons = c.doublons.len() as i64;
    let quand = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    if let Err(e) = backend.execute(
        "INSERT INTO album_completude \
         (album_id, release_mbid, attendues, presentes, manquantes, doublons, detail, calcule_le) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT (album_id) DO UPDATE SET \
         release_mbid = excluded.release_mbid, attendues = excluded.attendues, \
         presentes = excluded.presentes, manquantes = excluded.manquantes, \
         doublons = excluded.doublons, detail = excluded.detail, calcule_le = excluded.calcule_le",
        &[
            &album_id as &dyn ToSqlValue,
            &release_mbid as &dyn ToSqlValue,
            &attendues as &dyn ToSqlValue,
            &presentes as &dyn ToSqlValue,
            &manquantes as &dyn ToSqlValue,
            &doublons as &dyn ToSqlValue,
            &detail as &dyn ToSqlValue,
            &quand as &dyn ToSqlValue,
        ],
    ) {
        warn!(album_id, erreur = %e, "album_completude_ecriture_impossible");
    }
}

/// Calcule et garde la complétude d'un album à partir d'une réponse release
/// déjà en main (la passe des crédits). Rend `None` si la réponse n'a pas de
/// tracklist.
pub fn constater(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    release_mbid: &str,
    release: &Value,
) -> Option<Completude> {
    let mb = pistes_audio(release);
    if mb.is_empty() {
        return None;
    }
    let c = completude_contre_release(&mb, &pistes_locales(backend, album_id));
    debug!(
        album_id,
        attendues = c.attendues,
        manquantes = c.manquantes.len(),
        "album_completude"
    );
    garder(backend, album_id, release_mbid, &c);
    Some(c)
}

/// Ce que la fiche d'album peut dire de sa complétude.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Etat {
    /// L'album n'a pas de pressage MusicBrainz : rien à comparer.
    NonIdentifie,
    /// Le pressage est connu, mais sa release n'est pas (ou plus) gardée en
    /// base : la passe des crédits la ramènera.
    ReleaseNonGardee { release_mbid: String },
    Calculee {
        release_mbid: String,
        completude: Completude,
    },
}

/// La complétude d'un album, HORS RÉSEAU : la release gardée en base, sinon
/// le dernier constat gardé s'il porte encore sur le même pressage.
pub fn pour_album(backend: &Arc<dyn DbBackend>, album_id: i64) -> Etat {
    let Some(mbid) = pressage_de_l_album(backend, album_id) else {
        return Etat::NonIdentifie;
    };
    let inc = "recordings";
    if let Some(release) =
        super::musicbrainz_release_cache::lire(backend, &mbid, inc, chrono::Utc::now())
        && let Some(c) = constater(backend, album_id, &mbid, &release)
    {
        return Etat::Calculee {
            release_mbid: mbid,
            completude: c,
        };
    }
    if let Some(c) = lire_garde(backend, album_id, &mbid) {
        return Etat::Calculee {
            release_mbid: mbid,
            completude: c,
        };
    }
    Etat::ReleaseNonGardee { release_mbid: mbid }
}

/// Le dernier constat gardé, s'il porte sur `release_mbid` : un album
/// ré-identifié depuis ne doit pas montrer les trous de l'ancien pressage.
pub fn lire_garde(
    backend: &Arc<dyn DbBackend>,
    album_id: i64,
    release_mbid: &str,
) -> Option<Completude> {
    let r = backend
        .query_one(
            "SELECT release_mbid, detail FROM album_completude WHERE album_id = ?",
            &[&album_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()?;
    let mbid = r.first().and_then(|v| v.as_string())?;
    if mbid.trim() != release_mbid.trim() {
        return None;
    }
    serde_json::from_str(&r.get(1).and_then(|v| v.as_string())?).ok()
}

/// Les albums incomplets : au moins une piste manquante, sur le pressage que
/// l'album porte AUJOURD'HUI. Sert le filtre de la Bibliothèque.
pub fn albums_incomplets(backend: &Arc<dyn DbBackend>) -> Vec<i64> {
    backend
        .query_many(
            "SELECT c.album_id FROM album_completude c \
             JOIN albums a ON a.id = c.album_id \
             WHERE c.manquantes > 0 AND a.musicbrainz_release_id = c.release_mbid \
             ORDER BY c.album_id",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mb(disc: u32, position: u32, title: &str) -> MBTrack {
        MBTrack {
            position,
            disc,
            title: title.into(),
            ..Default::default()
        }
    }

    fn local(id: i64, disc: i32, position: i32, title: &str) -> LocalTrack {
        LocalTrack {
            id,
            disc,
            position,
            title: title.into(),
        }
    }

    /// Le cas du brief : un album de deux disques auquel il manque les
    /// pistes 7 et 8 du disque 2.
    #[test]
    fn detecte_un_trou_sur_un_album_a_plusieurs_disques() {
        let mut release = Vec::new();
        for p in 1..=6 {
            release.push(mb(1, p, &format!("Un-{p}")));
        }
        for p in 1..=9 {
            release.push(mb(2, p, &format!("Deux-{p}")));
        }
        let mut locales = Vec::new();
        for p in 1..=6 {
            locales.push(local(p as i64, 1, p, &format!("Un-{p}")));
        }
        for p in [1, 2, 3, 4, 5, 6, 9] {
            locales.push(local(100 + p as i64, 2, p, &format!("Deux-{p}")));
        }
        let c = completude_contre_release(&release, &locales);
        assert_eq!(c.attendues, 15);
        assert_eq!(c.presentes, 13);
        assert_eq!(
            c.manquantes
                .iter()
                .map(|m| (m.disque, m.position))
                .collect::<Vec<_>>(),
            vec![(2, 7), (2, 8)],
            "il manque les pistes 7 et 8 du disque 2, et elles seules"
        );
        assert!(c.incomplet());
        // Le disque 1 complet n'est pas confondu avec le disque 2 : la piste 7
        // n'existe que sur le disque 2.
        assert!(c.doublons.is_empty());
    }

    #[test]
    fn un_album_complet_n_a_pas_d_ecart() {
        let release = vec![mb(1, 1, "A"), mb(1, 2, "B")];
        let locales = vec![local(1, 1, 1, "A"), local(2, 1, 2, "B")];
        let c = completude_contre_release(&release, &locales);
        assert!(!c.a_des_ecarts());
        assert_eq!(c.presentes, 2);
    }

    #[test]
    fn un_disque_zero_vaut_le_disque_un() {
        let release = vec![mb(1, 1, "A"), mb(1, 2, "B")];
        let locales = vec![local(1, 0, 1, "A"), local(2, 0, 2, "B")];
        assert!(!completude_contre_release(&release, &locales).incomplet());
    }

    #[test]
    fn deux_fichiers_a_la_meme_place_sont_un_doublon() {
        let release = vec![mb(1, 1, "A"), mb(1, 2, "B")];
        let locales = vec![
            local(1, 1, 1, "A"),
            local(2, 1, 1, "A (bis)"),
            local(3, 1, 2, "B"),
        ];
        let c = completude_contre_release(&release, &locales);
        assert_eq!(
            c.doublons,
            vec![Doublon {
                disque: 1,
                position: 1,
                fichiers: 2
            }]
        );
        assert!(!c.incomplet());
    }

    /// Un fichier mal numéroté (disque 1 piste 12 au lieu du disque 2 piste 3)
    /// n'est pas compté manquant si son titre est sans ambiguïté.
    #[test]
    fn un_fichier_mal_numerote_se_retrouve_par_son_titre_unique() {
        let release = vec![mb(1, 1, "Ouverture"), mb(2, 1, "Final")];
        let locales = vec![local(1, 1, 1, "Ouverture"), local(2, 1, 12, "Final")];
        let c = completude_contre_release(&release, &locales);
        assert!(!c.incomplet(), "{c:?}");
        assert_eq!(c.hors_release, 0);
    }

    /// Un titre répété (reprise, prise alternative) ne sert PAS au repli.
    #[test]
    fn un_titre_repete_ne_sert_pas_au_repli() {
        let release = vec![mb(1, 1, "Blues"), mb(1, 2, "Blues")];
        let locales = vec![local(1, 1, 1, "Blues"), local(2, 1, 9, "Blues")];
        let c = completude_contre_release(&release, &locales);
        assert_eq!(c.manquantes.len(), 1);
        assert_eq!(c.hors_release, 1);
    }

    #[test]
    fn le_dvd_d_un_coffret_n_est_pas_compte_manquant() {
        let release = serde_json::json!({
            "id": "r1",
            "title": "Coffret",
            "media": [
                {"position": 1, "format": "CD", "tracks": [
                    {"position": 1, "title": "A", "recording": {"id": "a"}},
                    {"position": 2, "title": "B", "recording": {"id": "b"}}
                ]},
                {"position": 2, "format": "DVD-Video", "tracks": [
                    {"position": 1, "title": "Clip", "recording": {"id": "c"}}
                ]},
                {"position": 3, "format": "CD", "tracks": [
                    {"position": 1, "title": "C", "recording": {"id": "d"}}
                ]}
            ]
        });
        let pistes = pistes_audio(&release);
        assert_eq!(pistes.len(), 3);
        assert_eq!(
            (pistes[2].disc, pistes[2].position),
            (3, 1),
            "le disque 3 garde son numéro"
        );
        let locales = vec![
            local(1, 1, 1, "A"),
            local(2, 1, 2, "B"),
            local(3, 3, 1, "C"),
        ];
        assert!(!completude_contre_release(&pistes, &locales).incomplet());
    }

    fn base() -> Arc<dyn DbBackend> {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// La release gardée en base suffit : aucune requête, et le constat
    /// alimente le filtre « albums incomplets » tant que l'album garde CE
    /// pressage.
    #[test]
    fn le_constat_hors_reseau_alimente_le_filtre_des_incomplets() {
        let b = base();
        b.execute(
            "INSERT INTO albums (id, title, musicbrainz_release_id) VALUES (7, 'Double', 'rel-1')",
            &[],
        )
        .unwrap();
        for (id, disc, n) in [(1, 1, 1), (2, 1, 2), (3, 2, 1)] {
            b.execute(
                "INSERT INTO tracks (id, title, album_id, disc_number, track_number, file_path) \
                 VALUES (?, ?, 7, ?, ?, ?)",
                &[
                    &(id as i64) as &dyn ToSqlValue,
                    &format!("T{disc}-{n}") as &dyn ToSqlValue,
                    &(disc as i64) as &dyn ToSqlValue,
                    &(n as i64) as &dyn ToSqlValue,
                    &format!("/m/{id}.flac") as &dyn ToSqlValue,
                ],
            )
            .unwrap();
        }
        let release = serde_json::json!({
            "id": "rel-1", "title": "Double",
            "media": [
                {"position": 1, "tracks": [
                    {"position": 1, "title": "T1-1"}, {"position": 2, "title": "T1-2"}]},
                {"position": 2, "tracks": [
                    {"position": 1, "title": "T2-1"}, {"position": 2, "title": "T2-2"}]}
            ]
        });
        assert_eq!(
            pour_album(&b, 7),
            Etat::ReleaseNonGardee {
                release_mbid: "rel-1".into()
            }
        );
        super::super::musicbrainz_release_cache::ecrire(
            &b,
            "rel-1",
            super::super::musicbrainz_release::INC_RELEASE_COMPLET,
            &release,
            chrono::Utc::now(),
        );
        let Etat::Calculee { completude, .. } = pour_album(&b, 7) else {
            panic!("la release gardée devait suffire");
        };
        assert_eq!(
            completude.manquantes,
            vec![PisteManquante {
                disque: 2,
                position: 2,
                numero: None,
                titre: "T2-2".into()
            }]
        );
        assert_eq!(albums_incomplets(&b), vec![7]);
        // Ré-identifié ailleurs : l'ancien constat ne compte plus.
        b.execute(
            "UPDATE albums SET musicbrainz_release_id = 'rel-2' WHERE id = 7",
            &[],
        )
        .unwrap();
        assert!(albums_incomplets(&b).is_empty());
        assert!(lire_garde(&b, 7, "rel-2").is_none());
    }
}
