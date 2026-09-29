//! #5483 — choisir les pistes à convertir.
//!
//! Xavier Joly (Reivax66, 0.9.168) : « il faut ajouter la possibilité de
//! naviguer dans les répertoires pour être plus précis si l'on ne veut
//! convertir que certaines pistes d'un album par exemple. »
//!
//! `POST /converter/start` acceptait déjà `{"track_id": N}` dans `sources`,
//! mais l'écran n'envoyait que des albums, et deux défauts attendaient la
//! sélection fine :
//!
//! - une piste désignée deux fois (son album coché ET la piste cochée) était
//!   convertie deux fois ;
//! - deux pistes de même nom de fichier (`CD1/01.flac`, `CD2/01.flac`)
//!   visaient le même fichier de sortie : la seconde échouait en « destination
//!   file already exists ».
//!
//! Ce module résout toutes les sources — `sources` et le nouveau `track_ids`
//! — en une liste de fichiers SANS doublon, dans l'ordre de la demande, et
//! donne à chaque sortie d'une archive un nom libre.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use tracing::warn;
use tune_core::db::models::Track;
use tune_core::db::track_repo::TrackRepo;

use super::ConvertSource;

/// Un fichier à convertir, avec ce qu'on sait de son album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PisteResolue {
    pub chemin: PathBuf,
    /// Titre de l'album, quand la piste vient de la bibliothèque.
    pub album: Option<String>,
    /// Artiste de l'album (à défaut, celui de la piste).
    pub artiste: Option<String>,
}

impl PisteResolue {
    fn depuis_la_piste(piste: &Track) -> Option<Self> {
        let chemin = piste.file_path.as_ref()?;
        Some(Self {
            chemin: PathBuf::from(chemin),
            album: piste.album_title.clone().filter(|t| !t.trim().is_empty()),
            artiste: piste
                .album_artist
                .clone()
                .filter(|a| !a.trim().is_empty())
                .or_else(|| piste.artist_name.clone())
                .filter(|a| !a.trim().is_empty()),
        })
    }
    fn depuis_un_chemin(chemin: PathBuf) -> Self {
        Self {
            chemin,
            album: None,
            artiste: None,
        }
    }
}

/// Résout `sources` puis `track_ids` en fichiers à convertir, sans doublon
/// (la première occurrence garde sa place).
pub(super) fn resoudre_les_sources(
    repo: &TrackRepo,
    sources: &[ConvertSource],
    track_ids: &[i64],
) -> Vec<PisteResolue> {
    let mut brutes: Vec<PisteResolue> = Vec::new();
    let piste = |track_id: i64, brutes: &mut Vec<PisteResolue>| match repo.get(track_id) {
        Ok(Some(track)) => match PisteResolue::depuis_la_piste(&track) {
            Some(p) => brutes.push(p),
            None => warn!(track_id, "converter_skip_no_file_path"),
        },
        Ok(None) => warn!(track_id, "converter_skip_track_not_found"),
        Err(e) => warn!(track_id, error = %e, "converter_skip_track_lookup_error"),
    };

    for src in sources {
        if let Some(track_id) = src.track_id {
            piste(track_id, &mut brutes);
        } else if let Some(album_id) = src.album_id {
            match repo.list_by_album(album_id) {
                Ok(tracks) => {
                    brutes.extend(tracks.iter().filter_map(PisteResolue::depuis_la_piste))
                }
                Err(e) => warn!(album_id, error = %e, "converter_skip_album_lookup_error"),
            }
        } else if let Some(ref path) = src.path {
            let p = PathBuf::from(path);
            if p.is_dir() {
                let mut fichiers = Vec::new();
                super::collect_audio_files(&p, &mut fichiers);
                brutes.extend(fichiers.into_iter().map(PisteResolue::depuis_un_chemin));
            } else if p.is_file() && super::convertible_input(path) {
                brutes.push(PisteResolue::depuis_un_chemin(p));
            } else {
                warn!(path, "converter_skip_not_audio_or_missing");
            }
        }
    }
    for &track_id in track_ids {
        piste(track_id, &mut brutes);
    }

    let mut vus = HashSet::new();
    brutes.retain(|p| vus.insert(p.chemin.clone()));
    brutes
}

