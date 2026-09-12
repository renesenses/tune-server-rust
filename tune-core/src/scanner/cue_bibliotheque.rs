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
    /// Albums dont le titre a été réconcilié depuis le `TITLE` de la feuille.
    ///
    /// Compté à part des créations : un album CUE d'avant la 0.9.144 est titré
    /// du nom de son FLAC, et rien ne le corrigeait. Ce compteur dit combien de
    /// lignes un scan a effectivement RENOMMÉES.
    pub titres_corriges: usize,
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
    let ligne = match (
        artist_id,
        album_repo.get_or_create_for_folder(
            &dossier.to_string_lossy(),
            &titre_album,
            artist_id.unwrap_or(0),
            annee,
            None,
        ),
    ) {
        (Some(_), Ok(a)) => a,
        (_, Err(e)) => {
            warn!(dossier = %dossier.display(), error = %e, "cue_album_non_ecrit");
            bilan.echecs += album.pistes.len();
            return;
        }
        (None, Ok(a)) => a,
    };
    let ligne_album = ligne.id;

    // 🔴 LE TITRE DE LA FEUILLE L'EMPORTE SUR CE QUI EST DÉJÀ EN BASE.
    //
    // `get_or_create_for_folder` identifie l'album par son DOSSIER. Quand la
    // ligne existe déjà, il la rend telle quelle : il ne réconcilie que
    // l'artiste (`reclaim_unknown_artist`), jamais le titre. Un album CUE
    // indexé AVANT la 0.9.144 avait été vu comme un unique gros FLAC, donc
    // titré du NOM DE CE FICHIER — et aucun rescan ne pouvait plus le corriger.
    //
    // Gros Bidon (Didier), fil forum 1738, le 09/09/2026 : « les feuilles CUE
    // sont lues et interprétées. Par contre le nom de l'album n'est pas mis à
    // jour et garde le nom du fichier FLAC. » Il a tout essayé — retirer les
    // albums, retirer le dossier, vider la bibliothèque — et seule la dernière
    // a marché, symptôme exact d'une ligne jamais réconciliée.
    //
    // On n'écrase QUE si la feuille porte un vrai `TITLE` : `titre_album`
    // retombe sinon sur le nom du dossier, et remplacer un titre par un repli
    // serait une régression.
    if let (Some(id), Some(titre_feuille)) = (ligne_album, album.titre.as_deref()) {
        let titre_feuille = titre_feuille.trim();
        if !titre_feuille.is_empty() && ligne.title != titre_feuille {
            match album_repo.force_update_title(id, titre_feuille) {
                Ok(()) => {
                    bilan.titres_corriges += 1;
                    info!(
                        album_id = id,
                        avant = %ligne.title,
                        apres = %titre_feuille,
                        "cue_titre_album_reconcilie"
                    );
                }
                Err(e) => warn!(album_id = id, error = %e, "cue_titre_album_non_ecrit"),
            }
        }
    }

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
    racines: &[String],
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

    bilan.pistes_elaguees = elaguer_les_pistes_cue(&track_repo, racines);

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
/// ⚠️ **Un support absent n'est pas une image effacée.** Un NAS démonté, un
/// disque externe débranché, un partage SMB en panne rendent `exists()` faux
/// pour TOUTE la bibliothèque — et un élagage naïf effacerait des milliers de
/// pistes qu'un remontage aurait rendues.
///
/// 🔴 **Le discriminant est le plus proche ANCÊTRE lisible, pas le dossier
/// parent.** La première version exigeait que le dossier DE L'IMAGE soit
/// lisible, et prenait donc « l'utilisateur a effacé le dossier de l'album »
/// pour « le montage a disparu » — les deux rendent `read_dir` fautif sur ce
/// dossier-là. Gros Bidon (Didier), fil forum 1738 le 09/09/2026 : « je retire
/// ces albums de ma bibliothèque et je refais une mise à jour complète.
/// Malheureusement l'album ne disparait pas. […] J'ai l'impression que la
/// suppression d'un fichier ou dossier n'est pas toujours vu par Tune. » Il a
/// dû vider la bibliothèque entière pour s'en sortir.
///
/// On remonte donc la chaîne des parents, **bornée à la racine déclarée** : si
/// un ancêtre répond, le stockage est là et l'absence est un fait réel ; si
/// aucun ne répond jusqu'à la racine, c'est le montage qui manque.
///
/// ⚠️ Ce qui N'EST PAS traité ici, volontairement : une image qui n'est plus
/// sous aucune racine déclarée. `verdict_purge` la classe `HorsPerimetre` et
/// REFUSE de la supprimer — protection née de #1943, où un point de montage
/// changé avait emporté 21 277 pistes de Yacine. Retirer un dossier des
/// emplacements déclarés ne vide donc pas la bibliothèque, et c'est voulu.
///
/// Rend le nombre de lignes supprimées.
/// Le stockage répond-il quelque part au-dessus de ce dossier ?
///
/// Rend `true` dès qu'un ancêtre — le dossier lui-même compris — se laisse
/// lire. C'est la seule chose qui sépare « ce dossier a été effacé » de « ce
/// montage n'est pas là » : dans le premier cas le parent répond, dans le
/// second plus rien ne répond jusqu'à la racine.
fn un_ancetre_est_lisible(dossier: &Path, racine: &Path) -> bool {
    let mut courant = Some(dossier);
    while let Some(d) = courant {
        if std::fs::read_dir(d).is_ok() {
            return true;
        }
        if d == racine {
            // 🔴 LA BORNE. Sans elle la remontée atteint `/`, toujours lisible
            // — et « le montage a disparu » deviendrait indiscernable de « le
            // fichier a été effacé », soit le défaut qu'on corrige, retourné.
            return false;
        }
        courant = d.parent();
    }
    false
}

