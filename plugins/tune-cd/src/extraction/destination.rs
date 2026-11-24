//! Où vont les fichiers (#2466) : un dossier à l'intérieur d'un emplacement
//! de la bibliothèque (`music_dirs`), puis `Artiste/Album/NN - Titre.ext`.
//!
//! Les noms viennent de MusicBrainz ou de l'utilisateur : ils sont rendus
//! sûrs sur TOUS les systèmes (Windows est le plus strict), et ne peuvent
//! jamais sortir du dossier choisi — ni séparateur, ni `..`, ni nom réservé.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;

/// Réglage : la destination choisie (un chemin absolu, dans un emplacement).
pub const CLE_DESTINATION: &str = "cd_extraction_destination";
/// Réglage : le format par défaut (`flac` ou `wav`).
pub const CLE_FORMAT: &str = "cd_extraction_format";

/// Longueur maximale d'un nom (en caractères), sous les 255 octets des
/// systèmes de fichiers courants même en UTF-8 large.
pub const LONGUEUR_MAX: usize = 100;

const INTERDITS: &[char] = &['<', '>', ':', '"', '/', '\\', '|', '?', '*'];
const RESERVES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// Un nom de fichier ou de dossier sûr, `repli` s'il ne reste rien.
pub fn nom_sur(texte: &str, repli: &str) -> String {
    let remplace: String = texte
        .chars()
        .map(|c| {
            if c.is_control() || INTERDITS.contains(&c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut nom: String = rogner(&remplace).chars().take(LONGUEUR_MAX).collect();
    nom = rogner(&nom).to_string();
    if nom.is_empty() || nom.chars().all(|c| c == '_') {
        return repli.to_string();
    }
    let racine = nom
        .split('.')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_uppercase();
    if RESERVES.contains(&racine.as_str()) {
        nom.insert(0, '_');
    }
    nom
}

/// Sans espaces autour, sans point ni espace final (Windows les retire),
/// sans point initial (ni fichier caché, ni `..`).
fn rogner(s: &str) -> &str {
    s.trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .trim()
}

/// `Artiste/Album/NN - Titre.ext`. Un disque d'un coffret (`disques > 1`)
/// préfixe son numéro (`2-05 - Titre`) : deux disques du même album ne
/// s'écrasent pas.
pub fn chemin_relatif(
    artiste: &str,
    album: &str,
    numero: u8,
    titre: &str,
    disque: u32,
    disques: u32,
    extension: &str,
) -> PathBuf {
    let piste = if disques > 1 {
        format!("{disque}-{numero:02}")
    } else {
        format!("{numero:02}")
    };
    let fichier = format!(
        "{piste} - {}.{extension}",
        nom_sur(titre, &format!("Piste {numero:02}"))
    );
    PathBuf::from(nom_sur(artiste, "Artiste inconnu"))
        .join(nom_sur(album, "Album inconnu"))
        .join(fichier)
}

/// Les emplacements de la bibliothèque, dans l'ordre des réglages.
pub fn emplacements(db: &dyn DbBackend) -> Vec<String> {
    tune_core::library::exemplaires::dossiers_de_musique(db)
}

/// Pourquoi une destination est refusée : un motif stable et un message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refus {
    pub motif: &'static str,
    pub message: String,
}

fn refus(motif: &'static str, message: impl Into<String>) -> Refus {
    Refus {
        motif,
        message: message.into(),
    }
}

/// Vérifie `demande` : absolue, sans `.` ni `..`, à l'intérieur d'un des
/// `emplacements` — y compris une fois les liens symboliques résolus (un
/// lien posé dans la bibliothèque ne mène pas ailleurs). Le dossier peut ne
/// pas exister encore : c'est son plus proche parent existant qui est
/// vérifié.
pub fn verifier(demande: &str, emplacements: &[String]) -> Result<PathBuf, Refus> {
    let brut = demande.trim();
    if brut.is_empty() {
        return Err(refus("destination_vide", "La destination est vide."));
    }
    if brut.chars().any(char::is_control) {
        return Err(refus(
            "destination_invalide",
            "La destination contient un caractère de contrôle.",
        ));
    }
    let chemin = PathBuf::from(tune_core::scanner::walker::normalize_path(brut));
    if !chemin.is_absolute() {
        return Err(refus(
            "destination_relative",
            "La destination doit être un chemin absolu.",
        ));
    }
    // Sur le texte, pas sur `components()` : celui-ci efface les « . »
    // intermédiaires sans le dire.
    if brut.split(['/', '\\']).any(|seg| seg == "." || seg == "..") {
        return Err(refus(
            "destination_invalide",
            "La destination ne peut pas contenir « . » ni « .. ».",
        ));
    }
    let Some(racine) = emplacements
        .iter()
        .map(Path::new)
        .find(|r| chemin.starts_with(r))
    else {
        return Err(refus(
            "hors_bibliotheque",
            "La destination doit se trouver dans un emplacement de la bibliothèque.",
        ));
    };
    let racine_reelle = std::fs::canonicalize(racine).map_err(|e| {
        refus(
            "emplacement_inaccessible",
            format!("L'emplacement de la bibliothèque est inaccessible : {e}"),
        )
    })?;
    let mut existant = chemin.as_path();
    while !existant.exists() {
        existant = existant.parent().unwrap_or(racine);
    }
    let existant_reel = std::fs::canonicalize(existant).map_err(|e| {
        refus(
            "destination_invalide",
            format!("La destination est inaccessible : {e}"),
        )
    })?;
    if !existant_reel.starts_with(&racine_reelle) {
        return Err(refus(
            "hors_bibliotheque",
            "La destination sort de la bibliothèque par un lien symbolique.",
        ));
    }
    if existant == chemin && !existant_reel.is_dir() {
        return Err(refus(
            "destination_invalide",
            "La destination n'est pas un dossier.",
        ));
    }
    Ok(chemin)
}

/// Un sous-dossier proposé au choix de la destination (fil forum 3036).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct SousDossier {
    pub nom: String,
    pub chemin: String,
}

/// Ce que montre le choix du dossier : le dossier `courant` (`None` : la
/// liste des emplacements eux-mêmes), son `parent` (`None` en haut d'un
/// emplacement : on n'en remonte pas) et ses sous-dossiers.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ListeDossiers {
    pub courant: Option<String>,
    pub parent: Option<String>,
    pub dossiers: Vec<SousDossier>,
}

