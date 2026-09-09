//! Des plans CUE aux lignes de la bibliothèque — le lot 2b de #1763/#3631.
//!
//! [`super::cue`] lit le texte d'une feuille, [`super::cue_album`] la confronte
//! au disque et rend un plan. Les deux ont été fusionnés le 17/08/2026 par la
//! PR #1828, et depuis, **rien n'écrivait** : les `PisteCue` étaient
//! construites, comptées pour le rapport de scan, puis jetées. Aucun chemin de
//! production ne posait `cue_media_path`, alors que la colonne et son index
//! d'unicité existent sur les trois moteurs depuis la même PR.
//!
//! Ce module est le chaînon manquant. Il ne décide rien de neuf : il prend le
//! plan tel quel et le range.
//!
//! ## Ce qu'il garantit
//!
//! 1. **Aucun album fantôme.** Il n'écrit QUE ce que [`planifier_dossier`] a
//!    retenu. Les 649 feuilles `cue-image-introuvable` mesurées chez un testeur
//!    (#2060) sont écartées AVANT d'arriver ici, avec leur motif ; elles ne
//!    produisent pas une ligne de piste. C'est la règle nº1 de Gros Bidon
//!    (fil 1495) — foobar2000 fabriquait un album à partir d'un `.cue` sans
//!    fichier audio, et il a dû configurer une exception.
//! 2. **Un scan de plus ne double pas la bibliothèque.** L'identité d'une piste
//!    virtuelle est le couple `(cue_media_path, cue_start_ms)` : elle est relue
//!    avant d'écrire, et la ligne existante est MISE À JOUR, jamais dupliquée.
//!    Sans cela l'index unique partiel `idx_tracks_cue_identity` refuserait
//!    l'insertion et le scan perdrait la piste en silence.
//! 3. **Une ligne dont l'image a réellement disparu s'en va.** L'élagage
//!    ordinaire ne peut pas les voir : `get_all_file_info_by_path` filtre
//!    `file_path IS NOT NULL`, or ces pistes ont `file_path = NULL` par
//!    construction. Sans [`elaguer_les_pistes_cue`] elles resteraient en base
//!    pour toujours.
//!
//! ## Ce qu'il ne fait pas
//!
//! Il ne lit pas les balises du fichier image (une feuille CUE EST la source de
//! métadonnées, c'est tout son objet) et ne cherche pas de pochette : la ligne
//! `albums` porte son dossier, les passes existantes s'en chargent.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, info, warn};

use super::cue_album::{AlbumCue, InventaireCue, PisteCue, PlanCue, inventorier_avec};
use crate::db::album_repo::AlbumRepo;
use crate::db::artist_repo::ArtistRepo;
use crate::db::backend::DbBackend;
use crate::db::models::Track;
use crate::db::track_repo::TrackRepo;

/// Ce que l'écriture des albums CUE a réellement changé en base.
///
/// Distinct de [`InventaireCue`], qui dit ce que les feuilles DÉCRIVENT : un
/// témoin qui vérifie que la colonne existe ne prouve pas qu'elle est remplie,
/// et un inventaire qui compte des pistes ne prouve pas qu'elles ont été
/// écrites. Ces compteurs-là parlent de la base.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BilanCue {
    /// Albums pour lesquels au moins une piste a été posée ou rafraîchie.
    pub albums: usize,
    /// Pistes virtuelles créées par ce scan.
    pub pistes_creees: usize,
    /// Pistes virtuelles déjà présentes, rafraîchies en place.
    pub pistes_mises_a_jour: usize,
    /// Pistes virtuelles supprimées parce que leur fichier image a disparu.
    pub pistes_elaguees: usize,
    /// Écritures refusées par la base. Jamais fatales : une feuille bancale ne
    /// doit pas emporter le scan.
    pub echecs: usize,
}

/// L'artiste attribué à un album CUE qui ne nomme personne.
///
/// La même chaîne que le reste de la bibliothèque emploie pour un fichier sans
/// balise d'artiste : un album CUE anonyme se range avec eux au lieu de créer
/// un artiste vide à part.
const ARTISTE_INCONNU: &str = "Unknown Artist";

