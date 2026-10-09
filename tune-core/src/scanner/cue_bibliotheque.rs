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
//! 4. **Les balises du fichier image complètent la feuille, sans jamais la
//!    contredire** (#5463). La feuille reste la source première : son `TITLE`,
//!    ses `PERFORMER`, son `REM GENRE`, son `REM DATE` l'emportent. Mais le
//!    format CUE ne sait pas tout dire, et l'usage est de ranger le reste dans
//!    les balises du fichier qui l'accompagne — `DISCSUBTITLE` en tête (Gros
//!    Bidon, fil 2038). Voir [`completer_par_les_balises`].
//!
//! ## Ce qu'il ne fait pas
//!
//! Il ne prend pas au fichier image ce qui désigne UNE piste (titre,
//! interprète, numéro) : sur une image découpée, ces balises parlent du
//! fichier, pas de chaque tranche. Les albums écrits sont retenus dans le
//! bilan : l'appelant réévalue leurs pochettes — jaquette intégrée à l'image
//! comprise — avec le cache et la politique de sa passe (#5222).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tracing::{debug, info, warn};

use super::cue_album::{AlbumCue, InventaireCue, PisteCue, PlanCue, inventorier_avec};
use crate::db::album_metadata_repo::AlbumMetadataRepo;
use crate::db::album_repo::AlbumRepo;
use crate::db::artist_repo::ArtistRepo;
use crate::db::backend::DbBackend;
use crate::db::edition_album::Tenues;
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
    /// Identités écrites par cette passe, dédupliquées pour ne relire la
    /// pochette qu'une fois par album, et jamais hors du périmètre parcouru.
    albums_ecrits: HashSet<i64>,
    /// Albums pour lesquels au moins une piste a été posée ou rafraîchie.
    pub albums: usize,
    /// Pistes virtuelles créées par ce scan.
    pub pistes_creees: usize,
    /// Pistes virtuelles déjà présentes, rafraîchies en place.
    pub pistes_mises_a_jour: usize,
    /// Pistes virtuelles supprimées parce que leur fichier image a disparu.
    pub pistes_elaguees: usize,
    /// Doublons résorbés : un même fichier portait DEUX lignes — une posée par
    /// le scan ordinaire (avec `file_path`), une posée par la feuille (sans).
    /// La seconde est retirée au profit de la première. C'est l'état mesuré le
    /// 24/09/2026 sur le serveur .18, où 58 titres existaient deux fois.
    pub doublons_resorbes: usize,
    /// Albums dont le titre a été réconcilié depuis le `TITLE` de la feuille.
    ///
    /// Compté à part des créations : un album CUE d'avant la 0.9.144 est titré
    /// du nom de son FLAC, et rien ne le corrigeait. Ce compteur dit combien de
    /// lignes un scan a effectivement RENOMMÉES.
    pub titres_corriges: usize,
    /// Tranches retirées par la confrontation du scan complet : la feuille,
    /// retouchée ou supprimée, ne les décrit plus (#5108).
    pub tranches_retirees: usize,
    /// Tranches que la confrontation aurait retirées, mais que le plafond de
    /// purge a refusées : rien n'a été retiré (#5108).
    pub confrontation_refusee: usize,
    /// Écritures refusées par la base. Jamais fatales : une feuille bancale ne
    /// doit pas emporter le scan.
    pub echecs: usize,
}

