//! #5454 — la pochette MAJORITAIRE face aux VRAIES passes (scan rapide, scan
//! de démarrage, surveillant), sur de vrais FLAC, après la retouche d'un seul
//! fichier.
//!
//! Décision de Bertrand du 29/09/2026 sur #5034 (point 1, « suivre une
//! jaquette retouchée ») : une retouche n'est suivie que si elle devient
//! MAJORITAIRE. Retoucher la seule piste qui avait donné la pochette de
//! l'album ne la change plus ; la piste garde sa nouvelle image pour elle.
use super::pochettes_disque_tests_5034::{
    Passe, etat_sur, lot_du_surveillant, poser_jaquette, racine, scan_de_demarrage, scan_manuel,
};
use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::AudioFile;
use lofty::flac::FlacFile;
use lofty::ogg::VorbisComments;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::db::track_repo::TrackRepo;
use tune_core::library::artwork::content_hash;

const B: &[u8] = b"\xFF\xD8\xFF\xE0JAQUETTE-DE-L-ALBUM-5454";
const C: &[u8] = b"\xFF\xD8\xFF\xE0JAQUETTE-RETOUCHEE-5454";

fn nommer(h: Option<&str>) -> &'static str {
    match h {
        None => "aucune",
        Some(h) if h == content_hash(B) => "B",
        Some(h) if h == content_hash(C) => "C",
        Some(_) => "autre",
    }
}

/// Un album de dix pistes, toutes à la jaquette `B`, daté d'hier.
fn album_de_dix(racine: &Path) -> Vec<PathBuf> {
    let dossier = racine
        .join("Johnny Hallyday")
        .join("A partir de maintenant");
    std::fs::create_dir_all(&dossier).unwrap();
    let gabarit =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tune-core/tests/fixtures/test.flac");
    let hier = SystemTime::now() - Duration::from_secs(86_400);
    (1..=10u32)
        .map(|n| {
            let piste = dossier.join(format!("{n:02} - Titre {n}.flac"));
            std::fs::copy(&gabarit, &piste).unwrap();
            let mut f = std::fs::File::open(&piste).unwrap();
            let mut flac = FlacFile::read_from(&mut f, ParseOptions::new()).unwrap();
            drop(f);
            let mut vc = VorbisComments::default();
            let titre = format!("Titre {n}");
            let numero = n.to_string();
            for (k, v) in [
                ("TITLE", titre.as_str()),
                ("ARTIST", "Johnny Hallyday"),
                ("ALBUMARTIST", "Johnny Hallyday"),
                ("ALBUM", "A partir de maintenant"),
                ("TRACKNUMBER", numero.as_str()),
            ] {
                vc.insert(k.to_string(), v.to_string());
            }
            flac.set_vorbis_comments(vc);
            flac.save_to_path(&piste, WriteOptions::default()).unwrap();
            poser_jaquette(&piste, Some(B));
            std::fs::File::options()
                .write(true)
                .open(&piste)
                .and_then(|f| f.set_modified(hier))
                .unwrap();
            piste
        })
        .collect()
}

fn pochette_album(db: &Arc<dyn DbBackend>, pistes: &[PathBuf]) -> &'static str {
    let aid = TrackRepo::with_backend(db.clone())
        .get_by_path(&pistes[0].to_string_lossy())
        .unwrap()
        .expect("piste indexée")
        .album_id
        .expect("album");
    nommer(
        AlbumRepo::with_backend(db.clone())
            .get(aid)
            .unwrap()
            .unwrap()
            .cover_path
            .as_deref(),
    )
}

/// La pochette PROPRE de chaque piste (colonne brute, sans `COALESCE`).
fn pochettes_propres(db: &Arc<dyn DbBackend>, pistes: &[PathBuf]) -> Vec<&'static str> {
    pistes
        .iter()
        .map(|p| {
            let chemin = p.to_string_lossy().into_owned();
            let params: [&dyn ToSqlValue; 1] = [&chemin];
            let brute = db
                .query_one_strong("SELECT cover_path FROM tracks WHERE file_path = ?", &params)
                .unwrap()
                .and_then(|c| c.first().and_then(|v| v.as_string()));
            nommer(brute.as_deref())
        })
        .collect()
}

/// Ce qu'affiche chaque piste : `COALESCE(t.cover_path, al.cover_path)`.
fn pochettes_affichees(db: &Arc<dyn DbBackend>, pistes: &[PathBuf]) -> Vec<&'static str> {
    pistes
        .iter()
        .map(|p| {
            nommer(
                TrackRepo::with_backend(db.clone())
                    .get_by_path(&p.to_string_lossy())
                    .unwrap()
                    .expect("piste indexée")
                    .cover_path
                    .as_deref(),
            )
        })
        .collect()
}

