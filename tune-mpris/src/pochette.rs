//! La pochette de la piste en cours.
//!
//! Le serveur publie un `cover_path` sous quatre formes (même lecture que
//! `urlDePochette` du mini-lecteur et `artworkUrl` du client web) : une URL
//! distante, un chemin déjà sous `/api/`, un condensat ou un nom de fichier
//! du cache.
//!
//! MPRIS la publie en `mpris:artUrl`. GNOME et KDE la chargent eux-mêmes et ne
//! savent pas porter de jeton, alors que la route de pochette est derrière
//! l'authentification quand celle-ci est active : le compagnon la télécharge
//! avec son jeton et publie un `file://` du cache local.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// Combien de pochettes le cache garde.
pub const POCHETTES_GARDEES: usize = 32;

/// URL absolue de la pochette, ou `None`.
pub fn url_de_pochette(serveur: &str, chemin: &str) -> Option<String> {
    let c = chemin.trim();
    if c.is_empty() {
        return None;
    }
    let minuscule = c.to_ascii_lowercase();
    if minuscule.starts_with("http://") || minuscule.starts_with("https://") {
        return Some(c.to_string());
    }
    if c.starts_with("/api/") {
        return Some(format!("{serveur}{c}"));
    }
    let fichier = c.rsplit('/').next().filter(|f| !f.is_empty())?;
    Some(format!(
        "{serveur}/api/v1/library/artwork/{}",
        encoder_segment(fichier)
    ))
}

/// Encodage d'un segment de chemin (`encodeURIComponent`).
fn encoder_segment(s: &str) -> String {
    let mut sortie = String::with_capacity(s.len());
    for octet in s.bytes() {
        if octet.is_ascii_alphanumeric() || b"-_.!~*'()".contains(&octet) {
            sortie.push(octet as char);
        } else {
            sortie.push_str(&format!("%{octet:02X}"));
        }
    }
    sortie
}

/// Nom du fichier de cache d'une URL : stable, sans caractère dangereux.
pub fn nom_en_cache(url: &str, type_mime: Option<&str>) -> String {
    let mut h = DefaultHasher::new();
    url.hash(&mut h);
    let ext = match type_mime.map(|t| t.split(';').next().unwrap_or("").trim()) {
        Some("image/png") => "png",
        Some("image/webp") => "webp",
        Some("image/gif") => "gif",
        _ => "jpg",
    };
    format!("{:016x}.{ext}", h.finish())
}

/// Le fichier déjà en cache pour cette URL, quel que soit son type.
pub fn deja_en_cache(dossier: &Path, url: &str) -> Option<PathBuf> {
    let nom = nom_en_cache(url, None);
    let base = nom.trim_end_matches(".jpg");
    ["jpg", "png", "webp", "gif"]
        .iter()
        .map(|ext| dossier.join(format!("{base}.{ext}")))
        .find(|c| c.is_file())
}

/// Le dossier du cache : `$XDG_CACHE_HOME/tune-mpris`, sinon `~/.cache/tune-mpris`.
pub fn dossier_de_cache() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("XDG_CACHE_HOME").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(d).join("tune-mpris"));
    }
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(|h| PathBuf::from(h).join(".cache").join("tune-mpris"))
}

/// `file://` d'un chemin absolu, octets hors ASCII sûr encodés.
pub fn url_de_fichier(chemin: &Path) -> String {
    let brut = chemin.to_string_lossy();
    let mut sortie = String::from("file://");
    for octet in brut.bytes() {
        if octet.is_ascii_alphanumeric() || b"/-_.~".contains(&octet) {
            sortie.push(octet as char);
        } else {
            sortie.push_str(&format!("%{octet:02X}"));
        }
    }
    sortie
}

/// Écrit la pochette dans le cache et retire les plus anciennes au-delà de
/// [`POCHETTES_GARDEES`]. Rend le chemin écrit.
pub fn ranger(dossier: &Path, nom: &str, octets: &[u8]) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dossier)?;
    let chemin = dossier.join(nom);
    let provisoire = dossier.join(format!(".{nom}.part"));
    let ecrit =
        std::fs::write(&provisoire, octets).and_then(|()| std::fs::rename(&provisoire, &chemin));
    if let Err(e) = ecrit {
        let _ = std::fs::remove_file(&provisoire);
        return Err(e);
    }
    elaguer(dossier, POCHETTES_GARDEES);
    Ok(chemin)
}

/// Un `.part` plus vieux que ça est le reste d'une écriture interrompue.
const PART_ABANDONNE: std::time::Duration = std::time::Duration::from_secs(60);