impl BilanCue {
    /// Les images CUE sortent de l'import ordinaire : elles doivent néanmoins
    /// suivre la même priorité jaquette intégrée > image du dossier (#5222).
    /// Appeler après écriture des tranches, afin que `cue_media_path` soit
    /// visible à la relecture de l'album. La règle commune protège notamment
    /// les pochettes téléversées et les fournisseurs lors des passes rapides.
    pub fn reevaluer_pochettes(&self, db: &Arc<dyn DbBackend>, cache_dir: &Path, complet: bool) {
        for &album in &self.albums_ecrits {
            crate::library::pochette_disque::reevaluer_l_album(db, album, cache_dir, complet, None);
        }
    }
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
    // #5298 — une image de CD brute n'a pas d'en-tête : sa taille EST sa
    // durée, et son format est celui du CD audio, par définition.
    if crate::audio::image_cdda::est_image_cdda(image) {
        use crate::audio::image_cdda::{CADENCE, CANAUX, PROFONDEUR, duree_ms};
        return SondeImage {
            duree_ms: duree_ms(image),
            sample_rate: Some(CADENCE as i32),
            bit_depth: Some(PROFONDEUR as i32),
            channels: Some(CANAUX as i32),
        };
    }
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

/// Ce que les balises du fichier image apprennent, une seule fois par image
/// (#5463).
///
/// Seuls des champs que le lecteur tire des BALISES, jamais du chemin : le
/// titre, l'interprète et l'album de [`crate::metadata::TrackMetadata`] peuvent
/// être fabriqués depuis le nom de fichier ou de dossier, et ne sont donc pas
/// retenus ici — la feuille les porte de toute façon.
#[derive(Debug, Clone, Default, PartialEq)]
struct BalisesImage {
    // Valent pour l'ALBUM : elles décrivent toutes les tranches de l'image.
    nom_du_disque: Option<String>,
    genre: Option<String>,
    genres: Vec<String>,
    annee: Option<i32>,
    label: Option<String>,
    compositeur: Option<String>,
    // Désignent UN enregistrement : réservées à la piste qui occupe le
    // fichier entier.
    isrc: Option<String>,
    mbid_enregistrement: Option<String>,
    bpm: Option<f64>,
    commentaire: Option<String>,
}

impl BalisesImage {
    fn depuis(m: crate::metadata::TrackMetadata) -> Self {
        // Une trame présente mais vide vaut « je ne sais pas » : elle ne doit
        // pas passer pour une valeur qui comble un trou.
        let texte = |s: Option<String>| s.filter(|s| !s.trim().is_empty());
        BalisesImage {
            nom_du_disque: texte(m.disc_subtitle),
            genre: texte(m.genre),
            genres: m.genres,
            annee: m.year.map(|a| a as i32).filter(|a| *a > 0),
            label: texte(m.label),
            compositeur: texte(
                m.credits
                    .into_iter()
                    .find(|c| c.role == "composer")
                    .map(|c| c.name),
            ),
            isrc: texte(m.isrc),
            mbid_enregistrement: texte(m.musicbrainz_recording_id),
            bpm: m.bpm.filter(|b| *b > 0.0),
            commentaire: texte(m.comment),
        }
    }
}

/// Lit les balises du fichier image. Muet sur une image de CD brute, qui n'en
/// porte pas (#5298), et sur un fichier illisible : la feuille suffit alors.
fn lire_balises_image(image: &Path) -> BalisesImage {
    if crate::audio::image_cdda::est_image_cdda(image) {
        return BalisesImage::default();
    }
    match crate::metadata::try_read_metadata(image) {
        Ok(m) => BalisesImage::depuis(m),
        Err(e) => {
            debug!(image = %image.display(), error = %e, "cue_balises_image_non_lues");
            BalisesImage::default()
        }
    }
}

/// Complète une ligne CUE par les balises de son fichier image (#5463).
///
/// 🔴 **La feuille prime ; le fichier ne comble que ce qu'elle laisse vide.**
/// Un champ que [`piste_en_ligne`] a rempli depuis la feuille n'est jamais
/// touché. C'est la règle arrêtée pour #5463 : le format CUE ne porte ni nom de
/// disque, ni label, ni compositeur d'album, et l'usage — Gros Bidon, fil
/// 2038 — est de les mettre dans les balises du FLAC qui accompagne la feuille.
/// Tune les ignorait : `DISCSUBTITLE = Remastered Album` n'arrivait jamais en
/// base.
///
/// Deux portées :
///
/// - ce qui vaut pour l'album (nom du disque, genre, année, label,
///   compositeur) complète TOUTES les tranches de l'image ;
/// - ce qui désigne un enregistrement (ISRC, MBID d'enregistrement, tempo,
///   commentaire) ne complète que la piste qui occupe le fichier ENTIER. Sur
///   une image découpée, un ISRC du fichier recopié sur quinze tranches
///   fabriquerait quinze fausses identités.
fn completer_par_les_balises(t: &mut Track, b: &BalisesImage, fichier_entier: bool) {
    fn combler<T: Clone>(champ: &mut Option<T>, valeur: &Option<T>) {
        if champ.is_none() {
            champ.clone_from(valeur);
        }
    }
    combler(&mut t.disc_subtitle, &b.nom_du_disque);
    if t.genre.is_none() && b.genre.is_some() {
        t.genre.clone_from(&b.genre);
        // La liste suit le genre retenu : jamais une liste du fichier sous le
        // genre de la feuille.
        if !b.genres.is_empty() {
            t.genres = serde_json::to_string(&b.genres).ok();
        }
    }
    combler(&mut t.year, &b.annee);
    combler(&mut t.label, &b.label);
    combler(&mut t.composer, &b.compositeur);
    if fichier_entier {
        combler(&mut t.isrc, &b.isrc);
        combler(&mut t.musicbrainz_recording_id, &b.mbid_enregistrement);
        combler(&mut t.bpm, &b.bpm);
        combler(&mut t.comments, &b.commentaire);
    }
}

/// Cette piste occupe-t-elle un fichier ENTIER, à elle seule ?
///
/// Vrai quand elle est la seule tranche de son fichier, qu'elle démarre à zéro
/// et qu'aucune fin ne la borne : il ne reste alors plus rien du fichier
/// autour d'elle. C'est le cas de toute feuille « gapless » (un `FILE` par
/// piste), et c'est ce qui autorise à lui poser un `file_path` — voir
/// [`piste_en_ligne`].
///
/// ⛔ Jamais pour une image de CD brute (#5298). Le scan ordinaire ne voit pas
/// un `.bin` : lui poser un `file_path` le mettrait hors de ses fichiers
/// découverts, et la purge de fin de scan effacerait la piste qu'on vient
/// d'écrire. Toutes les passes par chemin (balises, ReplayGain) le prendraient
/// en outre pour un fichier audio ordinaire.
fn occupe_le_fichier_entier(piste: &PisteCue, tranches_du_fichier: usize) -> bool {
    tranches_du_fichier == 1
        && piste.debut_ms == 0
        && piste.fin_ms.is_none()
        && !crate::audio::image_cdda::est_image_cdda(&piste.media)
}

/// Combien de tranches chaque fichier image porte, dans cet album.
fn tranches_par_fichier(album: &AlbumCue) -> std::collections::HashMap<&Path, usize> {
    let mut par_fichier = std::collections::HashMap::new();
    for piste in &album.pistes {
        *par_fichier.entry(piste.media.as_path()).or_insert(0) += 1;
    }
    par_fichier
}

/// La ligne `tracks` d'une piste virtuelle.
///
/// `file_path` reste `NULL` **quand la piste est une tranche** : c'est ce qui
/// autorise N pistes sur le même fichier sous la contrainte `UNIQUE` de
/// `tracks.file_path`.
///
/// 🔴 **Mais une piste qui occupe un fichier ENTIER n'est pas une tranche.**
/// Une feuille « gapless » décrit un fichier par piste : chaque ligne a
/// `cue_start_ms = 0`, aucune fin, et un fichier pour elle seule. Lui refuser
/// son `file_path` la rendait introuvable PAR CHEMIN — et c'est de là que
/// viennent les deux symptômes opposés mesurés sur les serveurs de Bertrand :
///
/// - la ligne CUE est invisible au pré-filtre incrémental du scan (qui
///   s'indexe sur `file_path`), donc le fichier peut être réindexé à part et
///   le même titre existe DEUX fois ;
/// - toutes les passes qui filtrent `file_path IS NOT NULL` (ReplayGain,
///   paroles, écriture de balises) l'ignorent, alors que le fichier entier est
///   exactement ce qu'elles savent traiter.
///
/// `file_mtime` et `file_size` l'accompagnent : sans eux, `file_needs_scan`
/// conclurait « modifié » et le scan ordinaire relirait les balises du fichier
/// par-dessus le titre de la feuille, à chaque passage.
fn piste_en_ligne(
    piste: &PisteCue,
    album: &AlbumCue,
    album_id: Option<i64>,
    artist_id: Option<i64>,
    sonde: SondeImage,
    fichier_entier: bool,
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
    t.file_path = fichier_entier.then(|| piste.media.to_string_lossy().into_owned());
    if fichier_entier && let Ok(meta) = std::fs::metadata(&piste.media) {
        t.file_size = Some(meta.len() as i64);
        t.file_mtime = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64());
    }
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
    // #5463 — ce que la feuille dit de la piste, puis de l'album. Posés ICI,
    // avant `completer_par_les_balises` : la feuille prime sur le fichier.
    t.isrc = piste.isrc.clone();
    t.composer = piste
        .compositeur
        .clone()
        .or_else(|| album.compositeur.clone());
    t.comments = piste
        .commentaire
        .clone()
        .or_else(|| album.commentaire.clone());
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

/// Reprend, sur la ligne déjà en base, ce qu'une feuille CUE ne sait pas dire.
///
/// Une feuille porte des titres, des interprètes, un genre, une année. Elle ne
/// porte NI empreinte audio, NI identifiant MusicBrainz, NI ISRC, NI tempo, NI
/// pochette : tout cela vient de l'analyse du fichier et des passes
/// d'enrichissement. Écraser la ligne avec une piste neuve les effacerait à
/// chaque scan — et c'est exactement ce qui guette maintenant qu'une piste
/// CUE peut ADOPTER une ligne posée par le scan ordinaire, laquelle est
/// souvent enrichie depuis des mois.
///
/// On ne reprend jamais un champ que la feuille a rempli : le `TITLE` de la
/// feuille reste le titre, c'est toute la raison d'être du module. Ni un champ
/// que les balises de l'image viennent de combler ([`completer_par_les_balises`],
/// appelé AVANT) : une balise présente gagne, comme au scan ordinaire.
fn reprendre_l_acquis(neuve: &mut Track, existante: &Track) {
    if neuve.disc_subtitle.is_none() {
        neuve.disc_subtitle = existante.disc_subtitle.clone();
    }
    if neuve.audio_hash.is_none() {
        neuve.audio_hash = existante.audio_hash.clone();
    }
    if neuve.musicbrainz_recording_id.is_none() {
        neuve.musicbrainz_recording_id = existante.musicbrainz_recording_id.clone();
    }
    if neuve.isrc.is_none() {
        neuve.isrc = existante.isrc.clone();
    }
    if neuve.composer.is_none() {
        neuve.composer = existante.composer.clone();
    }
    if neuve.bpm.is_none() {
        neuve.bpm = existante.bpm;
    }
    if neuve.label.is_none() {
        neuve.label = existante.label.clone();
    }
    if neuve.comments.is_none() {
        neuve.comments = existante.comments.clone();
    }
    if neuve.cover_path.is_none() {
        neuve.cover_path = existante.cover_path.clone();
    }
    if neuve.file_mtime.is_none() {
        neuve.file_mtime = existante.file_mtime;
    }
    if neuve.file_size.is_none() {
        neuve.file_size = existante.file_size;
    }
    if neuve.duration_ms <= 0 {
        neuve.duration_ms = existante.duration_ms;
    }
}

/// `REM DATE` porte parfois une date complète (`1981-03-12`) ou du bruit.
fn annee_en_nombre(brut: &str) -> Option<i32> {
    let chiffres: String = brut.chars().take_while(|c| c.is_ascii_digit()).collect();
    chiffres.parse::<i32>().ok().filter(|a| *a > 0)
}

/// Ce dont [`ecrire_album`] a besoin, construit une fois par passe.
struct Depots {
    artistes: ArtistRepo,
    albums: AlbumRepo,
    pistes: TrackRepo,
    metadonnees: AlbumMetadataRepo,
    /// Ce que l'utilisateur a tenu à la main (#5319).
    tenues: Tenues,
}

impl Depots {
    fn pour(db: &Arc<dyn DbBackend>) -> Self {
        Self {
            artistes: ArtistRepo::with_backend(db.clone()),
            albums: AlbumRepo::with_backend(db.clone()),
            pistes: TrackRepo::with_backend(db.clone()),
            metadonnees: AlbumMetadataRepo::with_backend(db.clone()),
            // Une lecture par passe, comme au scan ordinaire (`TrackImporter`).
            tenues: Tenues::charger(db),
        }
    }
}

/// Ce qu'un album qui ne vient pas d'une feuille CUE impose à ses lignes.
///
/// Une image SACD (#5297) décrit elle-même ses propriétés audio : la sonde
/// `lofty` n'y lit rien, et le format rangé doit être celui que la lecture
/// reconnaît (`dsd`), pas l'extension `iso`. `None` pour une feuille CUE : le
/// chemin d'avant, inchangé.
struct Imposition<'a> {
    sonde: SondeImage,
    retoucher: &'a dyn Fn(&PisteCue, &mut Track),
}

/// Écrit un album CUE : artiste, album, puis ses pistes virtuelles.
fn ecrire_album(
    dossier: &Path,
    album: &AlbumCue,
    depots: &Depots,
    bilan: &mut BilanCue,
    images_couvertes: &mut HashSet<PathBuf>,
) {
    ecrire_album_avec(dossier, album, depots, bilan, images_couvertes, None);
}