/// La piste qui a DONNÉ la pochette de l'album (`albums.cover_source_path`) :
/// l'ordre de lecture du scan est parallèle, ce n'est pas forcément la piste 1.
fn piste_source(db: &Arc<dyn DbBackend>, pistes: &[PathBuf]) -> usize {
    let aid = TrackRepo::with_backend(db.clone())
        .get_by_path(&pistes[0].to_string_lossy())
        .unwrap()
        .unwrap()
        .album_id
        .unwrap();
    let fichier = AlbumRepo::with_backend(db.clone())
        .etat_pochette(aid)
        .unwrap()
        .unwrap()
        .fichier
        .expect("pochette tirée d'une piste");
    pistes
        .iter()
        .position(|p| p.to_string_lossy() == fichier)
        .expect("la source est une piste de l'album")
}

/// Dix pistes à la jaquette B. On retouche en C la jaquette de LA piste qui a
/// donné sa pochette à l'album, et la passe la relit :
/// - l'album GARDE B (neuf pistes contre une) ; avant #5454, il suivait la
///   retouche de sa piste source et prenait C (#5034, point 1) ;
/// - la piste retouchée porte C pour elle seule, les neuf autres n'écrivent
///   rien.
///
/// Puis on retouche cinq pistes de plus (six C contre quatre B) : la retouche
/// est devenue majoritaire, l'album la suit ; les quatre pistes restées en B
/// gardent la leur, les six en C retombent sur celle de l'album.
#[tokio::test]
async fn une_retouche_n_est_suivie_que_si_elle_devient_majoritaire_5454() {
    let _seul = crate::routes::system::scan::serialiser_les_scans_de_test();
    for passe in [Passe::Rapide, Passe::Demarrage, Passe::Surveillant] {
        let r = racine(&format!("majorite-5454-{passe:?}"));
        let pistes = album_de_dix(&r);
        let etat = etat_sur(&r);
        let db = etat.backend.clone();
        scan_manuel(&etat, false, None).await;
        assert_eq!(pochette_album(&db, &pistes), "B", "{passe:?} : montage");
        let source = piste_source(&db, &pistes);

        let rejouer = |touchees: Vec<PathBuf>| {
            let etat = etat.clone();
            let db = db.clone();
            let r: PathBuf = r.to_path_buf();
            async move {
                match passe {
                    Passe::Surveillant => {
                        for p in &touchees {
                            lot_du_surveillant(&db, &r, std::slice::from_ref(p), &[]);
                        }
                    }
                    Passe::Demarrage => scan_de_demarrage(&db).await,
                    _ => scan_manuel(&etat, false, None).await,
                }
            }
        };

        // 1. Une seule piste retouchée : celle qui avait donné la pochette.
        poser_jaquette(&pistes[source], Some(C));
        rejouer(vec![pistes[source].clone()]).await;
        let mut une = vec!["aucune"; 10];
        une[source] = "C";
        let mut affichage = vec!["B"; 10];
        affichage[source] = "C";
        assert_eq!(
            (
                pochette_album(&db, &pistes),
                pochettes_propres(&db, &pistes),
                pochettes_affichees(&db, &pistes)
            ),
            ("B", une, affichage),
            "{passe:?} : la retouche MINORITAIRE (1 piste sur 10) de la piste source \
             (n° {}) ne doit pas changer la pochette de l'album — (album, pochettes propres, \
             affichage)",
            source + 1
        );

        // 2. Cinq de plus : six C contre quatre B, la retouche l'emporte.
        let cinq: Vec<usize> = (0..10).filter(|&i| i != source).take(5).collect();
        for &i in &cinq {
            poser_jaquette(&pistes[i], Some(C));
        }
        rejouer(cinq.iter().map(|&i| pistes[i].clone()).collect()).await;
        let en_c = |i: usize| i == source || cinq.contains(&i);
        let propres: Vec<&str> = (0..10)
            .map(|i| if en_c(i) { "aucune" } else { "B" })
            .collect();
        let affichage: Vec<&str> = (0..10).map(|i| if en_c(i) { "C" } else { "B" }).collect();
        assert_eq!(
            (
                pochette_album(&db, &pistes),
                pochettes_propres(&db, &pistes),
                pochettes_affichees(&db, &pistes)
            ),
            ("C", propres, affichage),
            "{passe:?} : une retouche devenue MAJORITAIRE (6 pistes sur 10) est suivie — \
             (album, pochettes propres, affichage)"
        );
    }
}