/// Fil forum 3036 — les sous-dossiers de `demande` où ranger une
/// extraction, pour descendre aussi bas qu'on veut SOUS un emplacement de
/// la bibliothèque, et jamais au-dessus ni à côté.
///
/// `demande` passe d'abord par [`verifier`] (absolue, sans `.` ni `..`,
/// dans un emplacement une fois les liens résolus) : les refus sont les
/// mêmes que ceux du réglage, rendus AVANT toute lecture du dossier. Un
/// dossier qui n'existe pas encore se choisit (l'extraction le crée) mais
/// ne se liste pas. Les dossiers cachés et les dossiers système (corbeilles
/// de NAS…) ne sont pas proposés, ni un lien symbolique qui mène hors de
/// l'emplacement. Pas de sondage des petits-enfants : sur un partage
/// réseau, un dossier d'artistes en compte des milliers.
pub fn sous_dossiers(
    demande: Option<&str>,
    emplacements: &[String],
) -> Result<ListeDossiers, Refus> {
    let Some(demande) = demande.filter(|d| !d.trim().is_empty()) else {
        return Ok(ListeDossiers {
            courant: None,
            parent: None,
            dossiers: emplacements
                .iter()
                .map(|e| SousDossier {
                    nom: e.clone(),
                    chemin: e.clone(),
                })
                .collect(),
        });
    };
    let chemin = verifier(demande, emplacements)?;
    // Un dossier masqué (caché, système, propre à Tune) ne s'ouvre pas non
    // plus quand on le demande directement.
    if let Some(racine) = emplacements
        .iter()
        .map(|e| PathBuf::from(tune_core::scanner::walker::normalize_path(e)))
        .filter(|r| chemin.starts_with(r))
        .max_by_key(|r| r.components().count())
        && let Ok(reste) = chemin.strip_prefix(&racine)
        && reste
            .components()
            .any(|c| nom_masque(&c.as_os_str().to_string_lossy(), None))
    {
        return Err(refus(
            "dossier_masque",
            "Ce dossier est caché ou réservé : choisissez-en un autre.",
        ));
    }
    if !chemin.is_dir() {
        return Err(refus(
            "dossier_introuvable",
            "Ce dossier n'existe pas (encore) : il sera créé par l'extraction.",
        ));
    }
    let en_haut = emplacements
        .iter()
        .any(|e| Path::new(&tune_core::scanner::walker::normalize_path(e)) == chemin.as_path());
    let parent = if en_haut {
        None
    } else {
        chemin.parent().map(|p| p.to_string_lossy().to_string())
    };
    // Lecture sur le chemin CANONIQUE, vérifié dans l'emplacement : un lien
    // du chemin demandé, re-pointé entre la vérification et la lecture, ne
    // mène plus ailleurs (TOCTOU). Les chemins rendus restent exprimés sous
    // le chemin demandé, celui que `verifier` reconnaît.
    let reel = canonique_dans(&chemin, emplacements)?;
    #[cfg(test)]
    tests_course::apres_verification(&chemin);
    let entrees = std::fs::read_dir(&reel).map_err(|e| {
        refus(
            "destination_invalide",
            format!("Le dossier est illisible : {e}"),
        )
    })?;
    let mut dossiers: Vec<SousDossier> = Vec::new();
    for entree in entrees.flatten() {
        // Un nom hors UTF-8 n'est pas proposé : sa forme avec pertes (`�`)
        // désignerait un autre dossier, inexistant ou voisin.
        let Some(nom) = entree.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if nom_masque(&nom, attributs_windows(&entree)) {
            continue;
        }
        let lien = traite_comme_lien(&entree.file_type());
        if !entree.path().is_dir() {
            continue;
        }
        let Some(texte) = chemin.join(&nom).to_str().map(str::to_string) else {
            continue;
        };
        // Un lien est suivi seulement s'il reste dans l'emplacement : la
        // même vérification que la destination elle-même.
        if lien && verifier(&texte, emplacements).is_err() {
            continue;
        }
        dossiers.push(SousDossier { nom, chemin: texte });
    }
    dossiers.sort_by(|a, b| {
        a.nom
            .to_lowercase()
            .cmp(&b.nom.to_lowercase())
            .then_with(|| a.nom.cmp(&b.nom))
    });
    Ok(ListeDossiers {
        courant: Some(chemin.to_string_lossy().to_string()),
        parent,
        dossiers,
    })
}

