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

    #[cfg(unix)]
    #[test]
    fn sans_emplacement_tout_est_refuse() {
        assert_eq!(
            verifier("/tmp", &[]).unwrap_err().motif,
            "hors_bibliotheque"
        );
    }
}