/// Ce qu'une sonde du fichier image apprend, une seule fois par image.
///
/// Les N pistes d'une feuille partagent le MÊME fichier : les sonder une par
/// une multiplierait par N une lecture d'en-tête sur un partage réseau.
#[derive(Debug, Clone, Copy, Default)]
struct SondeImage {
    duree_ms: Option<i64>,
    sample_rate: Option<i32>,
    bit_depth: Option<i32>,
    channels: Option<i32>,
}

/// Sonde l'en-tête du fichier image : durée totale et propriétés audio.
///
/// Lecture de métadonnées seulement, aucun décodage. La durée n'est pas un
/// agrément : c'est elle qui donne sa longueur à la DERNIÈRE piste de chaque
/// feuille, celle dont l'`INDEX` suivant n'existe pas. Sans elle, cette piste
/// entrerait en base avec `duration_ms = 0` — et un zéro y est corrosif : le
/// poller perd l'armement gapless, l'avance en fin de piste et le préchargement.
fn sonder_image(image: &Path) -> SondeImage {
    use lofty::file::AudioFile;
    match lofty::read_from_path(image) {
        Ok(tagged) => {
            let p = tagged.properties();
            SondeImage {
                duree_ms: Some(p.duration().as_millis() as i64).filter(|d| *d > 0),
                sample_rate: p.sample_rate().map(|r| r as i32),
                bit_depth: p.bit_depth().map(|b| b as i32),
                channels: p.channels().map(|c| c as i32),
            }
        }
        Err(e) => {
            // Une image illisible par lofty reste jouable par Symphonia : on
            // perd la durée de la dernière piste, pas l'album.
            debug!(image = %image.display(), error = %e, "cue_image_non_sondee");
            SondeImage::default()
        }
    }
}

/// La ligne `tracks` d'une piste virtuelle.
///
/// `file_path` reste `NULL` : c'est ce qui autorise N pistes sur le même
/// fichier sous la contrainte `UNIQUE` de `tracks.file_path`.
fn piste_en_ligne(
    piste: &PisteCue,
    album: &AlbumCue,
    album_id: Option<i64>,
    artist_id: Option<i64>,
    sonde: SondeImage,
) -> Track {
    let titre = piste
        .titre
        .clone()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or_else(|| format!("Piste {:02}", piste.numero));
    let mut t = Track::new(titre);
    t.album_id = album_id;
    t.album_title = album.titre.clone();
    t.artist_id = artist_id;
    t.artist_name = piste
        .interprete
        .clone()
        .or_else(|| album.interprete.clone());
    t.album_artist = album.interprete.clone();
    t.track_number = piste.numero as i32;
    t.file_path = None;
    t.format = piste
        .media
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase());
    t.sample_rate = sonde.sample_rate;
    t.bit_depth = sonde.bit_depth;
    if let Some(ch) = sonde.channels {
        t.channels = ch;
    }
    t.genre = album.genre.clone();
    t.year = album.annee.as_deref().and_then(annee_en_nombre);
    t.cue_media_path = Some(piste.media.to_string_lossy().into_owned());
    t.cue_start_ms = Some(piste.debut_ms as i64);
    t.cue_end_ms = piste.fin_ms.map(|f| f as i64);
    t.duration_ms = duree_de_la_tranche(piste, sonde);
    t
}

/// La durée de la tranche, en millisecondes.
///
/// `fin_ms` absent veut dire « jusqu'au bout du fichier » : c'est la durée
/// sondée de l'image qui la donne. Si la sonde a échoué, on rend 0 plutôt
/// qu'un nombre inventé — la lecture rattrape déjà `duration_ms <= 0` en
/// sondant le fichier au moment de jouer.
fn duree_de_la_tranche(piste: &PisteCue, sonde: SondeImage) -> i64 {
    let debut = piste.debut_ms as i64;
    let fin = match (piste.fin_ms, sonde.duree_ms) {
        (Some(f), _) => f as i64,
        (None, Some(total)) => total,
        (None, None) => return 0,
    };
    (fin - debut).max(0)
}

/// `REM DATE` porte parfois une date complète (`1981-03-12`) ou du bruit.
fn annee_en_nombre(brut: &str) -> Option<i32> {
    let chiffres: String = brut.chars().take_while(|c| c.is_ascii_digit()).collect();
    chiffres.parse::<i32>().ok().filter(|a| *a > 0)
}