/// Une entrée dont le type n'a pas pu être lu est-elle traitée comme un
/// lien symbolique (donc vérifiée avant d'être proposée) ?
fn traite_comme_lien(t: &std::io::Result<std::fs::FileType>) -> bool {
    t.as_ref().map_or(true, |t| t.is_symlink())
}

/// La forme canonique de `chemin`, refusée si elle sort de l'emplacement
/// qui le contient (même règle que [`verifier`], rejouée au moment de lire).
fn canonique_dans(chemin: &Path, emplacements: &[String]) -> Result<PathBuf, Refus> {
    let reel = std::fs::canonicalize(chemin).map_err(|e| {
        refus(
            "destination_invalide",
            format!("La destination est inaccessible : {e}"),
        )
    })?;
    let dedans = emplacements
        .iter()
        .map(|e| PathBuf::from(tune_core::scanner::walker::normalize_path(e)))
        .filter(|r| chemin.starts_with(r))
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .any(|r| reel.starts_with(r));
    if !dedans {
        return Err(refus(
            "hors_bibliotheque",
            "La destination sort de la bibliothèque par un lien symbolique.",
        ));
    }
    Ok(reel)
}

/// Les attributs de fichier Windows d'une entrée (`None` ailleurs).
fn attributs_windows(entree: &std::fs::DirEntry) -> Option<u32> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        entree.metadata().ok().map(|m| m.file_attributes())
    }
    #[cfg(not(windows))]
    {
        let _ = entree;
        None
    }
}