/// [`ecrire_album`], avec ce qu'impose une source autre qu'une feuille.
fn ecrire_album_avec(
    dossier: &Path,
    album: &AlbumCue,
    depots: &Depots,
    bilan: &mut BilanCue,
    images_couvertes: &mut HashSet<PathBuf>,
    imposition: Option<&Imposition<'_>>,
) {
    let Depots {
        artistes: artist_repo,
        albums: album_repo,
        pistes: track_repo,
        metadonnees: tenues_meta,
        tenues,
    } = depots;
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
    // Les balises de chaque image, lues une fois (#5463). Pas pour une image
    // SACD : son Master TOC a déjà tout dit, et lofty n'y lit rien.
    let mut balises: HashMap<PathBuf, BalisesImage> = HashMap::new();
    if imposition.is_none() {
        for piste in &album.pistes {
            if !balises.contains_key(&piste.media) {
                balises.insert(piste.media.clone(), lire_balises_image(&piste.media));
            }
        }
    }
    let annee = album
        .annee
        .as_deref()
        .and_then(annee_en_nombre)
        .or_else(|| {
            album
                .pistes
                .first()
                .and_then(|p| balises.get(&p.media))
                .and_then(|b| b.annee)
        });
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
    //
    // 🔴 #5319 — SAUF sur un album que l'utilisateur a composé ou disposé à la
    // main (un coffret réunissant plusieurs feuilles : la feuille de CE
    // dossier ne décrit plus l'album entier), ni sur un titre qu'il a
    // lui-même tenu (`edition_manuelle`, la règle de
    // `AlbumRepo::realigner_sur_les_balises`).
    //
    // Fil 2094 — ni sur un coffret AUTOMATIQUE : ses disques sont tenus et
    // restent dans le coffret à la relecture, la passe ne le reforme donc
    // plus, et ne lui rendrait plus son titre.
    let titre_tenu = |id: i64| {
        tenues.albums_disposes().contains(&id)
            || tenues.est_un_coffret_auto(id)
            || tenues_meta
                .champs_edites_a_la_main(id)
                .unwrap_or_default()
                .iter()
                .any(|c| c == "title")
    };
    if let (Some(id), Some(titre_feuille)) = (ligne_album, album.titre.as_deref())
        && !titre_tenu(id)
    {
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

    // #5463 — `CATALOG` : le code-barres du disque. La feuille prime, comme
    // pour le titre ; aucune autre source du scan ne le pose.
    if let (Some(id), Some(code)) = (ligne_album, album.catalogue.as_deref()) {
        let code = code.trim();
        if !code.is_empty()
            && ligne.barcode.as_deref() != Some(code)
            && let Err(e) = album_repo.force_update_barcode(id, code)
        {
            warn!(album_id = id, error = %e, "cue_code_barres_non_ecrit");
        }
    }

    // Une sonde par IMAGE, pas par piste : un vinyle en deux faces sonde deux
    // fichiers pour dix pistes.
    let mut sondes: std::collections::HashMap<PathBuf, SondeImage> =
        std::collections::HashMap::new();
    let mut ecrites = 0usize;
    // Les albums que les tenues désignent à la place de celui du dossier : un
    // disque rattaché à la main à un coffret y reste (#5319).
    let mut albums_tenus: HashSet<i64> = HashSet::new();
    let tranches = tranches_par_fichier(album);
    // Les fichiers réellement DÉCOUPÉS — eux seuls doivent sortir du scan
    // ordinaire. Un fichier occupé en entier par une seule piste garde son
    // `file_path` : il reste vu par le scan, qui le reconnaîtra inchangé et
    // passera son chemin. L'en retirer le ferait au contraire sortir de
    // `discovered_paths`, et la purge effacerait la ligne qu'on vient d'écrire.
    let mut images_decoupees: HashSet<PathBuf> = HashSet::new();

    for piste in &album.pistes {
        let sonde = match imposition {
            Some(i) => i.sonde,
            None => *sondes
                .entry(piste.media.clone())
                .or_insert_with(|| sonder_image(&piste.media)),
        };
        let fichier_entier = occupe_le_fichier_entier(
            piste,
            tranches.get(piste.media.as_path()).copied().unwrap_or(1),
        );
        if !fichier_entier {
            images_decoupees.insert(piste.media.clone());
        }
        // Le `PERFORMER` d'une piste — le CD-Text d'un disque à plusieurs
        // interprètes — est SON artiste ; l'album garde le sien (#5298). Sans
        // cela, le soliste invité d'une piste disparaissait derrière
        // l'interprète de l'album.
        let artiste_de_la_piste = piste
            .interprete
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty() && *n != nom_artiste)
            .and_then(|n| artist_repo.get_or_create(n, None, None).ok())
            .and_then(|a| a.id)
            .or(artist_id);
        let mut ligne = piste_en_ligne(
            piste,
            album,
            ligne_album,
            artiste_de_la_piste,
            sonde,
            fichier_entier,
        );
        if let Some(i) = imposition {
            (i.retoucher)(piste, &mut ligne);
        }
        if let Some(b) = balises.get(&piste.media) {
            completer_par_les_balises(&mut ligne, b, fichier_entier);
        }
        // L'édition manuelle prime sur la feuille, comme sur les balises au
        // scan ordinaire (`TrackImporter::import`) : album, disque, numéro,
        // titre et artiste que l'utilisateur a tenus, par chemin ou par
        // identité CUE (#5319).
        if tenues.appliquer(&mut ligne)
            && let Some(tenu) = ligne.album_id
            && Some(tenu) != ligne_album
        {
            albums_tenus.insert(tenu);
        }
        let media = ligne.cue_media_path.clone().unwrap_or_default();
        let debut = ligne.cue_start_ms.unwrap_or(0);

        // DEUX identités peuvent désigner cette piste, et elles peuvent
        // désigner DEUX lignes distinctes.
        //
        // - `(cue_media_path, cue_start_ms)` retrouve ce qu'un scan précédent
        //   a posé depuis la feuille ;
        // - `file_path` retrouve la ligne que le scan ordinaire avait créée
        //   pour ce même fichier, avant que la feuille ne soit lue.
        //
        // Quand les deux répondent et qu'il s'agit de deux lignes, c'est
        // exactement le doublon mesuré sur le serveur .18 : le même titre en
        // double, une fois par chemin et une fois par feuille, invisibles l'un
        // à l'autre. On garde CELLE QUI PORTE LE CHEMIN — c'est elle que
        // protège `file_path UNIQUE`, elle que le pré-filtre incrémental voit,
        // et elle qui porte l'enrichissement accumulé — et on retire l'autre.
        // Sans ce choix, poser `file_path` sur la ligne CUE se ferait refuser
        // par la contrainte et la piste serait perdue en silence.
        let par_cue = match track_repo.get_by_cue_identity(&media, debut) {
            Ok(v) => v,
            Err(e) => {
                warn!(media = %media, debut, error = %e, "cue_identite_illisible");
                bilan.echecs += 1;
                continue;
            }
        };
        let par_chemin = if fichier_entier {
            track_repo.get_by_path(&media).unwrap_or(None)
        } else {
            None
        };
        let deja_la = match (par_chemin, par_cue) {
            (Some(chemin), Some(cue)) if chemin.id != cue.id => {
                if let Some(id) = cue.id
                    && track_repo.delete(id).is_ok()
                {
                    bilan.doublons_resorbes += 1;
                    info!(
                        media = %media,
                        supprimee = id,
                        gardee = ?chemin.id,
                        "cue_doublon_resorbe — la ligne sans chemin est retirée au profit de celle qui en porte un"
                    );
                }
                Some(chemin)
            }
            (Some(chemin), _) => Some(chemin),
            (None, cue) => cue,
        };

        match deja_la {
            Some(existante) => {
                ligne.id = existante.id;
                reprendre_l_acquis(&mut ligne, &existante);
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
            None => match track_repo.create(&ligne) {
                Ok(_) => {
                    bilan.pistes_creees += 1;
                    ecrites += 1;
                }
                Err(e) => {
                    warn!(media = %media, debut, error = %e, "cue_piste_non_creee");
                    bilan.echecs += 1;
                }
            },
        }
    }

    if ecrites > 0 {
        bilan.albums += 1;
        // Les fichiers image de cet album sont désormais REPRÉSENTÉS par leurs
        // tranches : le scan ordinaire ne doit plus les indexer comme des
        // pistes à part entière, sinon le même disque existe deux fois — une
        // piste « image entière » de 74 minutes à côté de ses 15 tranches.
        images_couvertes.extend(images_decoupees);
        if let Some(id) = ligne_album {
            bilan.albums_ecrits.insert(id);
            let _ = album_repo.update_track_count(id);
        }
        for id in albums_tenus {
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
    inventorier_et_ecrire_avec(db, dossiers, racines, None)
}

/// Ce que le scan complet confie à la confrontation des feuilles (#5108).
pub struct ConfrontationDuScan<'a> {
    /// Les fichiers audio que CE parcours a vus, avant tout retrait des images
    /// découpées. Un dossier dont la feuille a disparu n'est confronté que si
    /// l'une de ses images y figure : c'est la preuve que le parcours l'a lu.
    /// Un dossier hors du scan ciblé, exclu ou sur un support absent n'y est
    /// pas, et rien n'y est touché.
    pub fichiers_vus: &'a [PathBuf],
    /// Le plafond de la purge du scan, `purge_trop_massive(candidats, examinées)` :
    /// vrai ⇒ la confrontation ne retire RIEN et le dit.
    pub trop_massive: &'a dyn Fn(usize, usize) -> bool,
}

/// [`inventorier_et_ecrire`], plus la confrontation de la base aux feuilles
/// relues : celle du surveillant ([`relire_le_dossier`]), appliquée à chaque
/// dossier que le parcours a vu (#5108, suite de #5073).
///
/// - Feuille retouchée (INDEX déplacé, piste retirée) : la tranche qu'elle ne
///   décrit plus est retirée.
/// - Feuille supprimée : les tranches de l'image partent. L'image, restée
///   dans le parcours et absente des images couvertes, est réimportée entière
///   par le scan ordinaire. L'album n'existe plus deux fois.
///
/// La ligne « image entière » d'un fichier découpé n'est pas retirée ici : la
/// purge de fin du scan s'en charge, sous ses propres gardes.
pub fn inventorier_ecrire_et_confronter(
    db: Arc<dyn DbBackend>,
    dossiers: &[PathBuf],
    racines: &[String],
    confrontation: &ConfrontationDuScan<'_>,
) -> (InventaireCue, BilanCue, HashSet<PathBuf>) {
    inventorier_et_ecrire_avec(db, dossiers, racines, Some(confrontation))
}

fn inventorier_et_ecrire_avec(
    db: Arc<dyn DbBackend>,
    dossiers: &[PathBuf],
    racines: &[String],
    confrontation: Option<&ConfrontationDuScan<'_>>,
) -> (InventaireCue, BilanCue, HashSet<PathBuf>) {
    let mut relues: HashMap<PathBuf, FeuillesRelues> = HashMap::new();
    let (inventaire, mut bilan, images_couvertes) =
        ecrire_les_dossiers(&db, dossiers, |dossier, plan| {
            if confrontation.is_some() {
                relues.entry(dossier.to_path_buf()).or_default().noter(plan);
            }
        });

    let track_repo = TrackRepo::with_backend(db.clone());
    bilan.pistes_elaguees = elaguer_les_pistes_cue(&track_repo, racines);
    // Après l'élagage : une image disparue relève de ses gardes de support
    // absent (#1943), pas de la confrontation.
    if let Some(c) = confrontation {
        confronter_au_scan(&track_repo, dossiers, &relues, c, &mut bilan);
    }

    if bilan != BilanCue::default() {
        info!(
            albums = bilan.albums,
            pistes_creees = bilan.pistes_creees,
            pistes_mises_a_jour = bilan.pistes_mises_a_jour,
            pistes_elaguees = bilan.pistes_elaguees,
            doublons_resorbes = bilan.doublons_resorbes,
            tranches_retirees = bilan.tranches_retirees,
            confrontation_refusee = bilan.confrontation_refusee,
            echecs = bilan.echecs,
            images_couvertes = images_couvertes.len(),
            "scan_cue_tracks_written"
        );
    }
    (inventaire, bilan, images_couvertes)
}

/// Confronte, au scan complet, chaque dossier vu qui porte des tranches en
/// base. Rien n'est retiré si le total dépasse le plafond de purge.
fn confronter_au_scan(
    track_repo: &TrackRepo,
    dossiers_avec_feuille: &[PathBuf],
    relues: &HashMap<PathBuf, FeuillesRelues>,
    c: &ConfrontationDuScan<'_>,
    bilan: &mut BilanCue,
) {
    let images: Vec<String> = match track_repo.cue_media_paths() {
        Ok(v) => v.into_iter().filter(|m| !tranche_d_image_sacd(m)).collect(),
        Err(e) => {
            warn!(error = %e, "cue_confrontation_base_illisible");
            return;
        }
    };
    if images.is_empty() {
        return;
    }
    let mut par_dossier: std::collections::BTreeMap<PathBuf, Vec<String>> =
        std::collections::BTreeMap::new();
    for image in images {
        if let Some(parent) = Path::new(&image).parent() {
            par_dossier
                .entry(parent.to_path_buf())
                .or_default()
                .push(image);
        }
    }
    let vus: HashSet<&Path> = c.fichiers_vus.iter().map(PathBuf::as_path).collect();
    let avec_feuille: HashSet<&Path> = dossiers_avec_feuille.iter().map(PathBuf::as_path).collect();
    let aucune_feuille = FeuillesRelues::default();

    let mut a_retirer: Vec<i64> = Vec::new();
    let mut examinees = 0usize;
    for (dossier, images_avant) in &par_dossier {
        let vu = avec_feuille.contains(dossier.as_path())
            || images_avant.iter().any(|i| vus.contains(Path::new(i)));
        if !vu {
            continue;
        }
        let feuilles = relues.get(dossier).unwrap_or(&aucune_feuille);
        if let Some(conf) = confronter_le_dossier(track_repo, dossier, images_avant, feuilles) {
            examinees += conf.examinees;
            a_retirer.extend(conf.a_retirer);
        }
    }
    if a_retirer.is_empty() {
        return;
    }
    if (c.trop_massive)(a_retirer.len(), examinees) {
        warn!(
            candidats = a_retirer.len(),
            examinees,
            "cue_confrontation_refusee — trop de tranches à retirer d'un coup, rien n'est retiré"
        );
        bilan.confrontation_refusee = a_retirer.len();
        return;
    }
    for id in a_retirer {
        if track_repo.delete(id).is_ok() {
            bilan.tranches_retirees += 1;
        }
    }
    info!(
        tranches = bilan.tranches_retirees,
        examinees, "cue_tranches_retirees — les feuilles ne les décrivent plus"
    );
}

/// Le cœur commun du scan et du surveillant : planifier chaque dossier et
/// écrire ses albums par [`ecrire_album`]. `observer` voit chaque plan écrit.
fn ecrire_les_dossiers(
    db: &Arc<dyn DbBackend>,
    dossiers: &[PathBuf],
    mut observer: impl FnMut(&Path, &PlanCue),
) -> (InventaireCue, BilanCue, HashSet<PathBuf>) {
    let depots = Depots::pour(db);
    let mut bilan = BilanCue::default();
    let mut images_couvertes: HashSet<PathBuf> = HashSet::new();

    let inventaire = inventorier_avec(dossiers, |dossier: &Path, plan: &PlanCue| {
        for album in &plan.albums {
            ecrire_album(dossier, album, &depots, &mut bilan, &mut images_couvertes);
        }
        observer(dossier, plan);
    });
    (inventaire, bilan, images_couvertes)
}

/// L'identité d'album d'une image SACD, et son numéro de disque.
///
/// Une image = un album : le dossier ne suffit pas, plusieurs ISO se rangent
/// souvent côte à côte, et les fondre en un album les mélangerait. L'identité
/// est donc le CHEMIN de l'image. Exception : le disque d'un coffret
/// (`album_set_size` > 1 dans le Master TOC) rejoint ses frères du même
/// dossier, sous le numéro de disque que le Master TOC lui donne.
fn identite_de_l_album_iso(iso: &crate::audio::sacd::IsoSacdLu) -> (PathBuf, i32) {
    let d = &iso.disque;
    match iso.chemin.parent() {
        Some(parent) if d.disques_dans_l_album > 1 && d.rang_dans_l_album > 0 => {
            (parent.to_path_buf(), i32::from(d.rang_dans_l_album))
        }
        _ => (iso.chemin.clone(), 1),
    }
}

/// L'album d'une image SACD, dans la forme que l'écriture CUE range.
fn album_de_l_iso(
    iso: &crate::audio::sacd::IsoSacdLu,
    zone: &crate::audio::sacd::ZoneSacd,
) -> AlbumCue {
    let d = &iso.disque;
    let coffret = d.disques_dans_l_album > 1;
    let titre = if coffret {
        d.album_titre.clone().or_else(|| d.disque_titre.clone())
    } else {
        d.titre().map(str::to_string)
    }
    .or_else(|| {
        iso.chemin
            .file_stem()
            .and_then(|n| n.to_str())
            .map(str::to_string)
    });
    AlbumCue {
        feuilles: vec![iso.chemin.clone()],
        titre,
        interprete: d.artiste().map(str::to_string),
        genre: None,
        annee: d.annee.map(|a| a.to_string()),
        compositeur: None,
        catalogue: None,
        commentaire: None,
        pistes: zone
            .pistes
            .iter()
            .map(|p| PisteCue {
                media: iso.chemin.clone(),
                numero: p.numero,
                titre: p.titre.clone(),
                interprete: p.interprete.clone(),
                isrc: None,
                compositeur: None,
                commentaire: None,
                // La piste est une tranche de la zone, sur son horloge :
                // c'est ce que la lecture rejoue (`audio::sacd::ouvrir_lecture`).
                debut_ms: p.debut_ms(),
                fin_ms: Some(p.fin_ms()),
            })
            .collect(),
    }
}

/// Écrit les albums des images SACD que le parcours a lues nativement (#5297).
///
/// Chaque piste devient une TRANCHE de l'image, exactement comme une piste de
/// feuille CUE : `file_path = NULL`, `cue_media_path` = l'image,
/// `cue_start_ms`/`cue_end_ms` = la piste sur l'horloge de la zone lue. Le
/// même écrivain range les deux : identité `(image, début)`, reprise de
/// l'acquis d'enrichissement, élagage quand l'image disparaît. Rien n'est
/// extrait, rien n'est copié.
pub fn ecrire_les_iso_sacd(
    db: &Arc<dyn DbBackend>,
    isos: &[crate::audio::sacd::IsoSacdLu],
    bilan: &mut BilanCue,
) {
    if isos.is_empty() {
        return;
    }
    // Les tenues aussi : une image SACD est désignée par `(image, début)`
    // comme une tranche de feuille (#5319).
    let depots = Depots::pour(db);
    let mut couvertes: HashSet<PathBuf> = HashSet::new();
    let avant = bilan.pistes_creees + bilan.pistes_mises_a_jour;
    for iso in isos {
        let Some(zone) = iso.disque.zone_de_lecture() else {
            continue;
        };
        let album = album_de_l_iso(iso, zone);
        let (identite, disque) = identite_de_l_album_iso(iso);
        let compositeurs: HashMap<u32, Option<String>> = zone
            .pistes
            .iter()
            .map(|p| (p.numero, p.compositeur.clone()))
            .collect();
        let canaux = i32::from(zone.canaux);
        let retoucher = move |piste: &PisteCue, t: &mut Track| {
            t.format = Some("dsd".into());
            t.disc_number = disque;
            if t.composer.is_none() {
                t.composer = compositeurs.get(&piste.numero).cloned().flatten();
            }
            t.channels = canaux;
        };
        let imposition = Imposition {
            sonde: SondeImage {
                duree_ms: None,
                sample_rate: Some(crate::audio::sacd::FREQUENCE_DSD64 as i32),
                bit_depth: Some(1),
                channels: Some(canaux),
            },
            retoucher: &retoucher,
        };
        ecrire_album_avec(
            &identite,
            &album,
            &depots,
            bilan,
            &mut couvertes,
            Some(&imposition),
        );
        debug!(
            iso = %iso.chemin.display(),
            zone = zone.genre.as_str(),
            pistes = zone.pistes.len(),
            "iso_sacd_album_ecrit"
        );
    }
    info!(
        images = isos.len(),
        pistes = bilan.pistes_creees + bilan.pistes_mises_a_jour - avant,
        "scan_iso_sacd_natif — images SACD lues sans outil externe"
    );
}

/// Une tranche posée par une image SACD, et non par une feuille CUE.
///
/// La confrontation aux feuilles (#5108) retire les tranches qu'AUCUNE feuille
/// ne décrit : appliquée à une image SACD, elle effacerait l'album à chaque
/// scan d'un dossier qui porte aussi un `.cue`. Ces tranches-là ne relèvent
/// que de l'écriture des images et de l'élagage des images disparues.
fn tranche_d_image_sacd(media: &str) -> bool {
    crate::audio::sacd::est_extension_iso(Path::new(media))
}

/// Ce que [`relire_le_dossier`] a changé dans la bibliothèque (#5073).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RelectureDuDossier {
    /// Les fichiers image que les feuilles du dossier DÉCOUPENT : leurs
    /// tranches les représentent, le surveillant ne doit plus les importer en
    /// piste entière. C'est l'`images_cue` du scan.
    pub images_decoupees: HashSet<PathBuf>,
    /// Les fichiers image qu'aucune feuille ne décrit plus (feuille supprimée,
    /// ou qui désigne un autre fichier) : leurs tranches sont retirées, ils
    /// redeviennent des pistes ordinaires, à réimporter.
    pub images_liberees: Vec<PathBuf>,
    /// Ce que l'écriture des albums a changé, comme au scan.
    pub bilan: BilanCue,
    /// Tranches que la feuille ne décrit plus, retirées.
    pub tranches_retirees: usize,
    /// Pistes « image entière » retirées : le fichier est désormais découpé.
    pub pistes_entieres_retirees: usize,
}

/// Ce que les feuilles relues d'un dossier décrivent : le surveillant
/// ([`relire_le_dossier`]) et le scan complet le notent de la même façon.
#[derive(Debug, Default)]
struct FeuillesRelues {
    /// Les tranches décrites, en `(cue_media_path, cue_start_ms)`.
    decrites: HashSet<(String, i64)>,
    /// Un plan non vide a été lu pour ce dossier.
    vue: bool,
    /// Une feuille est présente mais illisible (en cours d'écriture, droits).
    illisible: bool,
}

impl FeuillesRelues {
    fn noter(&mut self, plan: &PlanCue) {
        use super::cue_album::MotifEcart;
        self.vue = true;
        for album in &plan.albums {
            for piste in &album.pistes {
                self.decrites.insert((
                    piste.media.to_string_lossy().into_owned(),
                    piste.debut_ms as i64,
                ));
            }
        }
        self.illisible |= plan
            .ecartees
            .iter()
            .any(|(_, motif)| matches!(motif, MotifEcart::Illisible(_)));
    }
}

/// Ce que la confrontation d'un dossier retirerait. Elle ne supprime rien.
#[derive(Debug, Default)]
struct ConfrontationDuDossier {
    /// Les tranches que les feuilles ne décrivent plus.
    a_retirer: Vec<i64>,
    /// Toutes les tranches examinées : le dénominateur du plafond de purge.
    examinees: usize,
    /// Les images dont plus aucune tranche n'est décrite.
    images_liberees: Vec<PathBuf>,
}

/// Confronte les tranches en base des `images_avant` de ce dossier à ce que
/// ses feuilles relues décrivent. Le cœur commun du surveillant et du scan.
///
/// ⚠️ Rend `None`, donc rien à retirer, sur un doute : dossier illisible
/// (#1943), feuille présente mais illisible, ou aucune feuille lue alors que
/// le dossier en porte encore une (plan vide, dossier au-delà du plafond
/// d'inventaire).
fn confronter_le_dossier(
    track_repo: &TrackRepo,
    dossier: &Path,
    images_avant: &[String],
    feuilles: &FeuillesRelues,
) -> Option<ConfrontationDuDossier> {
    if std::fs::read_dir(dossier).is_err() {
        return None;
    }
    if feuilles.illisible {
        warn!(dossier = %dossier.display(), "cue_relecture_feuille_illisible — aucune tranche retirée");
        return None;
    }
    // Aucune feuille vue : seulement si le dossier se lit ET n'en porte
    // vraiment plus — `planifier_dossier` rend aussi un plan vide sur un
    // dossier devenu illisible entre-temps.
    if !feuilles.vue && !dossier_lisible_sans_feuille(dossier) {
        return None;
    }
    let mut conf = ConfrontationDuDossier::default();
    for media in images_avant {
        let tranches = match track_repo.tranches_cue_du_media(media) {
            Ok(t) => t,
            Err(e) => {
                warn!(media = %media, error = %e, "cue_relecture_tranches_illisibles");
                continue;
            }
        };
        let mut gardees = 0usize;
        for (id, debut) in tranches {
            conf.examinees += 1;
            if feuilles.decrites.contains(&(media.clone(), debut)) {
                gardees += 1;
            } else {
                conf.a_retirer.push(id);
            }
        }
        if gardees == 0 && Path::new(media).is_file() {
            info!(image = %media, "cue_image_liberee — plus aucune feuille ne la découpe");
            conf.images_liberees.push(PathBuf::from(media));
        }
    }
    Some(conf)
}

/// Le dossier ne porte, à cet instant, aucune feuille `.cue` — et il se lit.
fn dossier_lisible_sans_feuille(dossier: &Path) -> bool {
    let Ok(entrees) = std::fs::read_dir(dossier) else {
        return false;
    };
    !entrees.flatten().any(|e| {
        e.path()
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("cue"))
    })
}