/// Écrit un album CUE : artiste, album, puis ses pistes virtuelles.
fn ecrire_album(
    dossier: &Path,
    album: &AlbumCue,
    artist_repo: &ArtistRepo,
    album_repo: &AlbumRepo,
    track_repo: &TrackRepo,
    bilan: &mut BilanCue,
    images_couvertes: &mut HashSet<PathBuf>,
) {
    if album.pistes.is_empty() {
        return;
    }
    let nom_artiste = album
        .interprete
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(ARTISTE_INCONNU);
    let artiste = match artist_repo.get_or_create(nom_artiste, None, None) {
        Ok(a) => a,
        Err(e) => {
            warn!(dossier = %dossier.display(), error = %e, "cue_artiste_non_ecrit");
            bilan.echecs += album.pistes.len();
            return;
        }
    };
    let artist_id = artiste.id;

    // Le titre d'album manquant retombe sur le nom du dossier : une feuille
    // sans `TITLE` reste un album, et un album sans nom est introuvable.
    let titre_album = album
        .titre
        .clone()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .or_else(|| {
            dossier
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "Album".to_string());
    let annee = album.annee.as_deref().and_then(annee_en_nombre);
    let ligne_album = match (
        artist_id,
        album_repo.get_or_create_for_folder(
            &dossier.to_string_lossy(),
            &titre_album,
            artist_id.unwrap_or(0),
            annee,
            None,
        ),
    ) {
        (Some(_), Ok(a)) => a.id,
        (_, Err(e)) => {
            warn!(dossier = %dossier.display(), error = %e, "cue_album_non_ecrit");
            bilan.echecs += album.pistes.len();
            return;
        }
        (None, Ok(a)) => a.id,
    };

    // Une sonde par IMAGE, pas par piste : un vinyle en deux faces sonde deux
    // fichiers pour dix pistes.
    let mut sondes: std::collections::HashMap<PathBuf, SondeImage> =
        std::collections::HashMap::new();
    let mut ecrites = 0usize;

    for piste in &album.pistes {
        let sonde = *sondes
            .entry(piste.media.clone())
            .or_insert_with(|| sonder_image(&piste.media));
        let mut ligne = piste_en_ligne(piste, album, ligne_album, artist_id, sonde);
        let media = ligne.cue_media_path.clone().unwrap_or_default();
        let debut = ligne.cue_start_ms.unwrap_or(0);

        match track_repo.get_by_cue_identity(&media, debut) {
            Ok(Some(existante)) => {
                ligne.id = existante.id;
                match track_repo.update(&ligne) {
                    Ok(()) => {
                        bilan.pistes_mises_a_jour += 1;
                        ecrites += 1;
                    }
                    Err(e) => {
                        warn!(media = %media, debut, error = %e, "cue_piste_non_mise_a_jour");
                        bilan.echecs += 1;
                    }
                }
            }
            Ok(None) => match track_repo.create(&ligne) {
                Ok(_) => {
                    bilan.pistes_creees += 1;
                    ecrites += 1;
                }
                Err(e) => {
                    warn!(media = %media, debut, error = %e, "cue_piste_non_creee");
                    bilan.echecs += 1;
                }
            },
            Err(e) => {
                warn!(media = %media, debut, error = %e, "cue_identite_illisible");
                bilan.echecs += 1;
            }
        }
    }

    if ecrites > 0 {
        bilan.albums += 1;
        // Les fichiers image de cet album sont désormais REPRÉSENTÉS par leurs
        // tranches : le scan ordinaire ne doit plus les indexer comme des
        // pistes à part entière, sinon le même disque existe deux fois — une
        // piste « image entière » de 74 minutes à côté de ses 15 tranches.
        images_couvertes.extend(sondes.keys().cloned());
        if let Some(id) = ligne_album {
            let _ = album_repo.update_track_count(id);
        }
    }
}

/// Inventorie les dossiers porteurs de feuilles CUE **et écrit** ce qu'ils
/// décrivent dans la bibliothèque.
///
/// Rend l'inventaire à l'identique de [`super::cue_album::inventorier`] — le
/// rapport de scan ne change pas d'une clé —, le bilan de ce qui a réellement
/// changé en base, et **les fichiers image que le scan ordinaire doit laisser
/// tranquilles** : ils sont désormais représentés par leurs tranches.
pub fn inventorier_et_ecrire(
    db: Arc<dyn DbBackend>,
    dossiers: &[PathBuf],
) -> (InventaireCue, BilanCue, HashSet<PathBuf>) {
    let artist_repo = ArtistRepo::with_backend(db.clone());
    let album_repo = AlbumRepo::with_backend(db.clone());
    let track_repo = TrackRepo::with_backend(db.clone());
    let mut bilan = BilanCue::default();
    let mut images_couvertes: HashSet<PathBuf> = HashSet::new();

    let inventaire = inventorier_avec(dossiers, |dossier: &Path, plan: &PlanCue| {
        for album in &plan.albums {
            ecrire_album(
                dossier,
                album,
                &artist_repo,
                &album_repo,
                &track_repo,
                &mut bilan,
                &mut images_couvertes,
            );
        }
    });

    bilan.pistes_elaguees = elaguer_les_pistes_cue(&track_repo);

    if bilan != BilanCue::default() {
        info!(
            albums = bilan.albums,
            pistes_creees = bilan.pistes_creees,
            pistes_mises_a_jour = bilan.pistes_mises_a_jour,
            pistes_elaguees = bilan.pistes_elaguees,
            echecs = bilan.echecs,
            images_couvertes = images_couvertes.len(),
            "scan_cue_tracks_written"
        );
    }
    (inventaire, bilan, images_couvertes)
}

/// Retire les pistes virtuelles dont le fichier image a disparu.
///
/// ⚠️ **Un dossier absent n'est pas une image effacée.** Un NAS démonté, un
/// disque externe débranché, un partage SMB en panne rendent `exists()` faux
/// pour TOUTE la bibliothèque — et un élagage naïf effacerait des milliers de
/// pistes qu'un remontage aurait rendues. La suppression n'a donc lieu que si
/// le DOSSIER de l'image est toujours là et lisible : dans ce cas, et dans ce
/// cas seulement, l'absence du fichier est un fait sur le stockage, pas sur le
/// montage.
///
/// Rend le nombre de lignes supprimées.
pub fn elaguer_les_pistes_cue(track_repo: &TrackRepo) -> usize {
    let images = match track_repo.cue_media_paths() {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "cue_elagage_lecture_impossible");
            return 0;
        }
    };
    let mut dossiers_lisibles: std::collections::HashMap<PathBuf, bool> =
        std::collections::HashMap::new();
    let mut supprimees = 0usize;
    for image in images {
        let chemin = Path::new(&image);
        if chemin.exists() {
            continue;
        }
        let Some(dossier) = chemin.parent() else {
            continue;
        };
        let lisible = *dossiers_lisibles
            .entry(dossier.to_path_buf())
            .or_insert_with(|| std::fs::read_dir(dossier).is_ok());
        if !lisible {
            // Support absent : on ne touche à rien.
            continue;
        }
        match track_repo.delete_by_cue_media(&image) {
            Ok(n) => {
                if n > 0 {
                    info!(image = %image, pistes = n, "cue_pistes_elaguees_image_disparue");
                }
                supprimees += n as usize;
            }
            Err(e) => warn!(image = %image, error = %e, "cue_elagage_impossible"),
        }
    }
    supprimees
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::sqlite::SqliteDb;
    use std::fs;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        Arc::new(db)
    }

    /// Un vrai WAV court : le plan écarte une image qu'aucun décodeur ne lit,
    /// donc une fixture vide ne prouverait rien.
    fn ecrire_wav(chemin: &Path, millisecondes: u32) {
        const TAUX: u32 = 44_100;
        let trames = TAUX * millisecondes / 1000;
        let octets = trames * 4;
        let mut f = Vec::new();
        f.extend_from_slice(b"RIFF");
        f.extend_from_slice(&(36 + octets).to_le_bytes());
        f.extend_from_slice(b"WAVEfmt ");
        f.extend_from_slice(&16u32.to_le_bytes());
        f.extend_from_slice(&1u16.to_le_bytes());
        f.extend_from_slice(&2u16.to_le_bytes());
        f.extend_from_slice(&TAUX.to_le_bytes());
        f.extend_from_slice(&(TAUX * 4).to_le_bytes());
        f.extend_from_slice(&4u16.to_le_bytes());
        f.extend_from_slice(&16u16.to_le_bytes());
        f.extend_from_slice(b"data");
        f.extend_from_slice(&octets.to_le_bytes());
        for n in 0..trames {
            let v = ((n as f32 / 40.0).sin() * 8000.0) as i16;
            f.extend_from_slice(&v.to_le_bytes());
            f.extend_from_slice(&v.to_le_bytes());
        }
        fs::write(chemin, f).unwrap();
    }

    const FEUILLE: &str = "REM GENRE \"Classical\"\nREM DATE 1981\nPERFORMER \"Glenn Gould\"\nTITLE \"Goldberg Variations\"\nFILE \"image.wav\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Aria\"\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    TITLE \"Variatio 1\"\n    INDEX 01 00:01:00\n";

    /// Un album CUE simple, prêt à scanner. Rend le dossier et l'image.
    fn album_simple(racine: &Path) -> (PathBuf, PathBuf) {
        let d = racine.join("Gould - Goldberg");
        fs::create_dir_all(&d).unwrap();
        let image = d.join("image.wav");
        ecrire_wav(&image, 4_000);
        fs::write(d.join("album.cue"), FEUILLE).unwrap();
        (d, image)
    }

    /// LA MOITIÉ QUI PROUVE — le scan ÉCRIT.
    ///
    /// Avant #3631, `grep cue_media_path` ne rendait hors des tests de
    /// migration que du DDL : les `PisteCue` étaient construites, comptées,
    /// puis jetées. Ce témoin appelle la CONDUITE et relit la base.
    #[test]
    fn le_scan_ecrit_les_pistes_de_la_feuille_avec_leurs_bornes() {
        let d = tempfile::TempDir::new().unwrap();
        let (dossier, image) = album_simple(d.path());
        let db = base();

        let (inv, bilan, images) = inventorier_et_ecrire(db.clone(), &[dossier]);

        assert_eq!(inv.albums, 1);
        assert_eq!(inv.pistes, 2);
        assert_eq!(bilan.pistes_creees, 2, "bilan : {bilan:?}");
        assert_eq!(bilan.albums, 1);
        assert!(
            images.contains(&image),
            "le fichier image doit sortir du scan ordinaire, sinon l'album existe deux fois"
        );

        let repo = TrackRepo::with_backend(db);
        let media = image.to_string_lossy().to_string();
        let aria = repo
            .get_by_cue_identity(&media, 0)
            .unwrap()
            .expect("la piste 1 doit être en base");
        assert_eq!(aria.title, "Aria");
        assert_eq!(aria.track_number, 1);
        assert_eq!(aria.file_path, None, "une piste CUE n'a pas de file_path");
        assert_eq!(aria.cue_media_path.as_deref(), Some(media.as_str()));
        assert_eq!(aria.cue_start_ms, Some(0));
        assert_eq!(aria.cue_end_ms, Some(1_000));
        assert_eq!(aria.duration_ms, 1_000);
        assert_eq!(aria.year, Some(1981));
        assert_eq!(aria.genre.as_deref(), Some("Classical"));
        // Et la LECTURE sait s'en servir : c'est le couple que la résolution
        // de flux lit pour borner le décodage.
        assert_eq!(
            aria.bornes_cue(),
            Some((media.clone(), 0, Some(1_000))),
            "les bornes doivent ressortir de la BASE, pas seulement du plan"
        );

        // La DERNIÈRE piste n'a pas de fin dans la feuille : sa durée vient de
        // la durée sondée du fichier image (4 s), moins son début (1 s).
        let variatio = repo
            .get_by_cue_identity(&media, 1_000)
            .unwrap()
            .expect("la piste 2 doit être en base");
        assert_eq!(variatio.cue_end_ms, None);
        assert!(
            (2_900..=3_100).contains(&variatio.duration_ms),
            "durée de la dernière piste : {} ms, attendu ~3000",
            variatio.duration_ms
        );
    }

    /// LA CONTRE-ÉPREUVE — une feuille dont l'image manque n'écrit RIEN.
    ///
    /// C'est le cas des 649 `cue-image-introuvable` mesurés chez un testeur
    /// (#2060), et la règle nº1 de Gros Bidon (fil 1495). Sans cette moitié, le
    /// témoin ci-dessus passerait aussi pour un écrivain qui range tout ce
    /// qu'il voit — et la bibliothèque se remplirait d'albums injouables.
    #[test]
    fn une_feuille_orpheline_n_ecrit_aucune_piste() {
        let d = tempfile::TempDir::new().unwrap();
        let dossier = d.path().join("Perdu");
        fs::create_dir_all(&dossier).unwrap();
        fs::write(dossier.join("album.cue"), FEUILLE).unwrap(); // pas de .wav
        let db = base();

        let (inv, bilan, images) = inventorier_et_ecrire(db.clone(), &[dossier]);

        assert_eq!(inv.albums, 0);
        assert_eq!(inv.feuilles_ecartees, 1);
        assert_eq!(
            inv.ecarts_par_cle.get("cue-image-introuvable"),
            Some(&1),
            "le motif doit être NOMMÉ, pas seulement compté"
        );
        assert_eq!(bilan, BilanCue::default(), "rien ne doit être écrit");
        assert!(images.is_empty());
        assert_eq!(TrackRepo::with_backend(db).count().unwrap(), 0);
    }

    /// Deux scans ne doublent pas la bibliothèque.
    ///
    /// Les pistes CUE n'ont pas de `file_path` : le pré-filtre incrémental du
    /// scan ne peut pas les reconnaître, et l'index unique partiel
    /// `idx_tracks_cue_identity` refuserait une seconde insertion. C'est la
    /// relecture par `(cue_media_path, cue_start_ms)` qui tient.
    #[test]
    fn deux_scans_ne_doublent_pas_les_pistes() {
        let d = tempfile::TempDir::new().unwrap();
        let (dossier, _) = album_simple(d.path());
        let db = base();

        let (_, premier, _) = inventorier_et_ecrire(db.clone(), &[dossier.clone()]);
        let (_, second, _) = inventorier_et_ecrire(db.clone(), &[dossier]);

        assert_eq!(premier.pistes_creees, 2);
        assert_eq!(second.pistes_creees, 0, "second scan : {second:?}");
        assert_eq!(second.pistes_mises_a_jour, 2);
        assert_eq!(TrackRepo::with_backend(db).count().unwrap(), 2);
    }

    /// L'élagage dédié : le fichier image effacé emporte ses tranches.
    ///
    /// L'élagage ordinaire ne peut pas les voir (`file_path IS NULL`). Sans
    /// celui-ci, l'album resterait dans la bibliothèque à jamais, injouable.
    #[test]
    fn une_image_effacee_emporte_ses_tranches() {
        let d = tempfile::TempDir::new().unwrap();
        let (dossier, image) = album_simple(d.path());
        let db = base();
        inventorier_et_ecrire(db.clone(), &[dossier.clone()]);
        assert_eq!(TrackRepo::with_backend(db.clone()).count().unwrap(), 2);

        fs::remove_file(&image).unwrap();
        fs::remove_file(dossier.join("album.cue")).unwrap();
        let (_, bilan, _) = inventorier_et_ecrire(db.clone(), &[dossier]);

        assert_eq!(bilan.pistes_elaguees, 2, "bilan : {bilan:?}");
        assert_eq!(TrackRepo::with_backend(db).count().unwrap(), 0);
    }

    /// LA CONTRE-ÉPREUVE de l'élagage — un NAS démonté n'efface RIEN.
    ///
    /// `exists()` est faux pour toute la bibliothèque quand le support est
    /// absent. Un élagage naïf viderait des milliers de pistes qu'un remontage
    /// aurait rendues. La suppression n'a lieu que si le DOSSIER est là.
    #[test]
    fn un_support_absent_n_elague_rien() {
        let d = tempfile::TempDir::new().unwrap();
        let (dossier, _) = album_simple(d.path());
        let db = base();
        inventorier_et_ecrire(db.clone(), &[dossier.clone()]);
        let repo = TrackRepo::with_backend(db.clone());
        assert_eq!(repo.count().unwrap(), 2);

        // Le dossier ENTIER disparaît : c'est la signature d'un support
        // démonté, pas d'un fichier effacé.
        fs::remove_dir_all(&dossier).unwrap();

        assert_eq!(elaguer_les_pistes_cue(&repo), 0);
        assert_eq!(
            repo.count().unwrap(),
            2,
            "un support absent ne doit JAMAIS vider la bibliothèque"
        );
    }

    #[test]
    fn l_annee_se_lit_meme_sur_une_date_complete() {
        assert_eq!(annee_en_nombre("1981"), Some(1981));
        assert_eq!(annee_en_nombre("1981-03-12"), Some(1981));
        assert_eq!(annee_en_nombre("inconnue"), None);
        assert_eq!(annee_en_nombre(""), None);
    }
}