/// `lost+found` : le dossier de récupération d'ext2/3/4, jamais de la musique.
const DOSSIERS_DE_RECUPERATION: &[&str] = &["lost+found"];
/// `FILE_ATTRIBUTE_HIDDEN`.
const ATTRIBUT_CACHE: u32 = 0x2;

/// Un nom de dossier à ne pas proposer. `attributs` : les attributs de
/// fichier Windows, s'ils sont connus.
fn nom_masque(nom: &str, attributs: Option<u32>) -> bool {
    nom.starts_with('.')
        || tune_core::scanner::dossier_systeme_ignore(nom)
        || tune_core::scanner::walker::SKIP_DIRS.contains(&nom)
        || DOSSIERS_DE_RECUPERATION.contains(&nom)
        || attributs.is_some_and(|a| a & ATTRIBUT_CACHE != 0)
}

/// Le point d'injection des témoins de course : une action jouée entre la
/// vérification du chemin et la lecture du dossier.
#[cfg(test)]
pub(crate) mod tests_course {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    type Action = Box<dyn FnOnce() + Send>;
    static ACTIONS: Mutex<Vec<(PathBuf, Action)>> = Mutex::new(Vec::new());

    pub fn programmer(chemin: PathBuf, action: Action) {
        ACTIONS.lock().unwrap().push((chemin, action));
    }

    pub fn apres_verification(chemin: &Path) {
        let action = {
            let mut a = ACTIONS.lock().unwrap();
            a.iter()
                .position(|(c, _)| c == chemin)
                .map(|i| a.remove(i).1)
        };
        if let Some(action) = action {
            action();
        }
    }
}

/// #7105 — la cause d'une écriture refusée, en un motif stable.
pub fn cause_d_ecriture(e: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::PermissionDenied => "droits_refuses",
        ErrorKind::ReadOnlyFilesystem => "lecture_seule",
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded => "espace_insuffisant",
        ErrorKind::NotFound => "dossier_introuvable",
        _ => "autre",
    }
}

/// #7105 — la phrase qui nomme la cause et le chemin, suivie du détail
/// donné par le système.
pub fn message_d_ecriture(cause: &str, chemin: &Path, e: &std::io::Error) -> String {
    let c = chemin.display();
    let phrase = match cause {
        "droits_refuses" => format!(
            "Le serveur Tune n'a pas le droit d'écrire dans {c} : donnez ce droit au compte \
             qui fait tourner le serveur, ou choisissez un autre dossier."
        ),
        "lecture_seule" => format!("{c} est sur un disque en lecture seule."),
        "espace_insuffisant" => format!("Il n'y a plus de place sur le disque de {c}."),
        "dossier_introuvable" => format!("{c} n'existe pas ou a disparu."),
        _ => format!("Écriture impossible dans {c}."),
    };
    format!("{phrase} ({e})")
}

/// #7105 — le dossier existant le plus proche de `chemin` (lui-même s'il
/// existe) : celui où le serveur devra créer le reste.
fn plus_proche_existant(chemin: &Path) -> Option<&Path> {
    let mut p = Some(chemin);
    while let Some(c) = p {
        if c.is_dir() {
            return Some(c);
        }
        p = c.parent();
    }
    None
}