/// Le chemin de sortie d'une piste dans le dossier d'une ARCHIVE : `nom.ext`,
/// ou `nom (2).ext`, `nom (3).ext`… si le nom est déjà pris par une piste
/// précédente du même travail. Le dossier est neuf à chaque travail : ce qui
/// s'y trouve vient de ce travail-ci.
///
/// Ne sert pas au mode « dossier de travail » (#2944), où un fichier présent
/// est laissé intact et la piste notée en erreur.
pub(super) fn sortie_libre(dossier: &Path, nom: &str, ext: &str) -> PathBuf {
    let premier = dossier.join(format!("{nom}.{ext}"));
    if !premier.exists() {
        return premier;
    }
    (2..)
        .map(|n| dossier.join(format!("{nom} ({n}).{ext}")))
        .find(|p| !p.exists())
        .expect("un suffixe libre finit toujours par exister")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tune_core::db::album_repo::AlbumRepo;
    use tune_core::db::artist_repo::ArtistRepo;
    use tune_core::db::backend::DbBackend;
    use tune_core::db::models::Artist;

    /// Une base SQLite de FICHIER (pas `:memory:`, dont le pool de lecture
    /// clone l'écriture et rend les épreuves aveugles).
    fn base() -> (tempfile::TempDir, Arc<dyn DbBackend>) {
        let dir = tempfile::tempdir().unwrap();
        let chemin = dir.path().join("tune.db");
        let db = tune_core::db::sqlite::SqliteDb::open(&chemin.to_string_lossy()).unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        (dir, Arc::new(db))
    }

    /// Un album de deux disques dont les fichiers portent les MÊMES noms.
    fn album_deux_disques(db: &Arc<dyn DbBackend>) -> (i64, Vec<i64>) {
        let artiste = ArtistRepo::with_backend(db.clone())
            .create(&Artist::new("Miles Davis".into()))
            .unwrap();
        let album = AlbumRepo::with_backend(db.clone())
            .get_or_create("Miles Smiles", artiste, Some(1967))
            .unwrap()
            .id
            .unwrap();
        let repo = TrackRepo::with_backend(db.clone());
        let mut ids = Vec::new();
        for (disque, numero, fichier, titre) in [
            (1, 1, "/m/Miles Smiles/CD1/01 - Orbits.dsf", "Orbits"),
            (1, 2, "/m/Miles Smiles/CD1/02 - Circle.dsf", "Circle"),
            (2, 1, "/m/Miles Smiles/CD2/01 - Orbits.dsf", "Orbits (alt)"),
        ] {
            let mut t = Track::new(titre.into());
            t.artist_id = Some(artiste);
            t.album_id = Some(album);
            t.disc_number = disque;
            t.track_number = numero;
            t.file_path = Some(fichier.into());
            ids.push(repo.create(&t).unwrap());
        }
        (album, ids)
    }

    fn chemins(pistes: &[PisteResolue]) -> Vec<String> {
        pistes
            .iter()
            .map(|p| p.chemin.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn seules_les_pistes_cochees_sont_resolues() {
        let (_d, db) = base();
        let (_album, ids) = album_deux_disques(&db);
        let repo = TrackRepo::with_backend(db);
        let pistes = resoudre_les_sources(&repo, &[], &[ids[2], ids[0]]);
        assert_eq!(
            chemins(&pistes),
            [
                "/m/Miles Smiles/CD2/01 - Orbits.dsf",
                "/m/Miles Smiles/CD1/01 - Orbits.dsf"
            ]
        );
        assert_eq!(pistes[0].album.as_deref(), Some("Miles Smiles"));
        assert_eq!(pistes[0].artiste.as_deref(), Some("Miles Davis"));
    }

    /// Contre-épreuve : sans le dédoublonnage, l'album et sa piste cochée
    /// rendent quatre fichiers, dont un en double.
    #[test]
    fn une_piste_designee_deux_fois_n_est_convertie_qu_une_fois() {
        let (_d, db) = base();
        let (album, ids) = album_deux_disques(&db);
        let repo = TrackRepo::with_backend(db);
        let sources = [ConvertSource {
            track_id: None,
            album_id: Some(album),
            path: None,
        }];
        let pistes = resoudre_les_sources(&repo, &sources, &[ids[1]]);
        assert_eq!(pistes.len(), 3, "{:?}", chemins(&pistes));
        assert_eq!(
            chemins(&pistes)[1],
            "/m/Miles Smiles/CD1/02 - Circle.dsf",
            "l'ordre de l'album est gardé"
        );
    }

    #[test]
    fn deux_pistes_homonymes_ne_se_heurtent_plus_dans_l_archive() {
        let dir = tempfile::tempdir().unwrap();
        let a = sortie_libre(dir.path(), "01 - Orbits", "flac");
        assert_eq!(a, dir.path().join("01 - Orbits.flac"));
        std::fs::write(&a, b"x").unwrap();
        let b = sortie_libre(dir.path(), "01 - Orbits", "flac");
        assert_eq!(b, dir.path().join("01 - Orbits (2).flac"));
        std::fs::write(&b, b"x").unwrap();
        assert_eq!(
            sortie_libre(dir.path(), "01 - Orbits", "flac"),
            dir.path().join("01 - Orbits (3).flac")
        );
    }

    /// La requête sans `sources`, avec seulement `track_ids`, est acceptée.
    #[test]
    fn la_requete_accepte_des_identifiants_de_pistes_seuls() {
        let r: super::super::StartJobRequest =
            serde_json::from_value(serde_json::json!({"format": "flac", "track_ids": [3, 7]}))
                .unwrap();
        assert!(r.sources.is_empty());
        assert_eq!(r.track_ids, [3, 7]);
    }
}
