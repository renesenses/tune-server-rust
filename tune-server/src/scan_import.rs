//! Shared track-import helpers for the library scanners.
//!
//! Both the manual scan ([`crate::routes::system::scan`]) and the auto/startup +
//! watcher scans ([`crate::auto_scan`]) turn a [`ScannedFile`]'s
//! [`TrackMetadata`] into a DB [`Track`] row. This module holds the single
//! field-mapping they share so the three former copies cannot drift again — they
//! had already diverged: the manual *insert* path omitted `disc_subtitle`, and
//! the auto/watcher helper omitted `genres` and `composer`.
//!
//! Artist/album *resolution* still lives with each caller for now (it needs
//! batch-wide compilation context); this module owns only the per-file field
//! mapping, which every scan path shares verbatim.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use tune_core::db::album_repo::AlbumRepo;
use tune_core::db::artist_repo::ArtistRepo;
use tune_core::db::backend::DbBackend;
use tune_core::db::models::{Album, Artist, Track};
use tune_core::metadata::TrackMetadata;
use tune_core::scanner::walker::ScannedFile;

/// True when an `album_artist` value denotes a various-artists compilation.
pub(crate) fn is_various_artists(s: &str) -> bool {
    let l = s.trim().to_lowercase();
    l == "various artists" || l == "various" || l == "va" || l == "compilations"
}

/// Decide, per `(folder, album title)`, whether that album is a various-artists
/// compilation, from the metadata of a set of scanned tracks.
///
/// A genuine single-artist album has one consistent `album_artist`. An album is
/// treated as a compilation when any of its tracks carries the compilation flag
/// or a "Various Artists" album_artist, OR when the `album_artist` value varies
/// across the tracks of the same `(folder, album)` — the tell-tale of a
/// compilation whose tracks were each tagged with their own artist as the
/// album_artist, which otherwise splits into one album (and cover) per artist.
///
/// Keys are `(folder, album_title.to_lowercase())`.
///
/// ⚠️ N'ALIMENTER qu'avec des valeurs venues des BALISES. Un fichier dont les
/// tags n'ont pas pu être lus arrive avec un artiste déduit du nom de dossier
/// (`TrackMetadata::artist_from_path`) : compté ici, ce faux second artiste
/// bascule l'album entier en compilation (#3232). Le filtrage se fait chez
/// l'appelant, [`TrackImporter::begin_batch`].
pub(crate) fn decide_compilation_albums<'a>(
    items: impl Iterator<Item = (String, &'a str, Option<&'a str>, bool)>,
) -> HashMap<(String, String), bool> {
    let mut acc: HashMap<(String, String), (bool, HashSet<String>)> = HashMap::new();
    for (dir, album, album_artist, comp_flag) in items {
        let entry = acc.entry((dir, album.to_lowercase())).or_default();
        let aa = album_artist.map(|s| s.trim()).filter(|s| !s.is_empty());
        if comp_flag || aa.map(is_various_artists).unwrap_or(false) {
            entry.0 = true;
        }
        if let Some(aa) = aa {
            entry.1.insert(aa.to_lowercase());
        }
    }
    acc.into_iter()
        .map(|(k, (flag, artists))| (k, flag || artists.len() >= 2))
        .collect()
}

/// Per-FOLDER compilation decision, complementing [`decide_compilation_albums`]
/// (keyed by `(folder, album)`, which misses a hand-made compilation whose
/// tracks carry DIFFERENT album tags — each `(folder, album)` group then holds a
/// single track, so the "≥2 artists" tell-tale never fires; JP Borderies).
///
/// Returns `folder → (various_artists, use_folder_title)`:
/// - `various_artists`: the folder holds ≥2 distinct artists → album artist is
///   "Various Artists". A single-artist multi-disc folder (1 artist) is untouched.
/// - `use_folder_title`: additionally ≥2 distinct album tags → the per-track
///   album tags are unrelated, so the folder name is the real album title. A
///   genuine various-artists album with ONE album tag (e.g. "Woodstock") keeps
///   its title (only `decide_compilation_albums` flags it): here it stays false.
///
/// `items` yields `(folder, artist, album)`; `artist` = the album_artist tag if
/// present, else the track artist.
///
/// ⚠️ Même règle que [`decide_compilation_albums`] : rien qui ne vienne des
/// BALISES. Un fichier en repli « tout depuis le chemin » doit entrer avec
/// `(dossier, None, None)` — c'est LUI qui fabriquait le second artiste et
/// faisait sortir le tag « compilation » au hasard (#3232).
pub(crate) fn decide_compilation_folders<'a>(
    items: impl Iterator<Item = (String, Option<&'a str>, Option<&'a str>)>,
) -> HashMap<String, (bool, bool)> {
    let mut acc: HashMap<String, (HashSet<String>, HashSet<String>)> = HashMap::new();
    for (dir, artist, album) in items {
        let e = acc.entry(dir).or_default();
        if let Some(a) = artist.map(str::trim).filter(|s| !s.is_empty()) {
            e.0.insert(a.to_lowercase());
        }
        if let Some(al) = album.map(str::trim).filter(|s| !s.is_empty()) {
            e.1.insert(al.to_lowercase());
        }
    }
    acc.into_iter()
        .map(|(dir, (artists, albums))| {
            let va = artists.len() >= 2;
            (dir, (va, va && albums.len() >= 2))
        })
        .collect()
}

/// Les seules balises dont dépendent les deux décisions ci-dessus, gardées
/// pour un dossier pendant TOUTE la durée du scan.
///
/// 🔴 #3232 — `begin_batch` recalculait les décisions sur le seul lot courant.
/// [`tune_core::scanner::walker::lots_alignes_sur_les_dossiers`] a supprimé le
/// cas de l'album à cheval sur une frontière, mais il reste celui que ce
/// découpage ne peut PAS supprimer : un dossier qui contient à lui seul plus de
/// `SCAN_BATCH_SIZE` fichiers ne tient pas dans un lot — le lot porte les
/// pochettes embarquées, et les garder toutes est ce qui a fait sauter la
/// machine de JeromeQ. Ce dossier-là est encore coupé, et il était donc jugé
/// par morceaux : mesuré sur l'anthologie de Pierre M, « Various Artists,
/// compilation » en un lot, « Angela Brown, pas une compilation » en deux.
///
/// Le coût est en VALEURS DISTINCTES, pas en fichiers : un titre d'album et un
/// artiste par album du dossier, dédoublonnés. Une bibliothèque de 58 000
/// fichiers tient dans quelques centaines de kilooctets — sans commune mesure
/// avec les pochettes d'un seul lot.
#[derive(Default)]
pub(crate) struct PreuvesDuDossier {
    /// `(titre d'album, artiste d'album, drapeau compilation)` — ce que
    /// [`decide_compilation_albums`] consomme.
    par_album: HashSet<(String, Option<String>, bool)>,
    /// `(artiste, titre d'album)` — ce que [`decide_compilation_folders`]
    /// consomme. Un fichier sans balises entre en `(None, None)` : il fait
    /// savoir que le dossier existe sans y apporter d'artiste.
    par_dossier: HashSet<(Option<String>, Option<String>)>,
}

/// Ce que ce scan PRÉSENTE et ce qu'il ÉCARTE.
///
/// 🔴 #3528 — `files_to_scan` est pré-filtré des fichiers inchangés avant
/// d'arriver au découpage en lots. Le verdict « compilation » se prenait donc
/// sur la seule fraction du dossier réellement relue : sur l'anthologie de
/// Pierre M, modifier UN fichier et relancer un scan rendait
/// `("Aretha Franklin", pas une compilation)` là où le scan complet rendait
/// `("Various Artists", compilation)`. Le mécanisme n'est PAS celui de #3232 —
/// il ne dépend ni de `SCAN_BATCH_SIZE` ni de la taille du dossier : un dossier
/// de trois fichiers suffit dès que le pré-filtre en écarte deux.
///
/// Les deux champs sont nécessaires et ne se déduisent pas l'un de l'autre :
/// - `a_scanner` donne les DOSSIERS que ce scan touche, seuls à devoir être
///   amorcés — sinon toute la bibliothèque entrerait dans les preuves, que
///   `begin_batch` recalcule à chaque lot ;
/// - `ecartes` donne les fichiers dont la base est le seul témoin. Un fichier
///   relu apporte ses balises fraîches et ne doit RIEN reprendre de la base :
///   c'est ce qui laisse un « Scan complet » (`force`) souverain, puisqu'il
///   n'écarte rien et n'amorce donc rien.
#[derive(Clone, Copy)]
pub struct PorteeDuScan<'a> {
    /// Les fichiers présentés au scan (`files_to_scan`).
    pub a_scanner: &'a [std::path::PathBuf],
    /// Les fichiers que le pré-filtre a écartés parce qu'ils n'ont pas changé.
    pub ecartes: &'a [std::path::PathBuf],
}

impl PorteeDuScan<'_> {
    /// Rien d'écarté : le scan voit tout, la base n'a rien à amorcer.
    pub const TOUT: PorteeDuScan<'static> = PorteeDuScan {
        a_scanner: &[],
        ecartes: &[],
    };
}

/// Serialize the parsed multi-genre list to a JSON array string for
/// `tracks.genres`. Falls back to splitting the single `genre` tag for legacy
/// rows that predate multi-genre parsing.
pub fn build_genres_json(genres: &[String], genre: Option<&str>) -> Option<String> {
    if !genres.is_empty() {
        Some(serde_json::to_string(genres).unwrap_or_default())
    } else if let Some(g) = genre.filter(|g| !g.is_empty()) {
        // Split in case the single tag carries separators (legacy data).
        let split = tune_core::metadata::split_genre_tag(g);
        if split.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&split).unwrap_or_default())
        }
    } else {
        None
    }
}