/// #7105 — la place libre, en octets, du disque qui porte `chemin`
/// (`None` : inconnue sur ce système).
pub fn espace_libre(chemin: &Path) -> Option<u64> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(chemin.as_os_str().as_bytes()).ok()?;
        let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY : `c` est une chaîne C valide, `s` une structure à remplir.
        if unsafe { libc::statvfs(c.as_ptr(), &mut s) } != 0 {
            return None;
        }
        #[allow(clippy::unnecessary_cast)]
        Some(s.f_bavail as u64 * s.f_frsize as u64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = chemin;
        None
    }
}

/// #7105 — avant de lancer : le serveur peut-il écrire dans `dossier`, et
/// reste-t-il `octets` sur son disque ? Un fichier témoin est créé puis
/// effacé dans le dossier existant le plus proche, sans rien laisser (les
/// dossiers de l'album sont créés par l'extraction). `libre` donne la place
/// libre (injectable : un disque plein ne se provoque pas en test).
pub fn verifier_ecriture(
    dossier: &Path,
    octets: u64,
    libre: impl Fn(&Path) -> Option<u64>,
) -> Result<(), super::ErreurTravail> {
    let Some(existant) = plus_proche_existant(dossier) else {
        let e = std::io::Error::from(std::io::ErrorKind::NotFound);
        return Err(super::ErreurTravail::ecriture(dossier, &e));
    };
    if let Err(e) = tempfile::Builder::new()
        .prefix(".tune-essai-ecriture-")
        .tempfile_in(existant)
    {
        return Err(super::ErreurTravail::ecriture(existant, &e));
    }
    if let Some(disponible) = libre(existant)
        && disponible < octets
    {
        return Err(super::ErreurTravail {
            code: "ecriture",
            message: format!(
                "Il n'y a pas assez de place sur le disque de {} : il faut environ {} Mo, \
                 il en reste {} Mo.",
                existant.display(),
                octets.div_ceil(1_000_000),
                disponible / 1_000_000
            ),
            cause: Some("espace_insuffisant"),
            chemin: Some(existant.to_string_lossy().to_string()),
        });
    }
    Ok(())
}

/// #7105 — la place que demande l'extraction de `secteurs` secteurs : le
/// PCM entier en WAV ; en FLAC, les trois quarts (un CD se compresse en
/// général de 40 à 60 %), plus une marge pour les balises et la pochette.
pub fn octets_necessaires(secteurs: u64, flac: bool) -> u64 {
    let pcm = secteurs * crate::toc::OCTETS_PAR_SECTEUR as u64;
    let donnees = if flac { pcm / 4 * 3 } else { pcm };
    donnees + 10_000_000
}