fn elaguer(dossier: &Path, garder: usize) {
    retirer_les_part_abandonnes(dossier, PART_ABANDONNE);
    let Ok(lecture) = std::fs::read_dir(dossier) else {
        return;
    };
    let mut fichiers: Vec<(std::time::SystemTime, PathBuf)> = lecture
        .filter_map(Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if fichiers.len() <= garder {
        return;
    }
    fichiers.sort_by_key(|f| std::cmp::Reverse(f.0));
    for (_, chemin) in fichiers.into_iter().skip(garder) {
        let _ = std::fs::remove_file(chemin);
    }
}

fn retirer_les_part_abandonnes(dossier: &Path, age: std::time::Duration) {
    let Ok(lecture) = std::fs::read_dir(dossier) else {
        return;
    };
    let maintenant = std::time::SystemTime::now();
    for entree in lecture.filter_map(Result::ok) {
        let nom = entree.file_name().to_string_lossy().to_string();
        if !(nom.starts_with('.') && nom.ends_with(".part")) {
            continue;
        }
        let vieux = entree
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| maintenant.duration_since(t).ok())
            .is_some_and(|d| d >= age);
        if vieux {
            let _ = std::fs::remove_file(entree.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: &str = "http://nas:8888";

    #[test]
    fn les_quatre_formes_du_cover_path() {
        assert_eq!(
            url_de_pochette(S, "https://static.qobuz.com/a.jpg").as_deref(),
            Some("https://static.qobuz.com/a.jpg")
        );
        assert_eq!(
            url_de_pochette(S, "/api/v1/library/artwork/abc").as_deref(),
            Some("http://nas:8888/api/v1/library/artwork/abc")
        );
        assert_eq!(
            url_de_pochette(S, "0123abcd").as_deref(),
            Some("http://nas:8888/api/v1/library/artwork/0123abcd")
        );
        assert_eq!(
            url_de_pochette(S, "/var/cache/tune/artwork/ab cd.jpg").as_deref(),
            Some("http://nas:8888/api/v1/library/artwork/ab%20cd.jpg")
        );
        assert_eq!(url_de_pochette(S, "  "), None);
        assert_eq!(url_de_pochette(S, "/var/cache/"), None);
    }

    #[test]
    fn le_nom_en_cache_est_stable_et_suit_le_type() {
        let a = nom_en_cache("http://x/1", Some("image/png"));
        assert_eq!(
            a,
            nom_en_cache("http://x/1", Some("image/png; charset=binary"))
        );
        assert!(a.ends_with(".png"));
        assert!(nom_en_cache("http://x/1", None).ends_with(".jpg"));
        assert_ne!(
            nom_en_cache("http://x/1", None),
            nom_en_cache("http://x/2", None)
        );
        assert!(!a.contains('/'));
    }

    #[test]
    fn une_pochette_rangee_se_retrouve_quel_que_soit_son_type() {
        let racine = tempfile::tempdir().unwrap();
        let dossier = racine.path().join("cache");
        assert_eq!(deja_en_cache(&dossier, "http://x/p"), None);
        let ecrit = ranger(
            &dossier,
            &nom_en_cache("http://x/p", Some("image/png")),
            b"png",
        )
        .unwrap();
        assert_eq!(deja_en_cache(&dossier, "http://x/p"), Some(ecrit));
        assert_eq!(deja_en_cache(&dossier, "http://x/autre"), None);
    }

    #[test]
    fn un_part_abandonne_est_retire_un_part_frais_garde() {
        let racine = tempfile::tempdir().unwrap();
        let dossier = racine.path().join("cache");
        std::fs::create_dir_all(&dossier).unwrap();
        let vieux = dossier.join(".a.jpg.part");
        let frais = dossier.join(".b.jpg.part");
        std::fs::write(&vieux, b"x").unwrap();
        std::fs::write(&frais, b"x").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&vieux)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(3600))
            .unwrap();
        retirer_les_part_abandonnes(&dossier, PART_ABANDONNE);
        assert!(!vieux.exists());
        assert!(frais.exists());
    }

    #[test]
    fn url_de_fichier_encode_espaces_et_accents() {
        assert_eq!(
            url_de_fichier(Path::new("/home/jé/a b.jpg")),
            "file:///home/j%C3%A9/a%20b.jpg"
        );
    }

    #[test]
    fn le_cache_garde_les_plus_recentes() {
        let racine = tempfile::tempdir().unwrap();
        let dossier = racine.path().join("cache");
        for i in 0..(POCHETTES_GARDEES + 3) {
            ranger(&dossier, &format!("{i:03}.jpg"), b"x").unwrap();
            // L'ordre se lit sur la date de modification.
            let t = std::time::SystemTime::UNIX_EPOCH
                + std::time::Duration::from_secs(1_000 + i as u64);
            let f = std::fs::File::options()
                .write(true)
                .open(dossier.join(format!("{i:03}.jpg")))
                .unwrap();
            f.set_modified(t).unwrap();
        }
        elaguer(&dossier, POCHETTES_GARDEES);
        let restants = std::fs::read_dir(&dossier).unwrap().count();
        assert_eq!(restants, POCHETTES_GARDEES);
        assert!(!dossier.join("000.jpg").exists(), "la plus ancienne part");
        assert!(
            dossier
                .join(format!("{:03}.jpg", POCHETTES_GARDEES + 2))
                .exists()
        );
    }
}
