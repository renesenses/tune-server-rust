//! Une balise « Track N » ne doit pas voler la place du nom de fichier (#2060,
//! #3522).
//!
//! **Belkadi Yacine**, Tune 0.9.140 · Linux, tickets support 85→92 du
//! 07/09/2026. Ses deux captures montrent la même chose des deux côtés du
//! partage : dans l'explorateur, `01 Ballade De Melody Nelson.flac` …
//! `19 Glory Box (Mudflap Mix).flac` ; dans Tune, dossier
//! `music / trip hop / Portishead / Portishead - Melody Nelson`, **19 pistes
//! intitulées « Track 1 » … « Track 19 »**.
//!
//! Son journal tranche entre les deux explications possibles :
//!
//! ```text
//! 11:11:20  batched_scan_complete total=25 metadata_ok=19 metadata_failed=0
//! 11:25:20  orchestrator_play zone_id=20 title=Track 6 source=local
//! 11:25:20  transcode_required file=/mnt/music/trip hop/Portishead/
//!           Portishead - Melody Nelson/06 Melody.flac
//! ```
//!
//! La lecture des balises n'a échoué sur aucun des 19 fichiers, et le titre
//! joué pour `06 Melody.flac` est « Track 6 ». Le titre vient donc de la
//! BALISE — écrite par un logiciel de gravure qui n'a pas reconnu le disque —
//! et non d'un repli du serveur : `grep` sur tout le dépôt ne trouve aucun
//! `format!("Track {…}")` hors fichiers de test.
//!
//! Ce témoin attaque par la fonction PUBLIQUE que le scanner appelle,
//! `tune_core::metadata::try_read_metadata`, sur de vrais FLAC écrits par
//! l'encodeur du dépôt et étiquetés par `tag_writer::write_tags`. Il ne lit le
//! texte d'aucun fichier source : il fait tourner la chaîne réelle.

use std::path::{Path, PathBuf};

use tune_core::metadata::tag_writer::{TagUpdate, write_tags};
use tune_core::metadata::try_read_metadata;

/// Un FLAC réel, produit par l'encodeur du dépôt — pas un fichier fabriqué à la
/// main dont `lofty` refuserait l'en-tête.
fn octets_flac() -> Vec<u8> {
    let mut enc = tune_core::audio::encoder::AudioEncoder::new("flac", 44_100, 16, 2);
    enc.start_sync().expect("démarrage de l'encodeur FLAC");
    // 8192 trames stéréo 16 bits : deux blocs FLAC pleins, de quoi donner des
    // propriétés audio valides à `lofty`.
    let mut pcm = Vec::with_capacity(8192 * 4);
    for i in 0..8192i32 {
        let v = ((i % 512) as i16).wrapping_mul(37);
        pcm.extend_from_slice(&v.to_le_bytes());
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    enc.write_sync(&pcm).expect("écriture PCM");
    enc.finish_sync().expect("clôture de l'encodeur FLAC")
}

/// Écrit `<dossier>/<nom>` et y pose le titre demandé. `None` = aucune balise.
async fn fichier_etiquete(dossier: &Path, nom: &str, titre: Option<&str>) -> PathBuf {
    let chemin = dossier.join(nom);
    std::fs::write(&chemin, octets_flac()).expect("écriture du FLAC");
    if let Some(t) = titre {
        let maj = TagUpdate {
            title: Some(t.to_string()),
            artist_name: Some("Portishead".to_string()),
            album_title: Some("Melody Nelson".to_string()),
            ..Default::default()
        };
        write_tags(chemin.to_str().unwrap(), &maj)
            .await
            .expect("écriture des balises");
    }
    chemin
}

fn titre_lu(chemin: &Path) -> String {
    let meta = try_read_metadata(chemin).expect("les balises se lisent");
    meta.title.unwrap_or_default()
}

/// Le cas d'Yacine, joué en entier.
#[tokio::test]
async fn une_balise_track_n_cede_au_nom_de_fichier() {
    let tmp = tempfile::tempdir().unwrap();
    let dossier = tmp
        .path()
        .join("trip hop/Portishead/Portishead - Melody Nelson");
    std::fs::create_dir_all(&dossier).unwrap();

    let chemin = fichier_etiquete(&dossier, "06 Melody.flac", Some("Track 6")).await;
    assert_eq!(
        titre_lu(&chemin),
        "Melody",
        "« Track 6 » ne dit rien que le numéro de piste ne dise déjà ; le nom du \
         fichier, lui, porte le titre"
    );
}

/// Contre-épreuve n° 1 — LA garde du correctif : une balise qui dit quelque
/// chose n'est jamais écrasée, même quand le nom du fichier dit autre chose.
#[tokio::test]
async fn une_vraie_balise_n_est_jamais_ecrasee_par_le_nom_de_fichier() {
    let tmp = tempfile::tempdir().unwrap();
    let dossier = tmp.path().join("album");
    std::fs::create_dir_all(&dossier).unwrap();

    // Le nom du fichier est FAUX (renommage en vrac) ; la balise est juste.
    let chemin = fichier_etiquete(&dossier, "06 piste six.flac", Some("Glory Box")).await;
    assert_eq!(
        titre_lu(&chemin),
        "Glory Box",
        "la balise gagne : c'est la règle, et le correctif ne l'entame pas"
    );
}

/// Contre-épreuve n° 2 — un nom de fichier qui ne porte QU'UN NUMÉRO
/// n'apprend rien de plus que « Track 6 » : la balise reste.
#[tokio::test]
async fn un_nom_de_fichier_purement_numerique_ne_remplace_rien() {
    let tmp = tempfile::tempdir().unwrap();
    let dossier = tmp.path().join("album");
    std::fs::create_dir_all(&dossier).unwrap();

    let chemin = fichier_etiquete(&dossier, "06.flac", Some("Track 6")).await;
    assert_eq!(titre_lu(&chemin), "Track 6");
}

/// Contre-épreuve n° 3 — un morceau RÉELLEMENT intitulé « Track 9 » est rangé
/// dans un fichier qui le dit : les deux côtés concordent, rien ne bouge.
#[tokio::test]
async fn un_morceau_reellement_intitule_track_9_garde_son_titre() {
    let tmp = tempfile::tempdir().unwrap();
    let dossier = tmp.path().join("album");
    std::fs::create_dir_all(&dossier).unwrap();

    let chemin = fichier_etiquete(&dossier, "09 Track 9.flac", Some("Track 9")).await;
    assert_eq!(titre_lu(&chemin), "Track 9");
}

/// Le repli historique (fichier sans aucune balise) est intact : c'est déjà le
/// nom du fichier qui parlait, et il parle toujours.
#[tokio::test]
async fn un_fichier_sans_balise_prend_toujours_le_nom_de_fichier() {
    let tmp = tempfile::tempdir().unwrap();
    let dossier = tmp.path().join("album");
    std::fs::create_dir_all(&dossier).unwrap();

    let chemin = fichier_etiquete(&dossier, "03 Toy Box.flac", None).await;
    assert_eq!(titre_lu(&chemin), "Toy Box");
}