/// La destination à employer sans demande explicite : le réglage s'il est
/// encore valide, sinon le premier emplacement de la bibliothèque. `None` :
/// la bibliothèque n'a aucun emplacement.
pub fn par_defaut(db: Arc<dyn DbBackend>) -> Option<(PathBuf, &'static str)> {
    let places = emplacements(db.as_ref());
    if let Some(r) = SettingsRepo::with_backend(db)
        .get(CLE_DESTINATION)
        .ok()
        .flatten()
        && let Ok(p) = verifier(&r, &places)
    {
        return Some((p, "reglage"));
    }
    places.first().map(|p| (PathBuf::from(p), "defaut"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn les_caracteres_interdits_sous_windows_sont_remplaces() {
        assert_eq!(
            nom_sur(r#"AC/DC: <Live> "1991" |?*\"#, "x"),
            "AC_DC_ _Live_ _1991_ ____"
        );
        assert_eq!(nom_sur("Titre\u{0}\u{7}\n", "x"), "Titre___");
    }

    #[test]
    fn ni_point_final_ni_point_initial_ni_nom_vide() {
        assert_eq!(nom_sur("Vol. 2...", "x"), "Vol. 2");
        assert_eq!(nom_sur("..", "Album inconnu"), "Album inconnu");
        assert_eq!(nom_sur("../../etc", "x"), "_.._etc");
        assert_eq!(nom_sur("   ", "Artiste inconnu"), "Artiste inconnu");
        assert_eq!(nom_sur("///", "repli"), "repli");
    }

    #[test]
    fn les_noms_reserves_de_windows_sont_prefixes() {
        assert_eq!(nom_sur("CON", "x"), "_CON");
        assert_eq!(nom_sur("nul.txt", "x"), "_nul.txt");
        assert_eq!(nom_sur("Console", "x"), "Console");
    }

    #[test]
    fn un_nom_trop_long_est_coupe_sur_un_caractere() {
        let long = "é".repeat(300);
        assert_eq!(nom_sur(&long, "x").chars().count(), LONGUEUR_MAX);
    }

    #[test]
    fn le_chemin_artiste_album_numero_titre() {
        assert_eq!(
            chemin_relatif("Artiste", "Album", 3, "Titre", 1, 1, "flac"),
            PathBuf::from("Artiste")
                .join("Album")
                .join("03 - Titre.flac")
        );
        assert_eq!(
            chemin_relatif("", "", 7, "", 2, 3, "wav"),
            PathBuf::from("Artiste inconnu")
                .join("Album inconnu")
                .join("2-07 - Piste 07.wav")
        );
        // Un titre hostile ne crée aucun sous-dossier.
        let p = chemin_relatif("A", "B", 1, "../../x", 1, 1, "flac");
        assert_eq!(p.components().count(), 3);
    }

    #[cfg(unix)]
    #[test]
    fn une_destination_hostile_est_refusee() {
        let bib = tempfile::tempdir().unwrap();
        let ailleurs = tempfile::tempdir().unwrap();
        let racine = bib.path().to_string_lossy().to_string();
        let places = vec![racine.clone()];

        // Acceptées : l'emplacement lui-même, un sous-dossier à créer.
        assert!(verifier(&racine, &places).is_ok());
        assert!(verifier(&format!("{racine}/CD/Nouveaux"), &places).is_ok());

        let motif = |d: &str| verifier(d, &places).unwrap_err().motif;
        assert_eq!(motif(&format!("{racine}/../etc")), "destination_invalide");
        assert_eq!(motif(&format!("{racine}/./x")), "destination_invalide");
        assert_eq!(motif("relatif/dossier"), "destination_relative");
        assert_eq!(
            motif(&ailleurs.path().to_string_lossy()),
            "hors_bibliotheque"
        );
        assert_eq!(motif(""), "destination_vide");
        // Un préfixe de texte n'est pas un parent : `/bib-voisine` n'est
        // pas dans `/bib`.
        assert_eq!(motif(&format!("{racine}-voisine")), "hors_bibliotheque");

        // Un lien symbolique posé DANS la bibliothèque vers l'extérieur.
        let lien = bib.path().join("lien");
        std::os::unix::fs::symlink(ailleurs.path(), &lien).unwrap();
        assert_eq!(
            motif(&format!("{}/sous", lien.to_string_lossy())),
            "hors_bibliotheque"
        );

        // Un fichier n'est pas un dossier.
        std::fs::write(bib.path().join("f"), b"x").unwrap();
        assert_eq!(motif(&format!("{racine}/f")), "destination_invalide");
    }

    // ─── Durcissement du choix de dossier (fil 3036, relecture) ──────────

    fn noms(l: &ListeDossiers) -> Vec<String> {
        l.dossiers.iter().map(|d| d.nom.clone()).collect()
    }

    /// Point 1 — un type d'entrée illisible est traité comme un lien : il
    /// passe par la vérification avant d'être proposé.
    #[test]
    fn un_type_d_entree_illisible_est_traite_comme_un_lien() {
        assert!(traite_comme_lien(&Err(std::io::Error::other("illisible"))));
        let ici = std::fs::symlink_metadata(".").unwrap().file_type();
        assert!(!traite_comme_lien(&Ok(ici)));
    }

    /// Point 2 — la lecture suit le chemin CANONIQUE vérifié : un lien du
    /// chemin demandé, re-pointé vers l'extérieur juste après la
    /// vérification, ne fait rien lister d'extérieur.
    #[cfg(unix)]
    #[test]
    fn un_lien_re_pointe_apres_la_verification_ne_fait_rien_lister_d_exterieur() {
        let bib = tempfile::tempdir().unwrap();
        let ailleurs = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(bib.path().join("A/sous/Bon")).unwrap();
        std::fs::create_dir_all(ailleurs.path().join("sous/Secret")).unwrap();
        let places = vec![bib.path().to_string_lossy().to_string()];
        let lien = bib.path().join("lien");
        std::os::unix::fs::symlink(bib.path().join("A"), &lien).unwrap();
        let demande = lien.join("sous");
        let (l2, cible) = (lien.clone(), ailleurs.path().to_path_buf());
        tests_course::programmer(
            demande.clone(),
            Box::new(move || {
                std::fs::remove_file(&l2).unwrap();
                std::os::unix::fs::symlink(&cible, &l2).unwrap();
            }),
        );
        let r = sous_dossiers(Some(&demande.to_string_lossy()), &places);
        match r {
            Ok(l) => assert_eq!(noms(&l), ["Bon"], "rien d'extérieur"),
            Err(r) => assert_eq!(r.motif, "hors_bibliotheque"),
        }
    }

    /// Point 3 — un nom qui n'est pas en UTF-8 n'est pas proposé : sa
    /// conversion avec pertes désignerait un autre dossier.
    #[cfg(target_os = "linux")]
    #[test]
    fn un_nom_qui_n_est_pas_en_utf8_n_est_pas_propose() {
        use std::os::unix::ffi::OsStrExt;
        let bib = tempfile::tempdir().unwrap();
        std::fs::create_dir(bib.path().join("Bon")).unwrap();
        std::fs::create_dir(bib.path().join(std::ffi::OsStr::from_bytes(b"caf\xe9"))).unwrap();
        let racine = bib.path().to_string_lossy().to_string();
        let l = sous_dossiers(Some(&racine), std::slice::from_ref(&racine)).unwrap();
        assert_eq!(noms(&l), ["Bon"]);
    }

    /// Point 4 — ni `lost+found`, ni les dossiers propres à Tune, ni un
    /// dossier caché (nom en `.` ou attribut caché de Windows) ; et un
    /// dossier caché demandé directement est refusé.
    #[cfg(unix)]
    #[test]
    fn les_dossiers_masques_ne_sont_ni_proposes_ni_ouverts() {
        let bib = tempfile::tempdir().unwrap();
        for d in [
            "Bon",
            "lost+found",
            "duplicates",
            ".tune",
            ".cache/dedans",
            "@eaDir",
        ] {
            std::fs::create_dir_all(bib.path().join(d)).unwrap();
        }
        let racine = bib.path().to_string_lossy().to_string();
        let places = [racine.clone()];
        let l = sous_dossiers(Some(&racine), &places).unwrap();
        assert_eq!(noms(&l), ["Bon"]);
        for d in [".cache", ".cache/dedans", "lost+found", "duplicates"] {
            let r = sous_dossiers(Some(&format!("{racine}/{d}")), &places).unwrap_err();
            assert_eq!(r.motif, "dossier_masque", "{d}");
        }
        // L'attribut caché de Windows (FILE_ATTRIBUTE_HIDDEN = 0x2).
        assert!(nom_masque("Musique", Some(0x2)));
        assert!(!nom_masque("Musique", Some(0x10)));
        assert!(!nom_masque("Musique", None));
    }

    #[cfg(unix)]
    #[test]
    fn sans_emplacement_tout_est_refuse() {
        assert_eq!(
            verifier("/tmp", &[]).unwrap_err().motif,
            "hors_bibliotheque"
        );
    }
}