pub fn elaguer_les_pistes_cue(track_repo: &TrackRepo, racines: &[String]) -> usize {
    let images = match track_repo.cue_media_paths() {
        Ok(v) => v,
        Err(e) => {
            warn!(error = %e, "cue_elagage_lecture_impossible");
            return 0;
        }
    };
    // Une liste de racines VIDE ne veut pas dire « tout est hors périmètre » :
    // elle veut dire qu'on ne sait rien. On ne supprime alors RIEN — même
    // raisonnement que `verdict_purge`, et même raison (#1943).
    if racines.is_empty() {
        return 0;
    }
    let mut dossiers_lisibles: std::collections::HashMap<PathBuf, bool> =
        std::collections::HashMap::new();
    let mut supprimees = 0usize;
    for image in images {
        let chemin = Path::new(&image);
        if chemin.exists() {
            continue;
        }
        // Hors de toute racine déclarée : `HorsPerimetre`. On ne supprime pas —
        // protection de #1943, et c'est ce qui fait que retirer un dossier des
        // emplacements ne vide pas la bibliothèque.
        let Some(racine) = racines
            .iter()
            .find(|r| crate::metadata::enrich_scope::sous_le_dossier(&image, r))
        else {
            continue;
        };
        let racine = Path::new(racine.trim_end_matches(['/', '\\']));
        let Some(dossier) = chemin.parent() else {
            continue;
        };
        let lisible = *dossiers_lisibles
            .entry(dossier.to_path_buf())
            .or_insert_with(|| un_ancetre_est_lisible(dossier, racine));
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

    /// La racine déclarée d'un test : le dossier temporaire lui-même.
    ///
    /// L'élagage borne sa remontée à la racine et refuse d'agir hors d'elle
    /// (#1943). Un test qui n'en passerait aucune n'élaguerait jamais rien —
    /// et serait vert sans rien prouver.
    fn racines(racine: &Path) -> Vec<String> {
        vec![racine.to_string_lossy().into_owned()]
    }

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

    /// 🔴 LE TITRE DE LA FEUILLE RÉCUPÈRE UN ALBUM TITRÉ DU NOM DU FLAC.
    ///
    /// Gros Bidon (Didier), fil forum 1738, 09/09/2026 : « Suite à la mise à
    /// jour 0.9.144 les feuilles CUE sont lues et interprétées. Par contre le
    /// nom de l'album n'est pas mis à jour et garde le nom du fichier FLAC. »
    ///
    /// C'est l'état de TOUTE bibliothèque montée avant la 0.9.144 : le gros
    /// FLAC avait été indexé comme un fichier ordinaire et l'album porte son
    /// nom. `get_or_create_for_folder` identifie l'album par son DOSSIER, donc
    /// il retrouvait cette ligne et la rendait telle quelle — ne réconciliant
    /// que l'artiste. Aucun rescan ne pouvait corriger le titre.
    ///
    /// Le témoin part de l'état d'AVANT, pas d'une base vierge : c'est tout
    /// son intérêt. Sur une base vierge le titre est bon du premier coup, et
    /// scanner deux fois de suite serait vert sans rien prouver.
    #[test]
    fn le_titre_de_la_feuille_remplace_le_nom_du_fichier() {
        let d = tempfile::TempDir::new().unwrap();
        let (dossier, _) = album_simple(d.path());
        let db = base();
        let album_repo = AlbumRepo::with_backend(db.clone());

        // `artist_id` doit désigner une VRAIE ligne : la contrainte de clé
        // étrangère refuse un 0 d'aisance, et le test échouerait avant même
        // d'atteindre ce qu'il mesure.
        let artiste = ArtistRepo::with_backend(db.clone())
            .get_or_create("Glenn Gould", None, None)
            .unwrap()
            .id
            .unwrap();

        // L'état d'AVANT la 0.9.144 : l'album existe, titré du nom du FLAC.
        let avant = album_repo
            .get_or_create_for_folder(&dossier.to_string_lossy(), "image", artiste, None, None)
            .unwrap();
        let id = avant.id.unwrap();
        assert_eq!(avant.title, "image");

        let (_, bilan, _) = inventorier_et_ecrire(db.clone(), &[dossier], &racines(d.path()));

        assert_eq!(bilan.titres_corriges, 1, "bilan : {bilan:?}");
        let apres = album_repo.get(id).unwrap().unwrap();
        assert_eq!(
            apres.title, "Goldberg Variations",
            "le titre de la feuille n'a pas repris la main sur le nom du fichier"
        );
        assert_eq!(
            apres.id,
            Some(id),
            "un album de plus a été créé au lieu du renommage"
        );
    }

    /// LA CONTRE-ÉPREUVE — une feuille SANS `TITLE` n'écrase rien.
    ///
    /// `titre_album` retombe alors sur le nom du dossier. Remplacer un titre
    /// existant par ce repli serait une régression : on ne renomme que sur un
    /// vrai `TITLE`.
    #[test]
    fn une_feuille_sans_titre_ne_renomme_pas_l_album() {
        const SANS_TITRE: &str = "PERFORMER \"Glenn Gould\"\nFILE \"image.wav\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Aria\"\n    INDEX 01 00:00:00\n";
        let d = tempfile::TempDir::new().unwrap();
        let dossier = d.path().join("Gould - Goldberg");
        fs::create_dir_all(&dossier).unwrap();
        ecrire_wav(&dossier.join("image.wav"), 4_000);
        fs::write(dossier.join("album.cue"), SANS_TITRE).unwrap();

        let db = base();
        let album_repo = AlbumRepo::with_backend(db.clone());
        let artiste = ArtistRepo::with_backend(db.clone())
            .get_or_create("Glenn Gould", None, None)
            .unwrap()
            .id
            .unwrap();
        let avant = album_repo
            .get_or_create_for_folder(
                &dossier.to_string_lossy(),
                "Un vrai titre",
                artiste,
                None,
                None,
            )
            .unwrap();
        let id = avant.id.unwrap();

        let (_, bilan, _) = inventorier_et_ecrire(db.clone(), &[dossier], &racines(d.path()));

        assert_eq!(bilan.titres_corriges, 0, "bilan : {bilan:?}");
        assert_eq!(
            album_repo.get(id).unwrap().unwrap().title,
            "Un vrai titre",
            "un repli sur le nom du dossier a écrasé un titre existant"
        );
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

        let (inv, bilan, images) =
            inventorier_et_ecrire(db.clone(), &[dossier], &racines(d.path()));

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

        let (inv, bilan, images) =
            inventorier_et_ecrire(db.clone(), &[dossier], &racines(d.path()));

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

        let (_, premier, _) =
            inventorier_et_ecrire(db.clone(), &[dossier.clone()], &racines(d.path()));
        let (_, second, _) = inventorier_et_ecrire(db.clone(), &[dossier], &racines(d.path()));

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
        inventorier_et_ecrire(db.clone(), &[dossier.clone()], &racines(d.path()));
        assert_eq!(TrackRepo::with_backend(db.clone()).count().unwrap(), 2);

        fs::remove_file(&image).unwrap();
        fs::remove_file(dossier.join("album.cue")).unwrap();
        let (_, bilan, _) = inventorier_et_ecrire(db.clone(), &[dossier], &racines(d.path()));

        assert_eq!(bilan.pistes_elaguees, 2, "bilan : {bilan:?}");
        assert_eq!(TrackRepo::with_backend(db).count().unwrap(), 0);
    }

    /// 🔴 CE TÉMOIN INSCRIVAIT LE DÉFAUT QU'IL CROYAIT GARDER.
    ///
    /// Il faisait `remove_dir_all(dossier)` — le dossier de l'ALBUM — et
    /// exigeait que rien ne soit élagué, au motif que « le dossier entier
    /// disparu = support démonté ». Or effacer un album, c'est exactement
    /// effacer son dossier : les deux rendaient `read_dir` fautif au même
    /// endroit, et le code ne pouvait pas les distinguer.
    ///
    /// C'est le défaut rapporté par Gros Bidon (Didier), fil 1738 le
    /// 09/09/2026 : ses albums CUE effacés du disque restaient en
    /// bibliothèque, et il a dû la vider entièrement. Un témoin vert pendant
    /// tout ce temps.
    #[test]
    fn un_dossier_dalbum_efface_emporte_ses_tranches() {
        let d = tempfile::TempDir::new().unwrap();
        let (dossier, _) = album_simple(d.path());
        let db = base();
        inventorier_et_ecrire(db.clone(), &[dossier.clone()], &racines(d.path()));
        let repo = TrackRepo::with_backend(db.clone());
        assert_eq!(repo.count().unwrap(), 2);

        // L'utilisateur efface l'album. La racine, elle, répond toujours.
        fs::remove_dir_all(&dossier).unwrap();

        assert_eq!(elaguer_les_pistes_cue(&repo, &racines(d.path())), 2);
        assert_eq!(
            repo.count().unwrap(),
            0,
            "un album effacé du disque doit quitter la bibliothèque"
        );
    }

    /// LA CONTRE-ÉPREUVE — un support démonté n'efface RIEN.
    ///
    /// Ici c'est la RACINE DÉCLARÉE qui disparaît, ce que fait un NAS démonté :
    /// plus rien ne répond jusqu'à elle. La remontée s'arrête à la borne.
    /// Sans cette borne elle atteindrait `/`, toujours lisible, et viderait la
    /// bibliothèque au premier démontage.
    #[test]
    fn un_support_absent_n_elague_rien() {
        let d = tempfile::TempDir::new().unwrap();
        let racine = d.path().join("Musique");
        fs::create_dir_all(&racine).unwrap();
        let (dossier, _) = album_simple(&racine);
        let db = base();
        let rac = vec![racine.to_string_lossy().into_owned()];
        inventorier_et_ecrire(db.clone(), &[dossier.clone()], &rac);
        let repo = TrackRepo::with_backend(db.clone());
        assert_eq!(repo.count().unwrap(), 2);

        // Le montage entier s'en va : la racine déclarée n'est plus là.
        fs::remove_dir_all(&racine).unwrap();

        assert_eq!(elaguer_les_pistes_cue(&repo, &rac), 0);
        assert_eq!(
            repo.count().unwrap(),
            2,
            "un support absent ne doit JAMAIS vider la bibliothèque"
        );
    }

    /// #1943 — hors périmètre n'est pas « disparu ».
    ///
    /// Retirer un dossier des emplacements déclarés ne supprime RIEN : c'est la
    /// protection née des 21 277 pistes de Yacine, effacées par un point de
    /// montage qui avait changé. Didier s'attendait au contraire (fil 1738) —
    /// c'est bien le comportement voulu, pas un défaut.
    #[test]
    fn un_dossier_retire_des_emplacements_ne_supprime_rien() {
        let d = tempfile::TempDir::new().unwrap();
        let (dossier, image) = album_simple(d.path());
        let db = base();
        inventorier_et_ecrire(db.clone(), &[dossier.clone()], &racines(d.path()));
        let repo = TrackRepo::with_backend(db.clone());
        assert_eq!(repo.count().unwrap(), 2);

        fs::remove_file(&image).unwrap();
        let ailleurs = vec![d.path().join("Autre").to_string_lossy().into_owned()];

        assert_eq!(elaguer_les_pistes_cue(&repo, &ailleurs), 0);
        assert_eq!(repo.count().unwrap(), 2, "hors périmètre = protégé");
    }

    /// Une liste de racines vide ne veut pas dire « tout est hors périmètre ».
    #[test]
    fn sans_racine_connue_on_ne_supprime_rien() {
        let d = tempfile::TempDir::new().unwrap();
        let (dossier, image) = album_simple(d.path());
        let db = base();
        inventorier_et_ecrire(db.clone(), &[dossier], &racines(d.path()));
        let repo = TrackRepo::with_backend(db.clone());
        fs::remove_file(&image).unwrap();
        assert_eq!(elaguer_les_pistes_cue(&repo, &[]), 0);
        assert_eq!(repo.count().unwrap(), 2);
    }

    #[test]
    fn l_annee_se_lit_meme_sur_une_date_complete() {
        assert_eq!(annee_en_nombre("1981"), Some(1981));
        assert_eq!(annee_en_nombre("1981-03-12"), Some(1981));
        assert_eq!(annee_en_nombre("inconnue"), None);
        assert_eq!(annee_en_nombre(""), None);
    }
}