/// #5073 (Gros Bidon, fil 1904) — le surveillant relit UN dossier dont une
/// feuille CUE ou un fichier audio vient de changer, par le MÊME découpage que
/// le scan ([`planifier_dossier`](super::cue_album::planifier_dossier) puis
/// `ecrire_album`).
///
/// Le surveillant importait chaque FLAC seul : un album « image + feuille »
/// déposé Tune lancé devenait UNE piste de 40 minutes, et le `.cue` n'était
/// jamais lu hors d'une analyse complète.
///
/// En plus de ce que fait le scan, cette relecture confronte la base à la
/// feuille relue, pour ce seul dossier :
///
/// - la ligne « image entière » d'un fichier désormais découpé est retirée —
///   le scan la retire par sa purge de fin, qui n'existe pas ici (feuille
///   arrivée APRÈS le FLAC) ;
/// - une tranche que la feuille ne décrit plus (feuille retouchée) est retirée ;
/// - un fichier qu'aucune feuille ne décrit plus (feuille supprimée) perd ses
///   tranches et revient dans [`RelectureDuDossier::images_liberees`], pour
///   être réimporté comme une piste ordinaire.
///
/// ⚠️ Rien n'est retiré sur un doute : dossier illisible (support absent, cf.
/// #1943), ou feuille présente mais illisible à cet instant (en cours
/// d'écriture, droits). L'élagage de toute la bibliothèque
/// ([`elaguer_les_pistes_cue`]) reste l'affaire du scan : il sonderait chaque
/// image de chaque album CUE à chaque événement.
pub fn relire_le_dossier(db: &Arc<dyn DbBackend>, dossier: &Path) -> RelectureDuDossier {
    let mut relecture = RelectureDuDossier::default();
    if std::fs::read_dir(dossier).is_err() {
        return relecture;
    }
    let track_repo = TrackRepo::with_backend(db.clone());
    // Les images de CE dossier que la base découpe déjà, avant relecture.
    let images_avant: Vec<String> = match track_repo.cue_media_paths() {
        Ok(v) => v
            .into_iter()
            .filter(|m| Path::new(m).parent() == Some(dossier) && !tranche_d_image_sacd(m))
            .collect(),
        Err(e) => {
            warn!(dossier = %dossier.display(), error = %e, "cue_relecture_base_illisible");
            return relecture;
        }
    };

    let mut feuilles = FeuillesRelues::default();
    let (_, bilan, images_decoupees) =
        ecrire_les_dossiers(db, &[dossier.to_path_buf()], |_, plan| feuilles.noter(plan));
    relecture.bilan = bilan;

    // Le fichier découpé perd sa ligne « image entière » : sans quoi l'album
    // existerait deux fois, 40 minutes d'un bloc à côté de ses tranches.
    for image in &images_decoupees {
        let chemin = image.to_string_lossy();
        if let Ok(Some(entiere)) = track_repo.get_by_path(&chemin)
            && entiere.cue_media_path.is_none()
            && let Some(id) = entiere.id
            && track_repo.delete(id).is_ok()
        {
            relecture.pistes_entieres_retirees += 1;
            info!(image = %chemin, piste = id, "cue_piste_entiere_remplacee_par_ses_tranches");
        }
    }
    relecture.images_decoupees = images_decoupees;

    let Some(conf) = confronter_le_dossier(&track_repo, dossier, &images_avant, &feuilles) else {
        return relecture;
    };
    for id in conf.a_retirer {
        if track_repo.delete(id).is_ok() {
            relecture.tranches_retirees += 1;
        }
    }
    relecture.images_liberees = conf.images_liberees;
    if relecture.tranches_retirees > 0 {
        info!(
            dossier = %dossier.display(),
            tranches = relecture.tranches_retirees,
            "cue_tranches_retirees — la feuille ne les décrit plus"
        );
    }
    relecture
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

    /// #5108 — une base de FICHIER : sur `:memory:`, le pool de lecture voit
    /// ce qu'une base réelle ne verrait pas.
    fn base_fichier(d: &Path) -> Arc<dyn DbBackend> {
        let db = SqliteDb::open(&d.join("tune-5108.db").to_string_lossy()).unwrap();
        db.init_schema().unwrap();
        Arc::new(db)
    }

    fn debuts(db: &Arc<dyn DbBackend>, image: &Path) -> Vec<i64> {
        let mut v: Vec<i64> = TrackRepo::with_backend(db.clone())
            .tranches_cue_du_media(&image.to_string_lossy())
            .unwrap()
            .into_iter()
            .map(|(_, debut)| debut)
            .collect();
        v.sort();
        v
    }

    fn jamais(_: usize, _: usize) -> bool {
        false
    }

    fn toujours(_: usize, _: usize) -> bool {
        true
    }

    /// #5108 — la confrontation du scan : feuille retouchée ⇒ l'ancienne
    /// tranche part ; le plafond de purge refuse ⇒ RIEN ne part, et le bilan
    /// le dit.
    #[test]
    fn le_scan_confronte_la_feuille_retouchee_sous_le_plafond_5108() {
        let d = tempfile::TempDir::new().unwrap();
        let (dossier, image) = album_simple(d.path());
        let db = base_fichier(d.path());
        inventorier_et_ecrire(db.clone(), &[dossier.clone()], &racines(d.path()));
        assert_eq!(debuts(&db, &image), vec![0, 1_000], "montage");

        fs::write(
            dossier.join("album.cue"),
            FEUILLE.replace("INDEX 01 00:01:00", "INDEX 01 00:02:00"),
        )
        .unwrap();
        let vus = vec![image.clone()];
        let (_, refuse, _) = inventorier_ecrire_et_confronter(
            db.clone(),
            &[dossier.clone()],
            &racines(d.path()),
            &ConfrontationDuScan {
                fichiers_vus: &vus,
                trop_massive: &toujours,
            },
        );
        assert_eq!(refuse.confrontation_refusee, 1, "bilan : {refuse:?}");
        assert_eq!(refuse.tranches_retirees, 0);
        assert_eq!(
            debuts(&db, &image),
            vec![0, 1_000, 2_000],
            "plafond atteint : l'ancienne tranche reste"
        );

        let (_, bilan, _) = inventorier_ecrire_et_confronter(
            db.clone(),
            &[dossier],
            &racines(d.path()),
            &ConfrontationDuScan {
                fichiers_vus: &vus,
                trop_massive: &jamais,
            },
        );
        assert_eq!(bilan.tranches_retirees, 1, "bilan : {bilan:?}");
        assert_eq!(debuts(&db, &image), vec![0, 2_000]);
    }

    /// #5108 — feuille supprimée : le dossier n'est confronté que si le
    /// parcours a vu son image. Hors du parcours (scan ciblé ailleurs, dossier
    /// exclu), rien n'est touché.
    #[test]
    fn la_feuille_supprimee_n_est_confrontee_que_si_le_parcours_a_vu_l_image_5108() {
        let d = tempfile::TempDir::new().unwrap();
        let (dossier, image) = album_simple(d.path());
        let db = base_fichier(d.path());
        inventorier_et_ecrire(db.clone(), &[dossier.clone()], &racines(d.path()));
        fs::remove_file(dossier.join("album.cue")).unwrap();

        let rien: Vec<PathBuf> = Vec::new();
        let (_, bilan, _) = inventorier_ecrire_et_confronter(
            db.clone(),
            &[],
            &racines(d.path()),
            &ConfrontationDuScan {
                fichiers_vus: &rien,
                trop_massive: &jamais,
            },
        );
        assert_eq!(bilan.tranches_retirees, 0);
        assert_eq!(
            debuts(&db, &image),
            vec![0, 1_000],
            "dossier non parcouru : intact"
        );

        let vus = vec![image.clone()];
        let (_, bilan, images) = inventorier_ecrire_et_confronter(
            db.clone(),
            &[],
            &racines(d.path()),
            &ConfrontationDuScan {
                fichiers_vus: &vus,
                trop_massive: &jamais,
            },
        );
        assert_eq!(bilan.tranches_retirees, 2, "bilan : {bilan:?}");
        assert!(debuts(&db, &image).is_empty());
        assert!(
            images.is_empty(),
            "l'image n'est plus couverte : le scan la réimporte"
        );
    }

    #[test]
    fn l_annee_se_lit_meme_sur_une_date_complete() {
        assert_eq!(annee_en_nombre("1981"), Some(1981));
        assert_eq!(annee_en_nombre("1981-03-12"), Some(1981));
        assert_eq!(annee_en_nombre("inconnue"), None);
        assert_eq!(annee_en_nombre(""), None);
    }

    /// Un vrai FLAC (le gabarit du dépôt, 1 s), étiqueté comme le fait Mp3tag.
    fn flac_etiquete(chemin: &Path, balises: &[(&str, &str)]) {
        use lofty::config::{ParseOptions, WriteOptions};
        use lofty::file::AudioFile;
        use lofty::flac::FlacFile;
        use lofty::ogg::VorbisComments;
        let gabarit = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test.flac");
        fs::copy(&gabarit, chemin).unwrap();
        let mut fh = fs::File::open(chemin).unwrap();
        let mut flac = FlacFile::read_from(&mut fh, ParseOptions::new()).unwrap();
        drop(fh);
        let mut vc = VorbisComments::default();
        for (cle, valeur) in balises {
            vc.insert((*cle).to_string(), (*valeur).to_string());
        }
        flac.set_vorbis_comments(vc);
        flac.save_to_path(chemin, WriteOptions::default()).unwrap();
    }

    /// Les balises du FLAC du fil 2038, et de quoi prouver qu'elles ne
    /// délogent rien : un titre et un interprète qui ne sont PAS ceux de la
    /// feuille, un ISRC qui ne désigne qu'un enregistrement.
    const BALISES_DU_FLAC: &[(&str, &str)] = &[
        ("DISCSUBTITLE", "Remastered Album"),
        ("GENRE", "Progressive Rock"),
        ("DATE", "2015"),
        ("LABEL", "A&M Records"),
        ("COMPOSER", "Rick Wakeman"),
        ("ISRC", "GBAAA1500001"),
        ("TITLE", "Titre du fichier"),
        ("ARTIST", "Artiste du fichier"),
    ];

    /// Écrit un dossier CUE sur un FLAC étiqueté, le scanne, rend les pistes
    /// dans l'ordre de leurs débuts.
    fn scanner_flac_et_feuille(feuille: &str) -> Vec<Track> {
        let d = tempfile::TempDir::new().unwrap();
        let dossier = d.path().join("Rick Wakeman - The Six Wives");
        fs::create_dir_all(&dossier).unwrap();
        let image = dossier.join("image.flac");
        flac_etiquete(&image, BALISES_DU_FLAC);
        fs::write(dossier.join("album.cue"), feuille).unwrap();
        let db = base();
        let (_, bilan, _) = inventorier_et_ecrire(db.clone(), &[dossier], &racines(d.path()));
        assert_eq!(bilan.echecs, 0, "bilan : {bilan:?}");
        let repo = TrackRepo::with_backend(db);
        let media = image.to_string_lossy().to_string();
        let mut pistes: Vec<Track> = repo
            .tranches_cue_du_media(&media)
            .unwrap()
            .into_iter()
            .filter_map(|(_, debut)| repo.get_by_cue_identity(&media, debut).unwrap())
            .collect();
        pistes.sort_by_key(|t| t.cue_start_ms);
        pistes
    }

    const DEUX_TRANCHES_SANS_REM: &str = "PERFORMER \"Rick Wakeman\"\nTITLE \"The Six Wives of Henry VIII\"\nFILE \"image.flac\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Catherine of Aragon\"\n    INDEX 01 00:00:00\n  TRACK 02 AUDIO\n    TITLE \"Anne of Cleves\"\n    INDEX 01 00:00:37\n";

    /// 🔴 #5463 (Gros Bidon, fil 2038) — `DISCSUBTITLE`, que le format CUE ne
    /// sait pas porter, est dans le FLAC qui accompagne la feuille. Tune ne
    /// lisait jamais les balises de l'image : le nom du disque n'arrivait pas
    /// en base, ni le genre, l'année, le label ou le compositeur que la feuille
    /// ne dit pas.
    #[test]
    fn les_balises_de_l_image_completent_la_feuille_5463() {
        let pistes = scanner_flac_et_feuille(DEUX_TRANCHES_SANS_REM);
        assert_eq!(pistes.len(), 2, "pistes : {pistes:?}");
        for t in &pistes {
            assert_eq!(
                t.disc_subtitle.as_deref(),
                Some("Remastered Album"),
                "#5463 — le DISCSUBTITLE du FLAC doit compléter chaque tranche de la feuille"
            );
            assert_eq!(t.genre.as_deref(), Some("Progressive Rock"));
            assert_eq!(t.year, Some(2015));
            assert_eq!(t.label.as_deref(), Some("A&M Records"));
            assert_eq!(t.composer.as_deref(), Some("Rick Wakeman"));
            // Ce qui désigne UNE piste reste à la feuille…
            assert_eq!(t.artist_name.as_deref(), Some("Rick Wakeman"));
            // … et un identifiant d'enregistrement n'est pas recopié sur des
            // tranches : il n'en désignerait aucune.
            assert_eq!(
                t.isrc, None,
                "l'ISRC d'une image découpée ne vaut pour aucune de ses tranches"
            );
        }
        assert_eq!(pistes[0].title, "Catherine of Aragon");
        assert_eq!(pistes[1].title, "Anne of Cleves");
    }

    /// LA CONTRE-ÉPREUVE DE PRIORITÉ — ce que la feuille dit, le fichier ne le
    /// remplace pas. `REM GENRE` et `REM DATE` l'emportent sur `GENRE` et
    /// `DATE` du FLAC ; le nom du disque, que la feuille ne dit pas, vient
    /// toujours du fichier.
    #[test]
    fn la_feuille_prime_sur_les_balises_de_l_image_5463() {
        let feuille = format!("REM GENRE \"Rock\"\nREM DATE 1973\n{DEUX_TRANCHES_SANS_REM}");
        let pistes = scanner_flac_et_feuille(&feuille);
        assert_eq!(pistes.len(), 2, "pistes : {pistes:?}");
        for t in &pistes {
            assert_eq!(
                t.genre.as_deref(),
                Some("Rock"),
                "#5463 — le REM GENRE de la feuille doit primer sur le GENRE du fichier"
            );
            assert_eq!(
                t.year,
                Some(1973),
                "#5463 — le REM DATE de la feuille doit primer sur la DATE du fichier"
            );
            assert_eq!(
                t.genres, None,
                "une liste de genres du fichier sous le genre de la feuille"
            );
            assert_eq!(t.disc_subtitle.as_deref(), Some("Remastered Album"));
        }
    }

    /// Une piste qui occupe le fichier ENTIER est cet enregistrement : son
    /// ISRC lui revient, que la feuille n'a pas dit. Le titre reste celui de
    /// la feuille.
    #[test]
    fn la_piste_du_fichier_entier_prend_aussi_ses_identifiants_5463() {
        const UNE_PISTE: &str = "PERFORMER \"Rick Wakeman\"\nTITLE \"The Six Wives of Henry VIII\"\nFILE \"image.flac\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Catherine of Aragon\"\n    INDEX 01 00:00:00\n";
        let pistes = scanner_flac_et_feuille(UNE_PISTE);
        assert_eq!(pistes.len(), 1, "pistes : {pistes:?}");
        let t = &pistes[0];
        assert!(
            t.file_path.is_some(),
            "montage : la piste doit occuper le fichier entier"
        );
        assert_eq!(
            t.isrc.as_deref(),
            Some("GBAAA1500001"),
            "#5463 — la piste du fichier entier doit prendre l'ISRC de son fichier"
        );
        assert_eq!(t.title, "Catherine of Aragon");
        assert_eq!(t.disc_subtitle.as_deref(), Some("Remastered Album"));
    }

    /// #5463, suite — l'`ISRC`, le `SONGWRITER` et le `REM COMMENT` de la
    /// FEUILLE primaient en principe, mais l'analyseur les jetait : sur la
    /// piste du fichier entier, c'est l'ISRC et le compositeur du FICHIER qui
    /// passaient. La feuille dit, le fichier se tait.
    #[test]
    fn la_feuille_prime_sur_le_fichier_pour_isrc_et_compositeur_5463() {
        const FEUILLE_COMPLETE: &str = "REM COMMENT \"ExactAudioCopy v1.0b4\"\nPERFORMER \"Rick Wakeman\"\nTITLE \"The Six Wives of Henry VIII\"\nFILE \"image.flac\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"Catherine of Aragon\"\n    ISRC USAM17302204\n    SONGWRITER \"R. Wakeman\"\n    INDEX 01 00:00:00\n";
        let pistes = scanner_flac_et_feuille(FEUILLE_COMPLETE);
        assert_eq!(pistes.len(), 1, "pistes : {pistes:?}");
        let t = &pistes[0];
        assert!(
            t.file_path.is_some(),
            "montage : la piste occupe le fichier entier"
        );
        assert_eq!(
            t.isrc.as_deref(),
            Some("USAM17302204"),
            "#5463 — l'ISRC de la feuille doit primer sur celui du fichier"
        );
        assert_eq!(
            t.composer.as_deref(),
            Some("R. Wakeman"),
            "#5463 — le SONGWRITER de la feuille doit primer sur le COMPOSER du fichier"
        );
        assert_eq!(t.comments.as_deref(), Some("ExactAudioCopy v1.0b4"));
        // Ce que la feuille ne dit pas vient toujours du fichier.
        assert_eq!(t.label.as_deref(), Some("A&M Records"));
    }

    /// #5463 — `CATALOG` est le code-barres UPC/EAN du disque : il va sur
    /// l'album, et y reprend la main sur une valeur plus ancienne.
    #[test]
    fn le_catalog_de_la_feuille_devient_le_code_barres_de_l_album_5463() {
        let d = tempfile::TempDir::new().unwrap();
        let dossier = d.path().join("Rick Wakeman - The Six Wives");
        fs::create_dir_all(&dossier).unwrap();
        flac_etiquete(&dossier.join("image.flac"), BALISES_DU_FLAC);
        fs::write(
            dossier.join("album.cue"),
            format!("CATALOG 0600753562390\n{DEUX_TRANCHES_SANS_REM}"),
        )
        .unwrap();
        let db = base();
        inventorier_et_ecrire(
            db.clone(),
            std::slice::from_ref(&dossier),
            &racines(d.path()),
        );
        let media = dossier.join("image.flac").to_string_lossy().to_string();
        let album_id = TrackRepo::with_backend(db.clone())
            .get_by_cue_identity(&media, 0)
            .unwrap()
            .and_then(|t| t.album_id)
            .expect("la piste 1 et son album doivent exister");
        let album = AlbumRepo::with_backend(db).get(album_id).unwrap().unwrap();
        assert_eq!(
            album.barcode.as_deref(),
            Some("0600753562390"),
            "#5463 — le CATALOG de la feuille doit devenir le code-barres de l'album"
        );
    }

    /// #5463 — une feuille RETOUCHÉE (un ISRC ajouté) doit atteindre la base
    /// au rescan : la ligne existe déjà, c'est `TrackRepo::update` qui
    /// l'écrit — et il n'écrivait pas la colonne `isrc`.
    #[test]
    fn une_feuille_retouchee_pose_son_isrc_au_rescan_5463() {
        let d = tempfile::TempDir::new().unwrap();
        let dossier = d.path().join("Rick Wakeman - The Six Wives");
        fs::create_dir_all(&dossier).unwrap();
        let image = dossier.join("image.flac");
        flac_etiquete(&image, BALISES_DU_FLAC);
        let cue = dossier.join("album.cue");
        fs::write(&cue, DEUX_TRANCHES_SANS_REM).unwrap();
        let db = base();
        let dossiers = std::slice::from_ref(&dossier);
        inventorier_et_ecrire(db.clone(), dossiers, &racines(d.path()));

        fs::write(
            &cue,
            DEUX_TRANCHES_SANS_REM.replace(
                "    INDEX 01 00:00:00\n",
                "    ISRC USAM17302204\n    INDEX 01 00:00:00\n",
            ),
        )
        .unwrap();
        let (_, bilan, _) = inventorier_et_ecrire(db.clone(), dossiers, &racines(d.path()));
        assert_eq!(bilan.pistes_mises_a_jour, 2, "bilan : {bilan:?}");

        let repo = TrackRepo::with_backend(db);
        let media = image.to_string_lossy().to_string();
        let une = repo.get_by_cue_identity(&media, 0).unwrap().unwrap();
        assert_eq!(
            une.isrc.as_deref(),
            Some("USAM17302204"),
            "#5463 — l'ISRC ajouté à la feuille doit atteindre la ligne déjà en base"
        );
        let deux = repo.get_by_cue_identity(&media, 493).unwrap().unwrap();
        assert_eq!(
            deux.isrc, None,
            "l'ISRC d'une piste ne déborde pas sur l'autre"
        );
    }

    /// #5297 — une image SACD lue nativement devient un album de TRANCHES :
    /// titres, compositeurs, bornes et propriétés DSD viennent des sommaires
    /// du disque. Un second passage met à jour sans doubler, la confrontation
    /// aux feuilles CUE du même dossier ne la touche pas, et l'élagage la
    /// retire quand l'image disparaît.
    #[test]
    fn une_image_sacd_devient_un_album_de_tranches() {
        use crate::audio::sacd::fabrique::{ImageFabriquee, ecrire};
        let d = tempfile::TempDir::new().unwrap();
        let db = base_fichier(d.path());
        let dossier = d.path().join("Miles Davis");
        fs::create_dir_all(&dossier).unwrap();
        let iso = dossier.join("Kind of Blue.iso");
        let bornes = ecrire(&iso, &ImageFabriquee::deux_pistes());
        let isos = vec![crate::audio::sacd::IsoSacdLu {
            chemin: iso.clone(),
            disque: crate::audio::sacd::lire_disque(&iso).unwrap(),
        }];

        let mut bilan = BilanCue::default();
        ecrire_les_iso_sacd(&db, &isos, &mut bilan);
        assert_eq!((bilan.albums, bilan.pistes_creees, bilan.echecs), (1, 2, 0));

        let repo = TrackRepo::with_backend(db.clone());
        let media = iso.to_string_lossy().into_owned();
        let (debut, fin) = bornes[1];
        let debut_ms = crate::audio::sacd::trames_en_ms(debut);
        let fin_ms = crate::audio::sacd::trames_en_ms(fin);
        let t = repo
            .get_by_cue_identity(&media, debut_ms as i64)
            .unwrap()
            .expect("la piste 2 est une tranche de l'image");
        assert_eq!(t.title, "Freddie Freeloader");
        assert_eq!(t.track_number, 2);
        assert_eq!(t.album_title.as_deref(), Some("Kind of Blue"));
        assert_eq!(t.composer.as_deref(), Some("Miles Davis"));
        assert_eq!(t.year, Some(1959));
        assert_eq!(t.file_path, None, "une tranche n'a pas de chemin propre");
        assert_eq!(t.cue_end_ms, Some(fin_ms as i64));
        assert_eq!(t.duration_ms, (fin_ms - debut_ms) as i64);
        assert_eq!(t.format.as_deref(), Some("dsd"));
        assert_eq!(t.sample_rate, Some(2_822_400));
        assert_eq!(t.bit_depth, Some(1));
        assert_eq!(t.channels, 2);
        assert_eq!(
            crate::audio::formats::AudioFormat::from_extension(t.format.as_deref().unwrap()),
            Some(crate::audio::formats::AudioFormat::Dsd),
            "la lecture doit reconnaître le format rangé comme du DSD"
        );

        // Un second scan : mise à jour en place, aucun doublon.
        let mut bilan = BilanCue::default();
        ecrire_les_iso_sacd(&db, &isos, &mut bilan);
        assert_eq!((bilan.pistes_creees, bilan.pistes_mises_a_jour), (0, 2));
        assert_eq!(repo.count().unwrap(), 2);

        // Une feuille CUE dans le même dossier : la confrontation du
        // surveillant, puis celle du scan, ne retirent pas les tranches de
        // l'image, qu'aucune feuille ne décrit.
        let wav = dossier.join("image.wav");
        ecrire_wav(&wav, 4_000);
        fs::write(dossier.join("album.cue"), FEUILLE).unwrap();
        let relecture = relire_le_dossier(&db, &dossier);
        assert_eq!(relecture.tranches_retirees, 0);
        let (_, bilan, _) = inventorier_ecrire_et_confronter(
            db.clone(),
            std::slice::from_ref(&dossier),
            &racines(d.path()),
            &ConfrontationDuScan {
                fichiers_vus: std::slice::from_ref(&wav),
                trop_massive: &|_, _| false,
            },
        );
        assert_eq!(bilan.tranches_retirees, 0);
        assert_eq!(repo.tranches_cue_du_media(&media).unwrap().len(), 2);

        // L'image disparue : ses tranches partent avec elle.
        fs::remove_file(&iso).unwrap();
        assert_eq!(elaguer_les_pistes_cue(&repo, &racines(d.path())), 2);
        assert!(repo.tranches_cue_du_media(&media).unwrap().is_empty());
    }
}