/// Map a [`ScannedFile`]'s metadata onto a DB [`Track`] row.
///
/// `album_id` / `artist_id` / `track_artist_name` come from the caller's
/// artist/album resolution. The title falls back to the file stem when the tag
/// has none. `id` is left `None`; the update path sets it afterwards.
pub fn build_track_row(
    meta: &TrackMetadata,
    sf: &ScannedFile,
    album_id: Option<i64>,
    artist_id: Option<i64>,
    track_artist_name: &str,
) -> Track {
    let title = meta.title.clone().unwrap_or_else(|| {
        std::path::Path::new(&sf.path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default()
    });

    let mut track = Track::new(title);
    track.album_id = album_id;
    track.artist_id = artist_id;
    track.artist_name = Some(track_artist_name.to_string());
    track.album_artist = meta.album_artist.clone();
    track.album_title = meta.album.clone();
    track.disc_number = meta.disc_number.unwrap_or(1) as i32;
    track.disc_subtitle = meta.disc_subtitle.clone();
    track.track_number = meta.track_number.unwrap_or(0) as i32;
    track.duration_ms = meta.duration_ms.unwrap_or(0) as i64;
    track.file_path = Some(sf.path.clone());
    track.format = meta.format.clone();
    track.sample_rate = meta.sample_rate.map(|s| s as i32);
    track.bit_depth = meta.bit_depth.map(|b| b as i32);
    track.channels = meta.channels.unwrap_or(2) as i32;
    track.file_size = Some(sf.file_size as i64);
    track.file_mtime = Some(sf.mtime as f64);
    track.audio_hash = sf.audio_hash.clone();
    track.genre = meta.genre.clone();
    track.genres = build_genres_json(&meta.genres, meta.genre.as_deref());
    track.composer = meta
        .credits
        .iter()
        .find(|c| c.role == "composer")
        .map(|c| c.name.clone());
    track.year = meta.year.map(|y| y as i32);
    track.bpm = meta.bpm;
    track.label = meta.label.clone();
    track.isrc = meta.isrc.clone();
    track.musicbrainz_recording_id = meta.musicbrainz_recording_id.clone();
    track.comments = meta.comment.clone();

    // Dernière frontière avant TrackRepo. Le walker nettoie déjà les tags à
    // leur lecture, mais ce constructeur est public et partagé avec l'auto-
    // scan : aucun appelant ne doit pouvoir réintroduire un NUL ou un BOM dans
    // une colonne texte. `file_path` reste l'adresse physique exacte ; la
    // réécrire rendrait le fichier existant impossible à rouvrir.
    let corrections = sanitize_track_row_text(&mut track);
    if !corrections.is_empty() {
        tracing::warn!(
            path = %sf.path,
            corrections = ?corrections,
            "scan_import_unsafe_text_sanitized_at_db_boundary"
        );
    }
    track
}

fn sanitize_track_row_text(track: &mut Track) -> Vec<tune_core::metadata::TextCorrection> {
    fn clean(
        field: &str,
        value: &mut Option<String>,
        corrections: &mut Vec<tune_core::metadata::TextCorrection>,
    ) {
        let Some(raw) = value.as_deref() else {
            return;
        };
        let (sanitized, mut found) =
            tune_core::metadata::sanitize_untrusted_single_line_text(raw, field);
        if !found.is_empty() {
            *value = (!sanitized.is_empty()).then_some(sanitized);
            corrections.append(&mut found);
        }
    }

    let (title, mut corrections) =
        tune_core::metadata::sanitize_untrusted_single_line_text(&track.title, "title");
    if !corrections.is_empty() {
        track.title = title;
    }
    clean("album_title", &mut track.album_title, &mut corrections);
    clean("artist_name", &mut track.artist_name, &mut corrections);
    clean("album_artist", &mut track.album_artist, &mut corrections);
    clean("disc_subtitle", &mut track.disc_subtitle, &mut corrections);
    clean("format", &mut track.format, &mut corrections);
    clean("isrc", &mut track.isrc, &mut corrections);
    clean("genre", &mut track.genre, &mut corrections);
    clean("genres", &mut track.genres, &mut corrections);
    clean("composer", &mut track.composer, &mut corrections);
    clean("label", &mut track.label, &mut corrections);
    clean(
        "musicbrainz_recording_id",
        &mut track.musicbrainz_recording_id,
        &mut corrections,
    );

    if let Some(raw) = track.comments.as_deref() {
        let (sanitized, mut found) = tune_core::metadata::sanitize_untrusted_text(raw, "comments");
        if !found.is_empty() {
            track.comments = (!sanitized.is_empty()).then_some(sanitized);
            corrections.append(&mut found);
        }
    }
    corrections
}

/// Batch-stateful importer that resolves a scanned file's artist and album in
/// the DB and builds its [`Track`] row, sharing one implementation between the
/// manual scan and the auto/startup + watcher scans.
///
/// It carries the caches and the per-batch compilation decision the resolution
/// needs, so both scan paths get the *same* album grouping — the classical-
/// soloist album-artist pinning and the compilation-flattening that previously
/// lived only in the manual scan (the auto/watcher path used a simpler resolver
/// and could split a compilation, or an album with per-track soloists, into one
/// album+cover per artist).
///
/// Usage per batch: call [`begin_batch`](Self::begin_batch) once with the whole
/// batch, then [`import`](Self::import) for each file the caller has decided to
/// (re)index. The caller keeps ownership of the unchanged-file skip, the
/// insert-vs-update decision, dedup, and the transaction.
pub struct TrackImporter {
    artist_repo: ArtistRepo,
    album_repo: AlbumRepo,
    quality_split: bool,
    cache_dir: std::path::PathBuf,
    /// Caches persist across batches for the lifetime of a scan.
    artist_cache: HashMap<String, Arc<Artist>>,
    // Keyed by (title, album_artist_id, year, mb_release_id) — the MB release id
    // is part of the identity so two distinct editions sharing title+artist+year
    // don't collapse into one via the cache before the DB is even consulted
    // (Dominique). Tracks of one album that are only partially MB-tagged still
    // reconcile at the DB layer (see get_or_create_with_mbid).
    // The album's FOLDER leads the key: it is what identifies a release (see
    // `scanner::album_folder`), so two rips of the same album in two folders
    // never share a cache entry even though title+artist+year match.
    album_cache: HashMap<(String, String, i64, Option<i32>, Option<String>), Arc<Album>>,
    albums_with_cover: HashSet<i64>,
    /// First track-artist seen per folder, used to pin the album artist when a
    /// track has no `album_artist` tag (classical soloists / features).
    dir_album_artist: HashMap<String, String>,
    /// Les preuves de chaque dossier, ACCUMULÉES sur toute la durée du scan.
    ///
    /// 🔴 #3232 — voir [`PreuvesDuDossier`]. Les trois cartes ci-dessous en
    /// sont recalculées à chaque lot : la décision porte sur tout ce que le
    /// scan a vu du dossier, jamais sur le seul lot courant.
    ///
    /// 🔴 #3528 — et elles sont AMORCÉES depuis la base, à la construction,
    /// pour les fichiers que le pré-filtre du scan incrémental a écartés : ce
    /// que le scan « a vu » du dossier ne se limite pas à ce qu'il a relu.
    preuves: HashMap<String, PreuvesDuDossier>,
    /// Lignes album déjà reprises sous « Various Artists » pendant ce scan
    /// (#3232) : la reprise se fait UNE fois, pas à chaque piste du dossier.
    albums_reclasses: HashSet<i64>,
    /// `(folder, album)` → is-compilation decision, dérivée de `preuves`.
    comp_decision: HashMap<(String, String), bool>,
    /// Per-batch FOLDER → (various-artists, use-folder-name-as-title). Catches a
    /// hand-made compilation folder whose tracks span multiple album tags AND
    /// artists — which the `(folder, album)` decision above misses because mixed
    /// album tags split every group down to one track (JP Borderies).
    folder_comp: HashMap<String, (bool, bool)>,
    /// Par DOSSIER du lot : l'unique artiste d'album **étiqueté**, quand le
    /// dossier n'en porte qu'un.
    ///
    /// Il sert d'artiste d'album aux fichiers dont les balises n'ont pas pu
    /// être lues. Sans lui, un fichier en délai dépassé retombait sur
    /// `dir_album_artist`, épinglé par le PREMIER fichier vu du dossier —
    /// donc sur le nom du dossier si c'était lui le premier — et partait dans
    /// un album à part (#3232).
    folder_tagged_artist: HashMap<String, String>,
    artwork_extracted: u64,
    /// « Scan complet » : relire la pochette depuis les fichiers et **écraser**
    /// celle de la base.
    ///
    /// Faux par défaut — c'est le scan incrémental et le surveillant de
    /// fichiers, qui gardent la sonde héritée (URL stables, #1444) et
    /// l'écriture `COALESCE` (une pochette posée une fois ne bouge plus).
    ///
    /// Vrai uniquement pour un scan forcé, exactement comme le genre d'album
    /// (`scan.rs`, « A forced full scan is an explicit "rebuild from the files"
    /// action ») : sans cela, remplacer `cover.jpg` dans sa bibliothèque
    /// n'avait AUCUN chemin pour atteindre l'écran, pas même le bouton « Scan
    /// complet » (#3028).
    force_artwork: bool,
}

impl TrackImporter {
    /// `portee` n'est PAS optionnel, et c'est délibéré : c'est le compilateur
    /// qui oblige chaque scan à dire ce qu'il présente et ce qu'il écarte.
    ///
    /// 🔴 #3528 — les preuves du dossier ne valaient que pour les fichiers
    /// PRÉSENTÉS. Un scan incrémental n'en présente qu'une fraction : le
    /// verdict « compilation » d'une anthologie basculait dès qu'un seul de ses
    /// fichiers était relu. Un amorçage écrit mais qu'un des deux scans aurait
    /// oublié d'appeler aurait laissé la moitié du défaut en place ; passer la
    /// portée par la signature rend l'oubli impossible.
    pub fn new(
        db: Arc<dyn DbBackend>,
        quality_split: bool,
        cache_dir: std::path::PathBuf,
        portee: PorteeDuScan<'_>,
    ) -> Self {
        let preuves = Self::amorcer_depuis_la_base(&db, portee);
        Self {
            artist_repo: ArtistRepo::with_backend(db.clone()),
            album_repo: AlbumRepo::with_backend(db),
            quality_split,
            cache_dir,
            artist_cache: HashMap::new(),
            album_cache: HashMap::new(),
            albums_with_cover: HashSet::new(),
            dir_album_artist: HashMap::new(),
            preuves,
            albums_reclasses: HashSet::new(),
            comp_decision: HashMap::new(),
            folder_comp: HashMap::new(),
            folder_tagged_artist: HashMap::new(),
            artwork_extracted: 0,
            force_artwork: false,
        }
    }

    /// Amorce les preuves des dossiers touchés avec ce que la base sait des
    /// fichiers que le pré-filtre a ÉCARTÉS (#3528).
    ///
    /// # Pourquoi la base peut témoigner
    ///
    /// Les deux décisions ne consomment que trois valeurs : le titre d'album,
    /// l'artiste d'album, le drapeau « compilation ». `tracks` porte la balise
    /// `album_artist` BRUTE ; le titre d'album et le drapeau se lisent sur la
    /// ligne `albums` où la piste a été rangée ; l'artiste de repli est
    /// l'artiste RÉSOLU de la piste.
    ///
    /// Cet artiste résolu est précisément ce qui rend l'amorçage sûr au regard
    /// de #3232 : un fichier dont les balises n'ont pas pu être lues n'a jamais
    /// posé son nom de dossier dans `tracks.artist_id` — `import` l'avait déjà
    /// remplacé par l'artiste épinglé du dossier (`folder_tagged_artist`, puis
    /// `dir_album_artist`). Le faux second artiste ne peut donc pas rentrer par
    /// cette porte. Une piste sans ligne album n'apporte, elle, ni artiste ni
    /// titre : elle entre en `(None, None)`, exactement comme un fichier non
    /// étiqueté dans [`TrackImporter::begin_batch`].
    ///
    /// # Portée
    ///
    /// Seuls les DOSSIERS touchés par ce scan sont amorcés : sans cette borne,
    /// toute la bibliothèque entrerait dans `preuves`, que `begin_batch`
    /// reparcourt à chaque lot. Et seuls les fichiers ÉCARTÉS sont lus : un
    /// fichier relu apporte ses balises fraîches, qui priment. Un « Scan
    /// complet » n'écarte rien, donc n'amorce rien — c'est par lui qu'un
    /// verdict devenu faux se corrige.
    fn amorcer_depuis_la_base(
        db: &Arc<dyn DbBackend>,
        portee: PorteeDuScan<'_>,
    ) -> HashMap<String, PreuvesDuDossier> {
        use unicode_normalization::UnicodeNormalization;
        let nfc = |s: &str| -> String { s.nfc().collect() };
        if portee.ecartes.is_empty() || portee.a_scanner.is_empty() {
            return HashMap::new();
        }
        // Les dossiers que ce scan touche, indexés sous leur forme NFC (celle
        // des chemins en base) mais RENDUS dans l'orthographe du disque :
        // c'est elle que `begin_batch` et `import` emploieront comme clef.
        let dossiers: HashMap<String, String> = portee
            .a_scanner
            .iter()
            .filter_map(|p| p.parent())
            .map(|d| {
                let tel_quel = d.to_string_lossy().into_owned();
                (nfc(&tel_quel), tel_quel)
            })
            .collect();
        let ecartes: HashSet<String> = portee
            .ecartes
            .iter()
            .map(|p| nfc(&p.to_string_lossy()))
            .collect();
        let repo = tune_core::db::track_repo::TrackRepo::with_backend(Arc::clone(db));
        let connues = match repo.preuves_de_compilation() {
            Ok(c) => c,
            Err(e) => {
                // Amorcer est ce qui rend le verdict stable ; ne pas pouvoir le
                // faire n'est pas une raison d'arrêter le scan, mais cela doit
                // se voir dans le journal : sans amorçage, un dossier dont un
                // seul fichier a changé retombe sur le défaut de #3528.
                tracing::warn!(error = %e, "preuves_compilation_amorcage_impossible");
                return HashMap::new();
            }
        };
        let mut preuves: HashMap<String, PreuvesDuDossier> = HashMap::new();
        let mut pistes = 0usize;
        for (chemin, preuve) in connues {
            let chemin = nfc(&chemin);
            if !ecartes.contains(&chemin) {
                continue;
            }
            let Some(dossier) = std::path::Path::new(&chemin)
                .parent()
                .and_then(|d| dossiers.get(&nfc(&d.to_string_lossy())))
            else {
                continue;
            };
            let p = preuves.entry(dossier.clone()).or_default();
            pistes += 1;
            let Some(album) = preuve.album else {
                // Aucune ligne album : le scan précédent n'a trouvé chez ce
                // fichier ni balise `album` ni dossier-compilation. Il fait
                // savoir que le dossier existe, rien de plus.
                p.par_dossier.insert((None, None));
                continue;
            };
            p.par_album.insert((
                album.clone(),
                preuve.album_artist.clone(),
                preuve.compilation,
            ));
            let artiste = preuve.album_artist.or(preuve.artiste);
            p.par_dossier.insert((artiste, Some(album)));
        }
        if pistes > 0 {
            tracing::info!(
                dossiers = preuves.len(),
                pistes,
                ecartes = portee.ecartes.len(),
                "preuves_compilation_amorcees_depuis_la_base"
            );
        }
        preuves
    }

    /// Active la relecture des pochettes pour un « Scan complet ».
    ///
    /// Voir [`TrackImporter::force_artwork`]. Appelé par la route de scan avec
    /// le même `force` qui commande déjà le contournement du saut de fichiers
    /// inchangés et la réécriture du genre d'album.
    #[must_use]
    pub fn with_force_artwork(mut self, force: bool) -> Self {
        self.force_artwork = force;
        self
    }

    /// Number of album covers extracted so far (for the scan report).
    pub fn artwork_extracted(&self) -> u64 {
        self.artwork_extracted
    }

    /// Compute the per-`(folder, album)` compilation decision for this batch so
    /// every track of an album agrees on its album artist regardless of
    /// inconsistent per-track `album_artist` tags.
    ///
    /// La décision porte sur le DOSSIER ENTIER, et non sur ce que le hasard du
    /// découpage a mis dans ce lot : c'est
    /// [`tune_core::scanner::walker::lots_alignes_sur_les_dossiers`] qui le
    /// garantit, en n'ouvrant jamais un lot au milieu d'un dossier.
    /// L'ancienne rédaction affirmait ici que « les pistes d'un album sont
    /// contiguës, donc dans le même lot » — c'était faux deux fois : un album
    /// de plus de `SCAN_BATCH_SIZE = 500` pistes coupé en deux, et surtout
    /// tout album à cheval sur une frontière de lot. Le dossier était alors
    /// jugé DEUX FOIS sur deux populations différentes, et le tag
    /// « compilation » sortait au hasard (Pierre M, fil 1043, #3232).
    ///
    /// Second volet du même défaut : un fichier dont les balises n'ont pas pu
    /// être lues (délai dépassé sur un NAS) arrive avec un artiste déduit du
    /// nom de dossier. Il n'entre dans AUCUNE des deux décisions — elles
    /// dépendent des balises, et il n'en a pas. Ce faux artiste suffisait à
    /// basculer un dossier entier en « Various Artists », et comme les délais
    /// dépassés changent d'un scan à l'autre, le basculement aussi.
    ///
    /// Troisième volet, et c'est celui que l'alignement sur les dossiers ne
    /// peut PAS couvrir : un dossier plus gros qu'un lot ne tient dans aucun
    /// lot — le lot porte les pochettes embarquées. `lots_alignes_sur_les_
    /// dossiers` le coupe, en le disant (`scan_dossier_plus_gros_qu_un_lot`),
    /// et le témoin en place n'a jamais vu ce cas : sa boucle part de la
    /// taille du plus gros dossier. Mesuré sur l'anthologie de Pierre M : en
    /// un lot « Various Artists, compilation », en deux lots « Angela Brown,
    /// pas une compilation ». Les preuves sont donc ACCUMULÉES par dossier sur
    /// toute la durée du scan ([`PreuvesDuDossier`]), et les décisions
    /// recalculées à chaque lot sur ce total — jamais sur le seul lot courant.
    /// Ce que les lots précédents avaient déjà écrit sous une décision
    /// partielle est repris par `import`, voir `reclasser_en_compilation`.
    pub fn begin_batch(&mut self, batch: &[ScannedFile]) {
        // Les balises de ce fichier ont-elles été lues ? Sinon, son artiste
        // n'est qu'un nom de dossier : il ne pèse dans aucune décision.
        let etiquete = |sf: &ScannedFile| sf.metadata.as_ref().is_some_and(|m| !m.artist_from_path);
        let dossier = |sf: &ScannedFile| {
            std::path::Path::new(&sf.path)
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default()
        };
        // 1. Verser CE lot dans les preuves du scan. Les ensembles
        //    dédoublonnent : le coût suit les valeurs distinctes, pas les
        //    fichiers.
        for sf in batch {
            let Some(meta) = sf.metadata.as_ref() else {
                continue;
            };
            let dir = dossier(sf);
            let p = self.preuves.entry(dir).or_default();
            if !etiquete(sf) {
                // Le dossier reste connu (il faut savoir qu'il existe), mais un
                // fichier sans balises n'y apporte ni artiste ni titre d'album.
                p.par_dossier.insert((None, None));
                continue;
            }
            if let Some(album) = meta.album.as_deref() {
                p.par_album.insert((
                    album.to_string(),
                    meta.album_artist.clone(),
                    meta.compilation,
                ));
            }
            let artist = meta.album_artist.as_deref().or(meta.artist.as_deref());
            p.par_dossier
                .insert((artist.map(str::to_string), meta.album.clone()));
        }

        // 2. Recalculer les deux décisions sur le TOTAL accumulé. Ce sont les
        //    mêmes fonctions qu'avant, avec la seule population qui vaille :
        //    le dossier entier, tel que le scan l'a vu jusqu'ici.
        self.comp_decision = decide_compilation_albums(self.preuves.iter().flat_map(|(dir, p)| {
            p.par_album
                .iter()
                .map(move |(album, aa, comp)| (dir.clone(), album.as_str(), aa.as_deref(), *comp))
        }));
        self.folder_comp = decide_compilation_folders(self.preuves.iter().flat_map(|(dir, p)| {
            p.par_dossier
                .iter()
                .map(move |(artist, album)| (dir.clone(), artist.as_deref(), album.as_deref()))
        }));

        // 3. L'unique artiste d'album ÉTIQUETÉ de chaque dossier, quand il n'y
        //    en a qu'un : c'est lui qu'adoptera un fichier sans balises, au
        //    lieu du nom de son dossier. Lui aussi se lit sur le total : un
        //    lot où le dossier n'a qu'un artiste ne doit pas en promettre un
        //    quand le dossier entier en porte deux.
        self.folder_tagged_artist = self
            .preuves
            .iter()
            .filter_map(|(dir, p)| {
                let mut vus: Vec<&str> = p
                    .par_dossier
                    .iter()
                    .filter_map(|(artist, _)| artist.as_deref().map(str::trim))
                    .filter(|s| !s.is_empty())
                    .collect();
                vus.sort_unstable();
                vus.dedup();
                match vus.as_slice() {
                    [seul] => Some((dir.clone(), (*seul).to_string())),
                    _ => None,
                }
            })
            .collect();
    }

    /// Resolve artist + album, extract album cover / artist image as a side
    /// effect, and build the `Track` row. Returns `None` when the file has no
    /// metadata. `id` is left `None`; the caller sets it for the update path.
    pub fn import(&mut self, sf: &ScannedFile) -> Option<(Track, Option<i64>)> {
        let meta = sf.metadata.as_ref()?;

        // Compilation status: prefer the per-(folder,album) decision so every
        // track of the album agrees; fall back to this track's own signal if
        // the album is not in the decision at all.
        //
        // Cette décision porte sur le DOSSIER ENTIER, et plus seulement sur ce
        // que le scan en a relu : les preuves sont accumulées sur toute la
        // durée du scan (#3232) ET amorcées depuis la base pour les fichiers
        // que le pré-filtre a écartés (#3528, voir `amorcer_depuis_la_base`).
        // Le repli ci-dessous ne sert donc plus qu'à un album que la base ne
        // connaît pas encore.
        let album_dir = std::path::Path::new(&sf.path)
            .parent()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        // Per-folder compilation signal (mixed album tags + artists) OR the
        // per-(folder,album) decision. `use_folder_title` = the folder name is
        // the real album title (per-track album tags are unrelated).
        let (folder_va, use_folder_title) = self
            .folder_comp
            .get(&album_dir)
            .copied()
            .unwrap_or((false, false));
        let is_compilation = folder_va
            || meta
                .album
                .as_ref()
                .and_then(|a| {
                    self.comp_decision
                        .get(&(album_dir.clone(), a.to_lowercase()))
                        .copied()
                })
                .unwrap_or_else(|| {
                    meta.compilation
                        || meta
                            .album_artist
                            .as_deref()
                            .map(is_various_artists)
                            .unwrap_or(false)
                });

        let album_artist_name = if is_compilation {
            "Various Artists".to_string()
        } else if let Some(aa) = meta.album_artist.as_deref() {
            aa.to_string()
        } else if meta.artist_from_path {
            // Les balises de ce fichier n'ont pas pu être lues : son « artiste »
            // est le nom d'un dossier. Il prend l'artiste d'album étiqueté du
            // dossier quand il n'y en a qu'un — sinon son album partait tout
            // seul, à côté de celui de ses voisines (#3232). Il n'ÉPINGLE
            // jamais le dossier : une valeur inventée ne doit pas devenir la
            // référence des fichiers qui, eux, portent des balises.
            self.folder_tagged_artist
                .get(&album_dir)
                .cloned()
                .or_else(|| self.dir_album_artist.get(&album_dir).cloned())
                .unwrap_or_else(|| {
                    meta.artist
                        .as_deref()
                        .unwrap_or(tune_core::db::artist_repo::UNKNOWN_ARTIST_NAME)
                        .to_string()
                })
        } else {
            // No album_artist tag: pin the album artist to the first track
            // artist seen in this folder so all of the album's tracks resolve to
            // a single album row instead of splitting per differing track artist.
            let track_a = meta
                .artist
                .as_deref()
                .unwrap_or(tune_core::db::artist_repo::UNKNOWN_ARTIST_NAME);
            self.dir_album_artist
                .entry(album_dir.clone())
                .or_insert_with(|| track_a.to_string())
                .clone()
        };

        let track_artist_name = meta
            .artist
            .as_deref()
            .unwrap_or(tune_core::db::artist_repo::UNKNOWN_ARTIST_NAME)
            .to_string();

        let album_artist_mbid = if is_compilation {
            None
        } else {
            meta.musicbrainz_album_artist_id
                .as_deref()
                .or(meta.musicbrainz_artist_id.as_deref())
        };
        let album_artist_entry = if let Some(cached) = self.artist_cache.get(&album_artist_name) {
            Some(Arc::clone(cached))
        } else {
            let result = self
                .artist_repo
                .get_or_create(
                    &album_artist_name,
                    album_artist_mbid,
                    meta.album_artist_sort.as_deref(),
                )
                .ok()
                .map(Arc::new);
            if let Some(ref a) = result {
                self.artist_cache
                    .insert(album_artist_name.clone(), Arc::clone(a));
            }
            result
        };
        let album_artist_id = album_artist_entry.as_ref().and_then(|a| a.id);

        let track_artist = if is_compilation && track_artist_name != album_artist_name {
            if let Some(cached) = self.artist_cache.get(&track_artist_name) {
                Some(Arc::clone(cached))
            } else {
                let result = self
                    .artist_repo
                    .get_or_create(
                        &track_artist_name,
                        meta.musicbrainz_artist_id.as_deref(),
                        None,
                    )
                    .ok()
                    .map(Arc::new);
                if let Some(ref a) = result {
                    self.artist_cache
                        .insert(track_artist_name.clone(), Arc::clone(a));
                }
                result
            }
        } else {
            album_artist_entry.clone()
        };
        let artist_id = track_artist.as_ref().and_then(|a| a.id);

        if let Some(ref album_title) = meta.album {
            let t = album_title.to_lowercase();
            if t.contains("best") || t.contains("greatest") || t.contains("hits") {
                // `debug!`, et non `info!` : cette sonde est émise UNE FOIS PAR
                // PISTE de tout album dont le titre contient « best »,
                // « greatest » ou « hits ». Chez un testeur, 311 lignes en dix
                // minutes sur 35 albums — dont 31 pour le seul « Very Best of
                // Maria Callas » — soit 31 % de la fenêtre du rapport de bogue,
                // qui ne retient que l'INFO et au-dessus (#2028). Elle a servi
                // à instruire les compilations ; laissée en `info!`, elle coûte
                // désormais plus qu'elle ne rapporte : elle chasse du rapport
                // ce qu'on y cherchait.
                tracing::debug!(
                    album = %album_title,
                    album_artist_tag = ?meta.album_artist,
                    artist_tag = ?meta.artist,
                    resolved_album_artist = album_artist_name.as_str(),
                    resolved_artist_id = ?album_artist_id,
                    resolved_artist_name = ?album_artist_entry.as_ref().map(|a| &a.name),
                    year = ?meta.year,
                    file = %sf.path,
                    "DIAG_generic_album_scan"
                );
            }
        }

        // The quality tier used to be appended to the title here ("Album
        // (96kHz/24bit)") so a hi-res copy would not merge with a CD rip. It
        // split far more than intended — an edition whose discs differ in sample
        // rate became several albums under near-identical titles — and the
        // client already shows the real quality as a badge from
        // `sample_rate`/`bit_depth`. The folder does the separating now.
        // `quality_split` keeps its meaning — "if the same album exists in CD and
        // Hi-Res, create two separate entries" — and the folder is what now
        // delivers it. Off ⇒ empty folder, so every copy shares one cache key and
        // one album row.
        let album_folder = if self.quality_split {
            tune_core::scanner::album_folder::album_folder(&sf.path).unwrap_or_default()
        } else {
            String::new()
        };
        let album_key = if use_folder_title {
            // Mixed compilation: one album per folder, named after the folder and
            // keyed on folder + Various-Artists id. The per-track album / year /
            // mbid are unrelated across the compilation, so they are dropped from
            // the key — otherwise a differing year would re-split the folder into
            // several albums.
            std::path::Path::new(&album_dir)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .filter(|s| !s.is_empty())
                .or_else(|| meta.album.clone())
                .map(|t| {
                    (
                        album_folder.clone(),
                        t,
                        album_artist_id.unwrap_or(0),
                        None,
                        None,
                    )
                })
        } else {
            meta.album.as_ref().map(|t| {
                (
                    album_folder.clone(),
                    t.clone(),
                    album_artist_id.unwrap_or(0),
                    meta.year.map(|y| y as i32),
                    meta.musicbrainz_release_id.clone(),
                )
            })
        };

        let album = if let Some(ref key) = album_key {
            if let Some(cached) = self.album_cache.get(key) {
                let c = Arc::clone(cached);
                if c.artist_id != Some(key.2) {
                    tracing::warn!(
                        album = %key.1,
                        cache_key_artist_id = key.2,
                        cached_album_id = ?c.id,
                        cached_album_artist_id = ?c.artist_id,
                        file = %sf.path,
                        "BUG_album_cache_artist_mismatch"
                    );
                }
                Some(c)
            } else {
                let result = self.album_repo.get_or_create_for_folder_with_track(
                    &key.0,
                    &key.1,
                    key.2,
                    key.3,
                    meta.musicbrainz_release_id.as_deref(),
                    // Le numéro de piste sert UNIQUEMENT à décider si ce
                    // dossier est l'éclat d'une compilation déjà indexée
                    // (#1440) : un numéro déjà pris ⇒ homonyme, pas éclat.
                    meta.track_number.map(|n| n as i32),
                );
                if let Err(ref e) = result {
                    tracing::warn!(
                        album = %key.1,
                        artist_id = key.2,
                        year = ?key.3,
                        error = %e,
                        file = %sf.path,
                        "BUG_album_create_failed"
                    );
                }
                let result = result.ok().map(Arc::new);
                if let Some(ref a) = result {
                    if a.artist_id != Some(key.2) {
                        tracing::warn!(
                            album = %key.1,
                            requested_artist_id = key.2,
                            returned_album_id = ?a.id,
                            returned_artist_id = ?a.artist_id,
                            mb_release_id = ?meta.musicbrainz_release_id,
                            file = %sf.path,
                            "BUG_album_artist_mismatch"
                        );
                    }
                    self.album_cache.insert(key.clone(), Arc::clone(a));
                }
                result
            }
        } else {
            None
        };

        let album_id = album.as_ref().and_then(|a| a.id);

        // Garder la décision qui vient d'être prise (#1957). C'est elle qui a
        // envoyé l'album sous « Various Artists » quelques lignes plus haut ;
        // sans cette écriture elle mourait ici, et rien dans la base ne disait
        // plus pourquoi vingt artistes tiennent dans un même disque.
        // `mark_compilation` ne fait que lever le drapeau : la décision est
        // prise par piste, et une anthologie dont la première piste, seule, ne
        // ressemble à rien ne doit pas dépendre de l'ordre des fichiers.
        if let Some(aid) = album_id
            && is_compilation
        {
            self.album_repo.mark_compilation(aid).ok();
            // 🔴 #3232 — la ligne album a pu être créée par un lot PRÉCÉDENT du
            // même dossier, sous une décision partielle : un dossier plus gros
            // qu'un lot est coupé, et son premier morceau ne montre parfois
            // qu'un seul artiste. La décision porte sur le DOSSIER : la ligne
            // est reprise sous « Various Artists », et sous le titre du dossier
            // quand les balises d'album des pistes sont sans rapport entre
            // elles. Le cas courant — le dossier tient dans un lot — n'entre
            // jamais ici : la ligne est née avec la bonne identité.
            let titre_voulu = album_key.as_ref().map(|k| k.1.as_str());
            let ligne = album.as_ref();
            let bon_artiste = ligne.and_then(|a| a.artist_id) == album_artist_id;
            let bon_titre = match (ligne, titre_voulu) {
                (Some(a), Some(t)) => a.title == t,
                _ => true,
            };
            // Une seule reprise par ligne album et par scan : la copie en
            // cache garde l'ancien titre, et sans cette marque chaque piste
            // suivante du dossier rejouerait le même UPDATE.
            if let (Some(va), Some(titre)) = (album_artist_id, titre_voulu)
                && (!bon_artiste || !bon_titre)
                && self.albums_reclasses.insert(aid)
            {
                match self.album_repo.reclasser_en_compilation(aid, va, titre) {
                    Ok(()) => tracing::info!(
                        album_id = aid,
                        dossier = %album_dir,
                        ancien_titre = ?ligne.map(|a| &a.title),
                        ancien_artiste_id = ?ligne.and_then(|a| a.artist_id),
                        nouveau_titre = %titre,
                        nouvel_artiste_id = va,
                        "album_reclasse_en_compilation"
                    ),
                    Err(e) => tracing::warn!(
                        album_id = aid,
                        dossier = %album_dir,
                        error = %e,
                        "album_reclassement_compilation_echoue"
                    ),
                }
            }
        }

        // Propagate date metadata from track tags to the album.
        if let Some(aid) = album_id {
            self.album_repo
                .update_dates(
                    aid,
                    meta.year.map(|y| y as i32),
                    meta.original_year.map(|y| y as i32),
                    meta.release_date.as_deref(),
                    meta.original_date.as_deref(),
                )
                .ok();
        }

        if let Some(aid) = album_id
            && !self.albums_with_cover.contains(&aid)
        {
            // Prefer the embedded cover already read while parsing the tags —
            // re-opening the file to extract it failed (os error 3) for some
            // accented Windows paths even though the first read had succeeded.
            //
            // Sur un « Scan complet », les variantes `*_refresh` sautent la
            // sonde héritée : celle-ci est adressée par le CHEMIN de la piste,
            // qui ne bouge pas quand on remplace `cover.jpg`, et rendait donc
            // l'ancienne image sans rouvrir le moindre fichier (#3028).
            let cover_hash = match meta.cover_art.as_ref() {
                Some(cover) if self.force_artwork => {
                    tune_core::library::artwork::cache_embedded_cover(
                        std::path::Path::new(&sf.path),
                        &self.cache_dir,
                        cover,
                    )
                }
                Some(cover) => tune_core::library::artwork::save_embedded_cover(
                    std::path::Path::new(&sf.path),
                    &self.cache_dir,
                    cover,
                ),
                None if self.force_artwork => tune_core::library::artwork::refresh_cover_hash(
                    std::path::Path::new(&sf.path),
                    &self.cache_dir,
                ),
                None => tune_core::library::artwork::get_or_extract(
                    std::path::Path::new(&sf.path),
                    &self.cache_dir,
                ),
            };
            if let Some(hash) = cover_hash {
                // `update_cover_path` est un `COALESCE` : il ne remplace jamais
                // une valeur déjà posée. C'est ce qu'il faut entre deux scans
                // complets, et c'est exactement ce qui retenait l'ancienne
                // pochette en base quand l'utilisateur en avait posé une neuve
                // sur son disque (#3028). Un scan forcé écrase, comme il écrase
                // déjà le genre d'album.
                let ecriture = if self.force_artwork {
                    self.album_repo.force_update_cover_path(aid, &hash)
                } else {
                    self.album_repo.update_cover_path(aid, &hash)
                };
                if let Err(e) = ecriture {
                    tracing::warn!(album_id = aid, error = %e, "cover_path_update_failed");
                }
                self.albums_with_cover.insert(aid);
                self.artwork_extracted += 1;
            }
        }

        // Check for a local artist image (artist.jpg/png next to the tracks).
        //
        // La mise en cache est passée dans `tune_core::library::artwork` pour
        // être adressée par le CONTENU (#1444) et testable : la même
        // `artist.jpg` recopiée dans les N dossiers d'album d'un artiste
        // n'écrit plus qu'UNE entrée de cache. L'entrée héritée, adressée par
        // le chemin, reste sondée d'abord — aucune URL déjà distribuée ne
        // bouge. Rien n'est enregistré si l'écriture du cache échoue, sinon la
        // base annonce « a une image » sans rien sur le disque (carré gris +
        // saut définitif).
        if let Some(ref art) = track_artist {
            if art.image_path.is_none() {
                match tune_core::library::artwork::folder_artist_image_hash(
                    std::path::Path::new(&sf.path),
                    &self.cache_dir,
                ) {
                    Some(hash) => {
                        let mut updated_artist = tune_core::db::models::Artist::clone(art);
                        updated_artist.image_path = Some(hash);
                        updated_artist.image_source = Some("local".to_string());
                        if let Err(e) = self.artist_repo.update(&updated_artist) {
                            tracing::warn!(error = %e, "artist_image_update_failed");
                        }
                    }
                    None => {
                        tracing::trace!(
                            artist = %art.name,
                            "artist_image_absente_ou_non_mise_en_cache"
                        );
                    }
                }
            }
        }

        let mut track = build_track_row(meta, sf, album_id, artist_id, &track_artist_name);

        // Per-track cover, ONLY for a folder the scanner had to name itself.
        //
        // `use_folder_title` means this folder holds several artists AND several
        // unrelated album tags, so every file in it was filed under one album
        // named after the folder. That is right for a hand-made compilation, but
        // a folder of unrelated files gets the same treatment — and the album
        // cover above is whichever artwork the FIRST such file happened to carry
        // (`albums_with_cover` never lets a later track override it). Bebelalu55
        // played a WAV and saw another artist's sleeve (forum #1312).
        //
        // Giving the track its own artwork fixes the display without touching
        // how the folder is grouped: reads do COALESCE(t.cover_path,
        // al.cover_path), so a track with no embedded art still falls back to
        // its album's, exactly as before.
        //
        // Only `meta.cover_art` is used — the bytes were already read while
        // parsing the tags. The `get_or_extract` fallback re-opens the file, and
        // paying that on every track of every mixed folder is not worth it.
        //
        // Même règle que la pochette d'album au-dessus : un « Scan complet »
        // saute la sonde héritée, sans quoi la pochette de PISTE resterait
        // périmée pendant que celle de l'album se rafraîchit (#3028).
        if use_folder_title && let Some(cover) = meta.cover_art.as_ref() {
            track.cover_path = if self.force_artwork {
                tune_core::library::artwork::cache_embedded_cover(
                    std::path::Path::new(&sf.path),
                    &self.cache_dir,
                    cover,
                )
            } else {
                tune_core::library::artwork::save_embedded_cover(
                    std::path::Path::new(&sf.path),
                    &self.cache_dir,
                    cover,
                )
            };
        }

        Some((track, album_id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::metadata::{TrackCredit, TrackMetadata};
    use tune_core::scanner::walker::ScannedFile;

    fn sf(path: &str) -> ScannedFile {
        ScannedFile {
            path: path.to_string(),
            metadata: None,
            unsupported: None,
            audio_hash: Some("hash-1".into()),
            file_size: 4096,
            mtime: 1_700_000_000,
        }
    }

    /// LE CAS RÉEL de .18, joué au niveau de l'import (#1440).
    ///
    /// Quatre volumes « ALLOPOP », un dossier par artiste de piste, tous
    /// tagués `ALBUMARTIST = La Souterraine` et sans année. Observé en
    /// production : un seul album de 71 pistes. Le dépôt, lui, sépare
    /// correctement — donc si ce test échoue, la fusion vient d'ici.
    #[test]
    fn four_volumes_of_one_title_do_not_collapse_at_import() {
        use std::sync::Arc;
        use tune_core::db::sqlite::SqliteDb;

        let tmp = tempfile::tempdir().unwrap();
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
        let mut imp = TrackImporter::new(
            backend.clone(),
            true,
            tmp.path().to_path_buf(),
            PorteeDuScan::TOUT,
        );

        let mut fichiers = Vec::new();
        for (vol, artiste) in ["Diane", "Tristan", "Nina", "Oscar"].iter().enumerate() {
            let d = tmp.path().join(artiste).join("ALLOPOP");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join("cover.jpg"), format!("IMAGE-VOL-{vol}")).unwrap();
            let chemin = d.join("01 - titre.flac").to_string_lossy().into_owned();
            let mut f = sf(&chemin);
            f.metadata = Some(TrackMetadata {
                title: Some(format!("titre {vol}")),
                artist: Some((*artiste).to_string()),
                album: Some("ALLOPOP".into()),
                album_artist: Some("La Souterraine".into()),
                track_number: Some(1),
                ..Default::default()
            });
            fichiers.push(f);
        }

        let mut albums = std::collections::HashSet::new();
        for f in &fichiers {
            let (piste, _) = imp.import(f).expect("import");
            albums.insert(piste.album_id);
        }

        assert_eq!(
            albums.len(),
            4,
            "quatre dossiers, quatre pochettes ⇒ quatre albums (obtenu : {albums:?})"
        );
    }

    /// #3028 — remplacer `cover.jpg` dans sa bibliothèque, joué jusqu'à la
    /// BASE et jusqu'aux OCTETS servis.
    ///
    /// Deux passes sur le même dossier, l'image changée entre les deux. Le
    /// scan ordinaire garde l'ancienne (témoin anti-régression : URL stables,
    /// #1444) ; le « Scan complet » écrit le condensat de la nouvelle, et le
    /// fichier de cache sous cette adresse porte bien les octets neufs.
    ///
    /// Ce test ÉCHOUE contre le code d'avant : `force_artwork` n'existait pas,
    /// la sonde héritée rendait l'ancien condensat et le `COALESCE` de
    /// `update_cover_path` refusait de le remplacer.
    #[test]
    fn un_scan_complet_remplace_la_pochette_periee_en_base() {
        use std::sync::Arc;
        use tune_core::db::album_repo::AlbumRepo;
        use tune_core::db::sqlite::SqliteDb;

        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache");
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
        let album_repo = AlbumRepo::with_backend(backend.clone());

        let dossier = tmp.path().join("Bilou").join("Album");
        std::fs::create_dir_all(&dossier).unwrap();
        std::fs::write(dossier.join("cover.jpg"), b"ANCIENNE-POCHETTE").unwrap();
        let chemin = dossier
            .join("01 - titre.flac")
            .to_string_lossy()
            .into_owned();
        let fichier = || {
            let mut f = sf(&chemin);
            f.metadata = Some(TrackMetadata {
                title: Some("titre".into()),
                artist: Some("Bilou".into()),
                album: Some("Album".into()),
                album_artist: Some("Bilou".into()),
                track_number: Some(1),
                ..Default::default()
            });
            f
        };

        // Première passe : la bibliothèque est indexée avec l'ancienne image.
        let mut imp = TrackImporter::new(backend.clone(), true, cache.clone(), PorteeDuScan::TOUT);
        let (piste, _) = imp.import(&fichier()).expect("import");
        let aid = piste.album_id.expect("album");
        let ancien = album_repo.get(aid).unwrap().unwrap().cover_path.unwrap();

        // L'utilisateur remplace l'image sur son disque.
        std::fs::write(dossier.join("cover.jpg"), b"NOUVELLE-POCHETTE").unwrap();

        // TÉMOIN — scan ordinaire : rien ne bouge, l'URL distribuée tient.
        let mut ordinaire =
            TrackImporter::new(backend.clone(), true, cache.clone(), PorteeDuScan::TOUT);
        ordinaire.import(&fichier()).expect("import");
        assert_eq!(
            album_repo.get(aid).unwrap().unwrap().cover_path.as_deref(),
            Some(ancien.as_str()),
            "un scan incrémental ne fait pas tourner les URL"
        );

        // « Scan complet ».
        let mut complet =
            TrackImporter::new(backend.clone(), true, cache.clone(), PorteeDuScan::TOUT)
                .with_force_artwork(true);
        complet.import(&fichier()).expect("import");

        let nouveau = album_repo.get(aid).unwrap().unwrap().cover_path.unwrap();
        assert_ne!(nouveau, ancien, "la base annonce une autre adresse");
        assert_eq!(
            nouveau,
            tune_core::library::artwork::content_hash(b"NOUVELLE-POCHETTE"),
            "l'adresse est le condensat de la NOUVELLE image"
        );
        let (fichier_cache, _) = tune_core::library::artwork::find_cached(&cache, &nouveau)
            .expect("l'entrée de cache existe");
        assert_eq!(
            std::fs::read(fichier_cache).unwrap(),
            b"NOUVELLE-POCHETTE",
            "les octets servis sont ceux de la nouvelle image"
        );
    }

    /// LE FAIT de #1957, joué de bout en bout : le drapeau `TCMP` du fichier
    /// arrive jusqu'à la LIGNE ALBUM, au lieu de servir au regroupement puis
    /// d'être jeté. Un album ordinaire, dans le même scan, reste à « non ».
    ///
    /// Ce test ÉCHOUE contre le code d'avant : `albums` n'avait pas de colonne.
    #[test]
    fn le_drapeau_compilation_atteint_la_ligne_album() {
        use std::sync::Arc;
        use tune_core::db::album_repo::AlbumRepo;
        use tune_core::db::sqlite::SqliteDb;

        let tmp = tempfile::tempdir().unwrap();
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
        let mut imp = TrackImporter::new(
            backend.clone(),
            true,
            tmp.path().to_path_buf(),
            PorteeDuScan::TOUT,
        );
        let albums = AlbumRepo::with_backend(backend.clone());

        // L'anthologie : deux pistes, deux artistes, `TCMP` posé sur les deux.
        let dossier = tmp.path().join("Jazz sur Seine");
        std::fs::create_dir_all(&dossier).unwrap();
        let mut anthologie = Vec::new();
        for (n, artiste) in ["Django", "Stéphane"].iter().enumerate() {
            let chemin = dossier
                .join(format!("0{}.flac", n + 1))
                .to_string_lossy()
                .into_owned();
            let mut f = sf(&chemin);
            f.metadata = Some(TrackMetadata {
                title: Some(format!("titre {n}")),
                artist: Some((*artiste).to_string()),
                album: Some("Jazz sur Seine".into()),
                album_artist: Some((*artiste).to_string()),
                track_number: Some(n as u32 + 1),
                compilation: true,
                ..Default::default()
            });
            anthologie.push(f);
        }

        // Le témoin : un vrai album d'un seul artiste, sans `TCMP`.
        let d2 = tmp.path().join("Kind of Blue");
        std::fs::create_dir_all(&d2).unwrap();
        let chemin = d2.join("01.flac").to_string_lossy().into_owned();
        let mut temoin = sf(&chemin);
        temoin.metadata = Some(TrackMetadata {
            title: Some("So What".into()),
            artist: Some("Miles Davis".into()),
            album: Some("Kind of Blue".into()),
            album_artist: Some("Miles Davis".into()),
            track_number: Some(1),
            ..Default::default()
        });

        let lot: Vec<_> = anthologie
            .into_iter()
            .chain(std::iter::once(temoin))
            .collect();
        imp.begin_batch(&lot);

        let mut id_compilation = None;
        let mut id_temoin = None;
        for f in &lot {
            let (_, album_id) = imp.import(f).expect("import");
            let album_id = album_id.expect("un album");
            if f.path.contains("Kind of Blue") {
                id_temoin = Some(album_id);
            } else {
                id_compilation = Some(album_id);
            }
        }

        let compilation = albums.get(id_compilation.unwrap()).unwrap().unwrap();
        assert!(
            compilation.is_compilation,
            "le drapeau TCMP doit atteindre la ligne album « {} »",
            compilation.title
        );

        let temoin = albums.get(id_temoin.unwrap()).unwrap().unwrap();
        assert!(
            !temoin.is_compilation,
            "un album ordinaire du même scan ne doit pas être marqué : « {} »",
            temoin.title
        );
    }

    #[test]
    fn build_genres_json_prefers_parsed_list() {
        let g = build_genres_json(&["Jazz".into(), "Fusion".into()], Some("ignored"));
        assert_eq!(g.as_deref(), Some(r#"["Jazz","Fusion"]"#));
    }

    #[test]
    fn build_genres_json_falls_back_to_single_tag_split() {
        // Empty parsed list → split the legacy single tag.
        let g = build_genres_json(&[], Some("Jazz; Fusion"));
        assert_eq!(g.as_deref(), Some(r#"["Jazz","Fusion"]"#));
        // Nothing at all → None (not an empty-array string).
        assert_eq!(build_genres_json(&[], None), None);
        assert_eq!(build_genres_json(&[], Some("")), None);
    }

    #[test]
    fn decide_compilation_folders_flags_mixed_folders_only() {
        // JP Borderies: a hand-made compilation — several artists AND several
        // album tags in one folder → Various Artists + folder-name title.
        let mixed = decide_compilation_folders(
            [
                (
                    "/comp".to_string(),
                    Some("Angela Brown"),
                    Some("Just Fabulous - Live"),
                ),
                (
                    "/comp".to_string(),
                    Some("Aretha Franklin"),
                    Some("Amazing Grace"),
                ),
                (
                    "/comp".to_string(),
                    Some("Nina Simone"),
                    Some("Pastel Blues"),
                ),
            ]
            .into_iter(),
        );
        assert_eq!(mixed.get("/comp"), Some(&(true, true)));

        // Genuine various-artists album: many artists, ONE album tag ("Woodstock")
        // → VA artist, but KEEP the album title (use_folder_title = false).
        let va_one_tag = decide_compilation_folders(
            [
                (
                    "/woodstock".to_string(),
                    Some("Jimi Hendrix"),
                    Some("Woodstock"),
                ),
                ("/woodstock".to_string(), Some("Santana"), Some("Woodstock")),
            ]
            .into_iter(),
        );
        assert_eq!(va_one_tag.get("/woodstock"), Some(&(true, false)));

        // Single-artist multi-disc: one artist, two album tags → untouched.
        let multidisc = decide_compilation_folders(
            [
                (
                    "/album".to_string(),
                    Some("Pink Floyd"),
                    Some("The Wall (Disc 1)"),
                ),
                (
                    "/album".to_string(),
                    Some("Pink Floyd"),
                    Some("The Wall (Disc 2)"),
                ),
            ]
            .into_iter(),
        );
        assert_eq!(multidisc.get("/album"), Some(&(false, false)));

        // Plain single-artist single-album folder → untouched.
        let plain = decide_compilation_folders(
            [(
                "/kob".to_string(),
                Some("Miles Davis"),
                Some("Kind of Blue"),
            )]
            .into_iter(),
        );
        assert_eq!(plain.get("/kob"), Some(&(false, false)));
    }

    #[test]
    fn build_track_row_maps_every_field_incl_previously_dropped_ones() {
        let meta = TrackMetadata {
            title: Some("So What".into()),
            album: Some("Kind of Blue".into()),
            album_artist: Some("Miles Davis".into()),
            disc_number: Some(1),
            disc_subtitle: Some("Side A".into()),
            track_number: Some(1),
            duration_ms: Some(544_000),
            sample_rate: Some(44_100),
            bit_depth: Some(24),
            channels: Some(2),
            format: Some("flac".into()),
            year: Some(1959),
            bpm: Some(136.0),
            label: Some("Columbia".into()),
            isrc: Some("USSM15900001".into()),
            musicbrainz_recording_id: Some("rec-1".into()),
            comment: Some("remaster".into()),
            genres: vec!["Jazz".into(), "Modal".into()],
            genre: Some("Jazz".into()),
            credits: vec![TrackCredit {
                name: "Miles Davis".into(),
                role: "composer".into(),
                instrument: None,
            }],
            ..Default::default()
        };
        let track = build_track_row(
            &meta,
            &sf("/m/kob/01.flac"),
            Some(7),
            Some(3),
            "Miles Davis",
        );

        assert_eq!(track.id, None);
        assert_eq!(track.title, "So What");
        assert_eq!(track.album_id, Some(7));
        assert_eq!(track.artist_id, Some(3));
        assert_eq!(track.artist_name.as_deref(), Some("Miles Davis"));
        assert_eq!(track.album_title.as_deref(), Some("Kind of Blue"));
        // disc_subtitle was dropped by the old manual *insert* path.
        assert_eq!(track.disc_subtitle.as_deref(), Some("Side A"));
        assert_eq!(track.duration_ms, 544_000);
        assert_eq!(track.sample_rate, Some(44_100));
        assert_eq!(track.bit_depth, Some(24));
        assert_eq!(track.channels, 2);
        assert_eq!(track.file_path.as_deref(), Some("/m/kob/01.flac"));
        assert_eq!(track.file_size, Some(4096));
        assert_eq!(track.audio_hash.as_deref(), Some("hash-1"));
        // genres + composer were dropped by the old auto/watcher helper.
        assert_eq!(track.genres.as_deref(), Some(r#"["Jazz","Modal"]"#));
        assert_eq!(track.composer.as_deref(), Some("Miles Davis"));
        assert_eq!(track.year, Some(1959));
        assert_eq!(track.bpm, Some(136.0));
        assert_eq!(track.isrc.as_deref(), Some("USSM15900001"));
        assert_eq!(track.comments.as_deref(), Some("remaster"));
    }

    #[test]
    fn build_track_row_title_falls_back_to_file_stem_and_defaults() {
        let meta = TrackMetadata::default();
        let track = build_track_row(
            &meta,
            &sf("/m/x/Untitled Take.flac"),
            None,
            None,
            "Unknown Artist",
        );
        assert_eq!(track.title, "Untitled Take");
        // Sensible defaults when tags are absent.
        assert_eq!(track.disc_number, 1);
        assert_eq!(track.track_number, 0);
        assert_eq!(track.channels, 2);
        assert_eq!(track.duration_ms, 0);
        assert_eq!(track.genres, None);
        assert_eq!(track.composer, None);
    }

    #[test]
    fn build_track_row_ne_persiste_aucun_nul_ni_bom_hors_adresse_physique() {
        let meta = TrackMetadata {
            title: Some("Titre\0cache".into()),
            album: Some("Album\u{feff}Live".into()),
            album_artist: Some("Lisa\0Strings".into()),
            disc_subtitle: Some("Disque\u{feff}I".into()),
            format: Some("flac\0".into()),
            isrc: Some("FR\u{feff}123".into()),
            genre: Some("Jazz\0Fusion".into()),
            genres: vec!["Jazz\u{feff}Fusion".into()],
            credits: vec![TrackCredit {
                name: "Miles\0Davis".into(),
                role: "composer".into(),
                instrument: None,
            }],
            label: Some("Columbia\u{feff}Records".into()),
            musicbrainz_recording_id: Some("rec\0id".into()),
            comment: Some("ligne 1\nligne\0 2".into()),
            ..Default::default()
        };
        let scanned = sf("/m/Jacobs, Lisa\u{feff}The Strings/01.flac");
        let track = build_track_row(
            &meta,
            &scanned,
            Some(7),
            Some(3),
            "Lisa\0\u{feff}The Strings",
        );

        let persisted_text = [
            Some(track.title.as_str()),
            track.album_title.as_deref(),
            track.artist_name.as_deref(),
            track.album_artist.as_deref(),
            track.disc_subtitle.as_deref(),
            track.format.as_deref(),
            track.isrc.as_deref(),
            track.genre.as_deref(),
            track.genres.as_deref(),
            track.composer.as_deref(),
            track.label.as_deref(),
            track.musicbrainz_recording_id.as_deref(),
            track.comments.as_deref(),
        ];
        assert!(
            persisted_text
                .into_iter()
                .flatten()
                .all(|value| !value.contains(['\0', '\u{feff}']))
        );
        assert_eq!(track.comments.as_deref(), Some("ligne 1\nligne 2"));
        assert_eq!(
            track.file_path.as_deref(),
            Some("/m/Jacobs, Lisa\u{feff}The Strings/01.flac")
        );
    }

    // ─────────────────────────────────────────────────────────────────────
    // #3232 — « le tag compilation est pris en compte au hasard, dans un même
    // répertoire » (Pierre M, fil 1043, 14/07/2026).
    //
    // Deux causes, deux volets, et un témoin sans lequel on livrerait le
    // défaut symétrique (tout devient une compilation, ou plus rien).
    // ─────────────────────────────────────────────────────────────────────

    /// Un dossier importé, rendu comme `chemin → (artiste d'album résolu,
    /// drapeau compilation de la ligne album, identifiant d'album)`.
    ///
    /// Passe par le VRAI découpage en lots
    /// (`lots_alignes_sur_les_dossiers`) puis par le vrai `begin_batch` /
    /// `import`, base neuve à chaque appel : c'est le trajet de production,
    /// pas une reconstitution.
    fn importer_par_lots(
        fichiers: &[ScannedFile],
        taille_de_lot: usize,
        cache: &std::path::Path,
    ) -> std::collections::BTreeMap<String, (String, bool, i64)> {
        importer_par_lots_avec_reglage(fichiers, taille_de_lot, cache, true)
    }

    /// Comme [`importer_par_lots`], en choisissant `quality_split` : c'est lui
    /// qui met — ou non — le dossier dans la clé d'une ligne album, et un
    /// coffret ne se regroupe pas de la même façon des deux côtés (#3855).
    fn importer_par_lots_avec_reglage(
        fichiers: &[ScannedFile],
        taille_de_lot: usize,
        cache: &std::path::Path,
        quality_split: bool,
    ) -> std::collections::BTreeMap<String, (String, bool, i64)> {
        importer_par_lots_detail_avec_reglage(fichiers, taille_de_lot, cache, quality_split)
            .into_iter()
            .map(|(chemin, (artiste, compilation, id, _titre))| {
                (chemin, (artiste, compilation, id))
            })
            .collect()
    }

    /// Comme [`importer_par_lots`], en rendant aussi le TITRE de la ligne
    /// album. Le titre fait partie du verdict : une compilation faite main est
    /// nommée d'après son dossier, et une décision prise sur un lot partiel la
    /// nommait d'après la première piste vue (#3232).
    #[allow(clippy::type_complexity)]
    fn importer_par_lots_detail(
        fichiers: &[ScannedFile],
        taille_de_lot: usize,
        cache: &std::path::Path,
    ) -> std::collections::BTreeMap<String, (String, bool, i64, String)> {
        importer_par_lots_detail_avec_reglage(fichiers, taille_de_lot, cache, true)
    }

    #[allow(clippy::type_complexity)]
    fn importer_par_lots_detail_avec_reglage(
        fichiers: &[ScannedFile],
        taille_de_lot: usize,
        cache: &std::path::Path,
        quality_split: bool,
    ) -> std::collections::BTreeMap<String, (String, bool, i64, String)> {
        use std::sync::Arc;
        use tune_core::db::album_repo::AlbumRepo;
        use tune_core::db::artist_repo::ArtistRepo;
        use tune_core::db::sqlite::SqliteDb;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
        let albums = AlbumRepo::with_backend(backend.clone());
        let artistes = ArtistRepo::with_backend(backend.clone());
        let mut imp = TrackImporter::new(
            backend.clone(),
            quality_split,
            cache.to_path_buf(),
            PorteeDuScan::TOUT,
        );

        let chemins: Vec<std::path::PathBuf> =
            fichiers.iter().map(|f| f.path.clone().into()).collect();
        let par_chemin: std::collections::HashMap<&str, &ScannedFile> =
            fichiers.iter().map(|f| (f.path.as_str(), f)).collect();

        let mut rendu = std::collections::BTreeMap::new();
        for lot in
            tune_core::scanner::walker::lots_alignes_sur_les_dossiers(&chemins, taille_de_lot)
        {
            let lot: Vec<ScannedFile> = lot
                .iter()
                .map(|p| (*par_chemin[p.to_str().unwrap()]).clone())
                .collect();
            imp.begin_batch(&lot);
            for f in &lot {
                let (_piste, album_id) = imp.import(f).expect("import");
                rendu.insert(f.path.clone(), album_id.expect("un album"));
            }
        }
        // Les lignes album sont relues APRÈS tous les lots : c'est l'état que
        // l'utilisateur voit, et c'est là qu'un dossier coupé en morceaux doit
        // avoir convergé (#3232).
        rendu
            .into_iter()
            .map(|(chemin, album_id)| {
                let album = albums.get(album_id).unwrap().unwrap();
                let nom = artistes
                    .get(album.artist_id.expect("un artiste d'album"))
                    .unwrap()
                    .unwrap()
                    .name;
                (
                    chemin,
                    (nom, album.is_compilation, album_id, album.title.clone()),
                )
            })
            .collect()
    }

    /// Fichier étiqueté : les balises ont été lues.
    fn fichier_etiquete(chemin: &str, artiste: &str, album: &str, piste: u32) -> ScannedFile {
        let mut f = sf(chemin);
        f.metadata = Some(TrackMetadata {
            title: Some(format!("{album} {piste}")),
            artist: Some(artiste.to_string()),
            album: Some(album.to_string()),
            album_artist: Some(artiste.to_string()),
            track_number: Some(piste),
            ..Default::default()
        });
        f
    }

    /// ÉPREUVE 1 + ÉPREUVE 3 — le verdict « compilation » ne dépend plus du
    /// découpage en lots, et les deux témoins restent ce qu'ils sont.
    ///
    /// `/anthologie` est la compilation faite main de Pierre M : trois pistes,
    /// DEUX artistes seulement, dont l'un porte deux pistes. Coupée en lots de
    /// deux, elle se présentait comme deux populations d'un seul artiste
    /// chacune — donc deux fois « pas une compilation ». Vue entière, elle en
    /// est une. C'est tout le défaut : le même dossier, jugé sur ce que le
    /// hasard du découpage avait mis dans le lot.
    ///
    /// Ce test ÉCHOUE contre le code d'avant : il suffit de rendre
    /// `lots_alignes_sur_les_dossiers` à un `chunks()` pour le voir tomber.
    #[test]
    fn le_verdict_compilation_ne_depend_pas_du_decoupage_en_lots() {
        let tmp = tempfile::tempdir().unwrap();
        let racine = tmp.path();
        let d = |nom: &str| racine.join(nom);
        for nom in ["anthologie", "Kind of Blue", "Woodstock"] {
            std::fs::create_dir_all(d(nom)).unwrap();
        }
        let c =
            |dossier: &str, fichier: &str| d(dossier).join(fichier).to_string_lossy().into_owned();

        let mut fichiers = Vec::new();
        // La compilation faite main : deux artistes, trois pistes.
        fichiers.push(fichier_etiquete(
            &c("anthologie", "01.flac"),
            "Angela Brown",
            "Just Fabulous",
            1,
        ));
        fichiers.push(fichier_etiquete(
            &c("anthologie", "02.flac"),
            "Angela Brown",
            "Just Fabulous",
            2,
        ));
        fichiers.push(fichier_etiquete(
            &c("anthologie", "03.flac"),
            "Aretha Franklin",
            "Amazing Grace",
            3,
        ));
        // TÉMOIN A — un vrai album d'un seul artiste. Il ne doit JAMAIS
        // basculer : sans lui, « tout est une compilation » passerait le test.
        for n in 1..=3u32 {
            fichiers.push(fichier_etiquete(
                &c("Kind of Blue", &format!("0{n}.flac")),
                "Miles Davis",
                "Kind of Blue",
                n,
            ));
        }
        // TÉMOIN B — une vraie compilation, elle, reste une compilation : trois
        // artistes sous un seul titre d'album.
        for (n, artiste) in ["Jimi Hendrix", "Santana", "Janis Joplin"]
            .iter()
            .enumerate()
        {
            fichiers.push(fichier_etiquete(
                &c("Woodstock", &format!("0{}.flac", n + 1)),
                artiste,
                "Woodstock",
                n as u32 + 1,
            ));
        }

        // Le découpage varie ; le verdict, non. La borne basse est la taille du
        // plus gros dossier (3) : un lot plus petit qu'un dossier ne peut pas
        // le contenir, et le scan réel travaille par 500.
        let mut reference = None;
        for taille in 3..=9usize {
            let rendu = importer_par_lots(&fichiers, taille, &tmp.path().join("cache"));
            let verdicts: std::collections::BTreeMap<String, (String, bool)> = rendu
                .iter()
                .map(|(k, (a, c, _))| (k.clone(), (a.clone(), *c)))
                .collect();

            for (chemin, (artiste, compilation)) in &verdicts {
                if chemin.contains("anthologie") {
                    assert_eq!(
                        (artiste.as_str(), *compilation),
                        ("Various Artists", true),
                        "lots de {taille} : la compilation faite main doit être vue ENTIÈRE ({chemin})"
                    );
                } else if chemin.contains("Kind of Blue") {
                    assert_eq!(
                        (artiste.as_str(), *compilation),
                        ("Miles Davis", false),
                        "lots de {taille} : témoin — un album d'un seul artiste le reste ({chemin})"
                    );
                } else {
                    assert_eq!(
                        (artiste.as_str(), *compilation),
                        ("Various Artists", true),
                        "lots de {taille} : témoin — une vraie compilation le reste ({chemin})"
                    );
                }
            }

            // Et pas seulement « correct » : IDENTIQUE d'un découpage à l'autre.
            match &reference {
                None => reference = Some(verdicts),
                Some(attendu) => assert_eq!(
                    &verdicts, attendu,
                    "lots de {taille} : le verdict a changé avec le découpage"
                ),
            }
        }

        // Une compilation, un seul album : le dossier ne s'est pas éclaté.
        let rendu = importer_par_lots(&fichiers, 4, &tmp.path().join("cache2"));
        let albums_anthologie: std::collections::BTreeSet<i64> = rendu
            .iter()
            .filter(|(k, _)| k.contains("anthologie"))
            .map(|(_, (_, _, id))| *id)
            .collect();
        assert_eq!(
            albums_anthologie.len(),
            1,
            "les trois pistes de l'anthologie tiennent dans un seul album"
        );
    }

    /// Les mêmes trois dossiers que l'épreuve ci-dessus : une compilation faite
    /// main (deux artistes), un album d'un seul artiste, une vraie compilation.
    fn les_trois_dossiers(racine: &std::path::Path) -> Vec<ScannedFile> {
        let d = |nom: &str| racine.join(nom);
        for nom in ["anthologie", "Kind of Blue", "Woodstock"] {
            std::fs::create_dir_all(d(nom)).unwrap();
        }
        let c =
            |dossier: &str, fichier: &str| d(dossier).join(fichier).to_string_lossy().into_owned();
        let mut fichiers = Vec::new();
        fichiers.push(fichier_etiquete(
            &c("anthologie", "01.flac"),
            "Angela Brown",
            "Just Fabulous",
            1,
        ));
        fichiers.push(fichier_etiquete(
            &c("anthologie", "02.flac"),
            "Angela Brown",
            "Just Fabulous",
            2,
        ));
        fichiers.push(fichier_etiquete(
            &c("anthologie", "03.flac"),
            "Aretha Franklin",
            "Amazing Grace",
            3,
        ));
        for n in 1..=3u32 {
            fichiers.push(fichier_etiquete(
                &c("Kind of Blue", &format!("0{n}.flac")),
                "Miles Davis",
                "Kind of Blue",
                n,
            ));
        }
        for (n, artiste) in ["Jimi Hendrix", "Santana", "Janis Joplin"]
            .iter()
            .enumerate()
        {
            fichiers.push(fichier_etiquete(
                &c("Woodstock", &format!("0{}.flac", n + 1)),
                artiste,
                "Woodstock",
                n as u32 + 1,
            ));
        }
        fichiers
    }

    /// ÉPREUVE 4 — le verdict tient AUSSI quand le dossier est PLUS GROS qu'un
    /// lot, le seul découpage que l'alignement sur les dossiers ne peut pas
    /// supprimer.
    ///
    /// L'épreuve précédente ne voit pas ce cas, et le dit : sa boucle part de
    /// la taille du plus gros dossier (3), « un lot plus petit qu'un dossier ne
    /// peut pas le contenir ». C'est justement là que
    /// `lots_alignes_sur_les_dossiers` capitule — le lot porte les pochettes
    /// embarquées, garder un dossier de 2 000 fichiers d'un bloc est ce qui a
    /// fait sauter la machine de JeromeQ — et il le journalise
    /// (`scan_dossier_plus_gros_qu_un_lot`, « la décision compilation de ce
    /// dossier sera prise par morceaux »). Un dossier fourre-tout de plus de
    /// 500 fichiers, c'est exactement ce que Pierre M a sur son NAS.
    ///
    /// Mesuré contre le code d'avant, sur l'anthologie :
    ///   lots de 1 → (« Angela Brown », pas une compilation)
    ///   lots de 2 → (« Angela Brown », pas une compilation)
    ///   lots de 3 → (« Various Artists », compilation)
    /// Le même dossier, deux verdicts, selon le découpage.
    ///
    /// Ce test ÉCHOUE contre le code d'avant.
    #[test]
    fn le_verdict_tient_meme_quand_le_dossier_est_plus_gros_qu_un_lot() {
        let tmp = tempfile::tempdir().unwrap();
        let fichiers = les_trois_dossiers(tmp.path());

        // Le verdict de référence : tout dans un seul lot.
        let reference: std::collections::BTreeMap<String, (String, bool, String)> =
            importer_par_lots_detail(&fichiers, 500, &tmp.path().join("ref"))
                .into_iter()
                .map(|(k, (a, c, _, t))| (k, (a, c, t)))
                .collect();

        // La borne basse est 1 : un lot d'un seul fichier coupe CHAQUE dossier.
        for taille in 1..=9usize {
            let rendu: std::collections::BTreeMap<String, (String, bool, String)> =
                importer_par_lots_detail(&fichiers, taille, &tmp.path().join(format!("c{taille}")))
                    .into_iter()
                    .map(|(k, (a, c, _, t))| (k, (a, c, t)))
                    .collect();
            assert_eq!(
                rendu, reference,
                "lots de {taille} : le verdict a changé avec le découpage"
            );
        }

        // Et le verdict de référence est bien celui qu'on attend — sans quoi
        // « identique à la référence » se contenterait d'une référence fausse.
        for (chemin, (artiste, compilation, titre)) in &reference {
            if chemin.contains("anthologie") {
                assert_eq!(
                    (artiste.as_str(), *compilation, titre.as_str()),
                    ("Various Artists", true, "anthologie"),
                    "la compilation faite main est nommée d'après son dossier ({chemin})"
                );
            } else if chemin.contains("Kind of Blue") {
                assert_eq!(
                    (artiste.as_str(), *compilation, titre.as_str()),
                    ("Miles Davis", false, "Kind of Blue"),
                    "témoin — un album d'un seul artiste le reste ({chemin})"
                );
            } else {
                assert_eq!(
                    (artiste.as_str(), *compilation, titre.as_str()),
                    ("Various Artists", true, "Woodstock"),
                    "témoin — une vraie compilation garde son titre ({chemin})"
                );
            }
        }

        // Un dossier, un album, quelle que soit la découpe.
        for taille in [1usize, 2, 500] {
            let rendu =
                importer_par_lots_detail(&fichiers, taille, &tmp.path().join(format!("a{taille}")));
            for dossier in ["anthologie", "Kind of Blue", "Woodstock"] {
                let ids: std::collections::BTreeSet<i64> = rendu
                    .iter()
                    .filter(|(k, _)| k.contains(dossier))
                    .map(|(_, (_, _, id, _))| *id)
                    .collect();
                assert_eq!(
                    ids.len(),
                    1,
                    "lots de {taille} : {dossier} doit tenir dans un seul album"
                );
            }
        }
    }

    /// Un scan complet, PUIS un scan incrémental qui ne présente qu'un seul
    /// fichier — les autres passent par `ecartes`, comme le pré-filtre du vrai
    /// scan les y met.
    ///
    /// La base est la MÊME pour les deux passes : c'est tout l'objet de #3528,
    /// la seconde n'a que la base pour savoir ce que la première a vu. Les
    /// pistes sont donc réellement écrites (`create_batch`), sans quoi
    /// l'amorçage lirait une table vide et le test serait vert contre rien.
    ///
    /// Rend, par chemin, `(artiste d'album, drapeau compilation, titre de la
    /// ligne album)` APRÈS la seconde passe.
    #[allow(clippy::type_complexity)]
    fn scan_complet_puis_incremental(
        fichiers: &[ScannedFile],
        modifie: &str,
        cache: &std::path::Path,
        quality_split: bool,
    ) -> std::collections::BTreeMap<String, (String, bool, String)> {
        use std::sync::Arc;
        use tune_core::db::album_repo::AlbumRepo;
        use tune_core::db::artist_repo::ArtistRepo;
        use tune_core::db::sqlite::SqliteDb;
        use tune_core::db::track_repo::TrackRepo;

        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let backend: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);
        let albums = AlbumRepo::with_backend(backend.clone());
        let artistes = ArtistRepo::with_backend(backend.clone());
        let pistes = TrackRepo::with_backend(backend.clone());

        // --- Passe 1 : le scan complet. Rien d'écarté.
        let tous: Vec<std::path::PathBuf> =
            fichiers.iter().map(|f| f.path.clone().into()).collect();
        let mut imp = TrackImporter::new(
            backend.clone(),
            quality_split,
            cache.join("plein"),
            PorteeDuScan {
                a_scanner: &tous,
                ecartes: &[],
            },
        );
        imp.begin_batch(fichiers);
        let mut a_inserer = Vec::new();
        for f in fichiers {
            let (piste, _) = imp.import(f).expect("import");
            a_inserer.push(piste);
        }
        assert_eq!(
            pistes.create_batch(&a_inserer).unwrap(),
            fichiers.len(),
            "les pistes du scan complet doivent être en base"
        );

        // --- Passe 2 : le scan incrémental. UN seul fichier présenté, tous les
        // autres écartés parce qu'ils n'ont pas changé.
        let (presente, ecartes): (Vec<&ScannedFile>, Vec<&ScannedFile>) =
            fichiers.iter().partition(|f| f.path == modifie);
        assert_eq!(presente.len(), 1, "un seul fichier modifié");
        let a_scanner: Vec<std::path::PathBuf> =
            presente.iter().map(|f| f.path.clone().into()).collect();
        let ecartes_chemins: Vec<std::path::PathBuf> =
            ecartes.iter().map(|f| f.path.clone().into()).collect();
        let mut imp = TrackImporter::new(
            backend.clone(),
            quality_split,
            cache.join("incremental"),
            PorteeDuScan {
                a_scanner: &a_scanner,
                ecartes: &ecartes_chemins,
            },
        );
        let lot: Vec<ScannedFile> = presente.iter().map(|f| (*f).clone()).collect();
        imp.begin_batch(&lot);
        let connues = pistes.get_all_file_info_by_path().unwrap();
        for f in &lot {
            let (mut piste, _) = imp.import(f).expect("import");
            piste.id = Some(connues.get(&f.path).expect("piste connue").id);
            pistes.update(&piste).expect("mise à jour");
        }

        // L'état que l'utilisateur voit, une fois le scan incrémental fini.
        let connues = pistes.get_all_file_info_by_path().unwrap();
        fichiers
            .iter()
            .map(|f| {
                let id = connues.get(&f.path).expect("piste connue").id;
                let piste = pistes.get(id).unwrap().unwrap();
                let album = albums
                    .get(piste.album_id.expect("un album"))
                    .unwrap()
                    .unwrap();
                let nom = artistes
                    .get(album.artist_id.expect("un artiste d'album"))
                    .unwrap()
                    .unwrap()
                    .name;
                (
                    f.path.clone(),
                    (nom, album.is_compilation, album.title.clone()),
                )
            })
            .collect()
    }

    /// ÉPREUVE 5 — #3528 : le verdict tient quand le scan ne PRÉSENTE qu'un
    /// seul fichier du dossier.
    ///
    /// # Ce n'est pas le foyer de #3232
    ///
    /// Le seuil de `SCAN_BATCH_SIZE` n'y est pour rien : `files_to_scan` est
    /// pré-filtré des fichiers inchangés AVANT le découpage en lots
    /// (`routes::system::scan`, `auto_scan`), si bien que l'anthologie de trois
    /// fichiers de Pierre M n'en présente qu'un dès que les deux autres sont à
    /// jour. Les preuves accumulées de #3232 ne vivent que le temps du scan :
    /// elles ne voient pas ce que le pré-filtre a écarté.
    ///
    /// # Ce que le rangement par dossier masquait
    ///
    /// Mesuré sur Shrek, correctif retiré, les deux réglages de `quality_split`
    /// séparément :
    ///
    ///   `quality_split` activé  → verdict inchangé
    ///   `quality_split` DÉSACTIVÉ → ("Angela Brown", pas une compilation,
    ///                               "Just Fabulous")
    ///
    /// Activé, le dossier EST l'identité de l'album
    /// (`AlbumRepo::get_or_create_for_folder_with_track` rend la ligne déjà
    /// posée pour ce dossier, quel que soit le titre demandé) : la décision
    /// partielle est prise, mais elle n'a pas de ligne à abîmer. Désactivé, la
    /// clef d'album redevient `(titre, artiste, année, mbid)` — c'est-à-dire le
    /// verdict lui-même — et la piste relue QUITTE l'anthologie pour un album à
    /// son seul nom. Le second réglage est donc le témoin qui rougit ; le
    /// premier reste dans l'épreuve parce qu'un masque n'est pas un correctif,
    /// et qu'il doit rester vert.
    ///
    /// Ce test ÉCHOUE contre le code d'avant.
    #[test]
    fn le_verdict_tient_quand_le_scan_ne_presente_qu_un_fichier_du_dossier() {
        let tmp = tempfile::tempdir().unwrap();
        let fichiers = les_trois_dossiers(tmp.path());

        // Un fichier modifié dans CHAQUE dossier, à tour de rôle : le verdict
        // du dossier ne doit dépendre ni de celui qu'on relit, ni du fait
        // qu'on n'en relise qu'un.
        for quality_split in [true, false] {
            for (n, modifie) in fichiers.iter().enumerate() {
                let cache = tmp.path().join(format!("cache{quality_split}{n}"));
                let rendu =
                    scan_complet_puis_incremental(&fichiers, &modifie.path, &cache, quality_split);
                for (chemin, verdict) in &rendu {
                    let attendu = if chemin.contains("anthologie") {
                        ("Various Artists", true, "anthologie")
                    } else if chemin.contains("Kind of Blue") {
                        ("Miles Davis", false, "Kind of Blue")
                    } else {
                        ("Various Artists", true, "Woodstock")
                    };
                    assert_eq!(
                        (verdict.0.as_str(), verdict.1, verdict.2.as_str()),
                        attendu,
                        "quality_split={quality_split}, après relecture du seul {} : \
                         le verdict de {chemin} a changé",
                        modifie.path
                    );
                }
            }
        }
    }

    /// ÉPREUVE 2 — un fichier dont les balises n'ont pas pu être lues ne
    /// bascule pas son dossier en « Various Artists ».
    ///
    /// C'est le lien que personne n'avait fait : Pierre M signalait, dans le
    /// MÊME message, des délais dépassés en rafale sur son NAS. Un fichier en
    /// délai dépassé est indexé par `tagless_fallback_no_props`, qui déduisait
    /// du chemin un `album_artist` — « Beatles », le nom du dossier parent, là
    /// où les autres pistes portent « The Beatles ». Deux artistes dans un
    /// dossier : compilation. Et comme les délais changent d'un scan à
    /// l'autre, le basculement aussi — « au hasard ».
    ///
    /// Le repli est appelé ICI, tel quel : c'est bien la fonction de
    /// production qui fabrique la métadonnée de l'épreuve.
    ///
    /// Ce test ÉCHOUE contre le code d'avant.
    #[test]
    fn un_fichier_sans_balises_ne_bascule_pas_le_dossier_en_various_artists() {
        let tmp = tempfile::tempdir().unwrap();
        // Le dossier ne s'écrit pas comme le tag : c'est le cas ordinaire.
        let dossier = tmp.path().join("Beatles").join("Abbey Road");
        std::fs::create_dir_all(&dossier).unwrap();
        let c = |n: &str| dossier.join(n).to_string_lossy().into_owned();

        let mut fichiers = vec![
            fichier_etiquete(&c("01.flac"), "The Beatles", "Abbey Road", 1),
            fichier_etiquete(&c("02.flac"), "The Beatles", "Abbey Road", 2),
        ];
        // Le fichier en délai dépassé, monté par la VRAIE fonction de repli.
        let chemin_muet = c("03.wav");
        let muet =
            tune_core::metadata::tagless_fallback_no_props(std::path::Path::new(&chemin_muet));
        assert!(
            muet.artist_from_path,
            "le repli doit se dénoncer, sinon la décision ne peut pas l'écarter"
        );
        assert_eq!(
            muet.artist.as_deref(),
            Some("Beatles"),
            "le nom déduit du chemin diffère bien du tag — c'est ce qui fabriquait le second artiste"
        );
        let mut f = sf(&chemin_muet);
        f.metadata = Some(muet);
        fichiers.push(f);

        let rendu = importer_par_lots(&fichiers, 8, &tmp.path().join("cache"));
        for (chemin, (artiste, compilation, _)) in &rendu {
            assert_eq!(
                (artiste.as_str(), *compilation),
                ("The Beatles", false),
                "un fichier illisible ne fait pas d'Abbey Road une compilation ({chemin})"
            );
        }
        let albums: std::collections::BTreeSet<i64> =
            rendu.values().map(|(_, _, id)| *id).collect();
        assert_eq!(
            albums.len(),
            1,
            "et il ne part pas non plus dans un album à lui tout seul"
        );

        // TÉMOIN — une VRAIE compilation reste une compilation, même quand un
        // de ses fichiers est illisible. Sans ce témoin, écarter purement et
        // simplement les fichiers en repli passerait pour une correction.
        let dossier = tmp.path().join("Various").join("Woodstock");
        std::fs::create_dir_all(&dossier).unwrap();
        let c = |n: &str| dossier.join(n).to_string_lossy().into_owned();
        let mut fichiers = vec![
            fichier_etiquete(&c("01.flac"), "Jimi Hendrix", "Woodstock", 1),
            fichier_etiquete(&c("02.flac"), "Santana", "Woodstock", 2),
        ];
        let chemin_muet = c("03.wav");
        let mut f = sf(&chemin_muet);
        f.metadata = Some(tune_core::metadata::tagless_fallback_no_props(
            std::path::Path::new(&chemin_muet),
        ));
        fichiers.push(f);
        let rendu = importer_par_lots(&fichiers, 8, &tmp.path().join("cache3"));
        for (chemin, (artiste, compilation, _)) in &rendu {
            assert_eq!(
                (artiste.as_str(), *compilation),
                ("Various Artists", true),
                "témoin : une vraie compilation le reste, fichier illisible compris ({chemin})"
            );
        }
    }

    /// #3855 — LE COFFRET DE PIERRE M, à la forme exacte de ses captures.
    ///
    /// « The Complete RCA Album Collection » (Reiner / Chicago SO, 63 CD). Le
    /// dossier `CD02 Strauss Ein Heldenleben` contient six fichiers ; l'éditeur
    /// de balises affiche `Album Artist <different>` : cinq portent
    /// « Fritz Reiner », un porte « Chicago Symphony Orchestra, Fritz Reiner ».
    /// Dans Tune il voyait DEUX albums de même titre — la piste 2 d'un côté,
    /// les pistes 1, 3, 4, 5 et 6 de l'autre.
    ///
    /// La clé d'une ligne album porte l'identifiant de l'artiste d'album
    /// (`import`, plus bas) : deux valeurs d'`album_artist` dans un même
    /// dossier ⇒ deux lignes. `decide_compilation_albums` est la règle qui
    /// absorbe ce cas ; ce témoin mesure qu'elle joue, sur les DEUX réglages de
    /// `quality_split` — le sien n'est pas connu, et l'album de 399 titres de
    /// sa capture ne s'explique que si le dossier ne fait pas partie de la clé.
    ///
    /// ⚠️ Ce témoin ne dit PAS que le défaut de Pierre M est corrigé chez lui :
    /// sa version n'est pas établie et une base construite avant #3232/#3528
    /// garde ce que le premier scan a décidé. Il dit que le code d'aujourd'hui
    /// ne le reproduit plus.
    #[test]
    fn un_coffret_dont_l_album_artist_varie_dans_un_dossier_ne_se_coupe_pas_en_deux_3855() {
        const TITRE: &str = "The Complete RCA Album Collection";
        const LONG: &str = "Chicago Symphony Orchestra, Fritz Reiner";
        let tmp = tempfile::tempdir().unwrap();
        let coffret = tmp.path().join("Reiner").join(TITRE);
        let disque = |n: &str| coffret.join(n);
        for n in ["CD01 Bartok", "CD02 Strauss Ein Heldenleben", "CD03 Mahler"] {
            std::fs::create_dir_all(disque(n)).unwrap();
        }
        let piste = |dossier: &str,
                     fichier: &str,
                     artiste: &str,
                     album_artiste: &str,
                     numero_disque: u32,
                     numero: u32| {
            let chemin = disque(dossier).join(fichier).to_string_lossy().into_owned();
            let mut f = sf(&chemin);
            f.metadata = Some(TrackMetadata {
                title: Some(format!("d{numero_disque} p{numero}")),
                artist: Some(artiste.to_string()),
                album: Some(TITRE.to_string()),
                album_artist: Some(album_artiste.to_string()),
                disc_number: Some(numero_disque),
                track_number: Some(numero),
                year: Some(2013),
                ..Default::default()
            });
            f
        };
        let mut fichiers = Vec::new();
        for p in 1..=3u32 {
            fichiers.push(piste(
                "CD01 Bartok",
                &format!("0{p}.flac"),
                "Bela Bartok",
                LONG,
                1,
                p,
            ));
        }
        // LE dossier de sa capture : six fichiers, deux valeurs d'album_artist.
        for p in 1..=6u32 {
            let aa = if p == 2 { LONG } else { "Fritz Reiner" };
            fichiers.push(piste(
                "CD02 Strauss Ein Heldenleben",
                &format!("0{p}.flac"),
                "Richard Strauss",
                aa,
                2,
                p,
            ));
        }
        for p in 1..=3u32 {
            fichiers.push(piste(
                "CD03 Mahler",
                &format!("0{p}.flac"),
                "Gustav Mahler",
                LONG,
                3,
                p,
            ));
        }
        for quality_split in [true, false] {
            let cache = tmp.path().join(format!("cache{quality_split}"));
            let rendu = importer_par_lots_avec_reglage(&fichiers, 500, &cache, quality_split);
            let lignes: std::collections::BTreeSet<i64> = rendu
                .iter()
                .filter(|(chemin, _)| chemin.contains("CD02"))
                .map(|(_, (_, _, id))| *id)
                .collect();
            assert_eq!(
                lignes.len(),
                1,
                "quality_split={quality_split} : les six pistes de CD02 se sont réparties \
                 sur {} lignes album — c'est le symptôme de #3855. Rendu : {rendu:?}",
                lignes.len()
            );
            // Et le coffret ne perd aucune piste au passage.
            assert_eq!(rendu.len(), 12, "quality_split={quality_split}");
        }
    }
}
