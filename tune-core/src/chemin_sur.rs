//! Chemins venus d'un client : normalisation LEXICALE et refus de toute
//! remontée (`..`).
//!
//! Le défaut corrigé ici : la garde « ce chemin est-il sous un dossier de
//! musique ? » était un test de PRÉFIXE — `Path::starts_with`, qui compare
//! composant par composant, ou une comparaison de texte. Les deux prennent
//! `/musique/../etc` pour un chemin sous `/musique`, puis le système de
//! fichiers résout le `..` et l'opération a lieu hors de la bibliothèque.
//!
//! Le contrôle reste lexical, sans `canonicalize` : un partage réseau lent ou
//! décroché ne doit pas faire refuser un chemin, et une piste peut ne plus
//! exister sur le disque. Les DEUX séparateurs sont reconnus quel que soit
//! l'hôte : sur POSIX, `D:\Musique\..\Windows` est un seul composant `Normal`
//! pour `std::path`, et son `..` y serait invisible.
//!
//! Un seul module, utilisé par toutes les routes concernées, pour qu'aucune
//! copie ne diverge.

use std::path::{Path, PathBuf};

/// Une adresse distante `http://` ou `https://` (après rognage), et rien
/// d'autre : ni chemin nu, ni `file://`, ni un autre schéma. C'est la seule
/// forme qu'un client puisse envoyer comme flux à jouer — une sortie AirPlay,
/// Sendspin ou locale ouvrirait un chemin nu comme un fichier de la machine.
pub fn est_une_adresse_http(valeur: &str) -> bool {
    let v = valeur.trim();
    (v.starts_with("http://") || v.starts_with("https://")) && v.len() > "https://".len()
}

/// Pourquoi un chemin est refusé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusChemin {
    /// Chemin vide.
    Vide,
    /// Un segment `..` (ou sa forme encodée `%2e%2e` restée littérale).
    Remontee,
    /// Chemin relatif là où un absolu est attendu.
    Relatif,
    /// Chemin absolu (ou préfixé d'un lecteur) là où un nom relatif est attendu.
    Absolu,
    /// Le chemin n'est sous aucune des racines autorisées.
    HorsRacines,
}

/// Un segment est-il une remontée ? `..` tel quel, ou `%2e%2e` (toute casse,
/// éventuellement mêlé : `.%2e`) resté littéral après un double encodage :
/// une couche suivante pourrait le décoder une fois de plus.
///
/// Windows retire les points et espaces finaux d'un segment : `.. `,
/// `.. .` ou `...` y valent `..`. Tout segment fait UNIQUEMENT de points et
/// d'espaces, avec au moins deux points, est donc refusé sur tous les hôtes
/// (faux positif accepté : un dossier nommé `...`).
fn est_une_remontee(segment: &str) -> bool {
    if segment == ".." {
        return true;
    }
    let bas = segment.to_ascii_lowercase();
    let decode = bas.replace("%2e", ".").replace("%20", " ");
    decode == ".."
        || (decode.chars().all(|c| c == '.' || c == ' ')
            && decode.chars().filter(|&c| c == '.').count() >= 2)
}

/// `C:` en tête d'un chemin Windows.
fn est_un_lecteur(segment: &str) -> bool {
    let o = segment.as_bytes();
    o.len() == 2 && o[0].is_ascii_alphabetic() && o[1] == b':'
}

/// Le chemin, découpé sur `/` et `\`, sans segments vides ni `.`.
/// `Err(Remontee)` au moindre `..`.
fn segments(chemin: &str) -> Result<Vec<&str>, RefusChemin> {
    let mut sortie = Vec::new();
    for s in chemin.split(['/', '\\']) {
        if s.is_empty() || s == "." {
            continue;
        }
        if est_une_remontee(s) {
            return Err(RefusChemin::Remontee);
        }
        sortie.push(s);
    }
    Ok(sortie)
}

/// Absolu au sens de l'un OU l'autre système : `/…`, `\…` (dont `\\nas\…`),
/// ou `C:…`.
fn est_absolu(chemin: &str) -> bool {
    chemin.starts_with(['/', '\\']) || chemin.split(['/', '\\']).next().is_some_and(est_un_lecteur)
}

/// Le chemin ne contient-il aucune remontée ? Refuse aussi le vide.
pub fn sans_remontee(chemin: &str) -> Result<(), RefusChemin> {
    if chemin.trim().is_empty() {
        return Err(RefusChemin::Vide);
    }
    segments(chemin).map(|_| ())
}

/// `chemin` (absolu) est-il `racine` elle-même, ou en dessous ?
///
/// Faux dès qu'un `..` apparaît d'un côté ou de l'autre, si l'un des deux est
/// relatif, ou sur un simple préfixe de NOM (`/musique2` n'est pas sous
/// `/musique`). La lettre de lecteur se compare sans casse.
pub fn sous_la_racine(chemin: &str, racine: &str) -> bool {
    if !est_absolu(chemin) || !est_absolu(racine) {
        return false;
    }
    let (Ok(c), Ok(r)) = (segments(chemin), segments(racine)) else {
        return false;
    };
    if r.len() > c.len() {
        return false;
    }
    r.iter().zip(c.iter()).enumerate().all(|(i, (a, b))| {
        if i == 0 && est_un_lecteur(a) && est_un_lecteur(b) {
            a.eq_ignore_ascii_case(b)
        } else {
            a == b
        }
    })
}

/// La première racine qui contient `chemin`, ou le refus.
pub fn racine_contenante<'a, S: AsRef<str>>(
    chemin: &str,
    racines: &'a [S],
) -> Result<&'a S, RefusChemin> {
    if chemin.trim().is_empty() {
        return Err(RefusChemin::Vide);
    }
    segments(chemin)?;
    if !est_absolu(chemin) {
        return Err(RefusChemin::Relatif);
    }
    racines
        .iter()
        .find(|r| !r.as_ref().trim().is_empty() && sous_la_racine(chemin, r.as_ref()))
        .ok_or(RefusChemin::HorsRacines)
}

/// Joint un nom RELATIF fourni par un client sous `base`, sans jamais en
/// sortir : refuse un nom vide, absolu, préfixé d'un lecteur, ou porteur d'un
/// `..` sur l'un ou l'autre séparateur.
pub fn joindre_sous(base: &Path, relatif: &str) -> Result<PathBuf, RefusChemin> {
    if relatif.trim().is_empty() {
        return Err(RefusChemin::Vide);
    }
    if est_absolu(relatif) || relatif.contains(':') {
        return Err(RefusChemin::Absolu);
    }
    let segs = segments(relatif)?;
    if segs.is_empty() {
        return Err(RefusChemin::Vide);
    }
    let mut p = base.to_path_buf();
    for s in segs {
        p.push(s);
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RACINES: &[&str] = &["/mnt/musique", r"D:\Musique"];

    #[test]
    fn un_chemin_dans_la_racine_passe() {
        assert!(racine_contenante("/mnt/musique", RACINES).is_ok());
        assert!(racine_contenante("/mnt/musique/", RACINES).is_ok());
        assert!(racine_contenante("/mnt/musique/Jazz/Miles", RACINES).is_ok());
        assert!(racine_contenante("/mnt/musique/./Jazz", RACINES).is_ok());
        assert!(racine_contenante("/mnt//musique/Jazz", RACINES).is_ok());
        assert!(racine_contenante(r"D:\Musique\Jazz", RACINES).is_ok());
        assert!(racine_contenante(r"d:\Musique\Jazz", RACINES).is_ok());
        assert!(racine_contenante("D:/Musique/Jazz", RACINES).is_ok());
        // Un nom qui CONTIENT deux points n'est pas une remontée.
        assert!(racine_contenante("/mnt/musique/..Live../a..b", RACINES).is_ok());
    }

    #[test]
    fn une_remontee_est_refusee_meme_si_elle_reste_dedans() {
        for c in [
            "/mnt/musique/../etc",
            "/mnt/musique/..",
            "/mnt/musique/Jazz/../../../etc/passwd",
            "/mnt/musique/Jazz/../Rock", // resterait dedans : refusé quand même
            "/mnt/musique/Jazz/..",
        ] {
            assert_eq!(
                racine_contenante(c, RACINES),
                Err(RefusChemin::Remontee),
                "{c}"
            );
        }
    }

    #[test]
    fn une_remontee_windows_est_refusee_sur_tout_hote() {
        for c in [
            r"D:\Musique\..\Windows",
            r"D:\Musique\..\..\Windows\System32",
            r"D:/Musique\..\Windows",
            r"/mnt/musique\..\..\etc",
        ] {
            assert_eq!(
                racine_contenante(c, RACINES),
                Err(RefusChemin::Remontee),
                "{c}"
            );
        }
    }

    /// N3 — sous Windows, `.. `, `...` et `.. .` remontent comme `..`.
    #[test]
    fn une_remontee_windows_a_points_ou_espaces_finaux_est_refusee() {
        for c in [
            r"C:\Musique\.. \Windows",
            r"C:\Musique\...\Windows",
            "/musique/.. ./etc",
            "/musique/. ./etc",
            "/musique/%2e%2e%20/etc",
        ] {
            assert_eq!(sans_remontee(c), Err(RefusChemin::Remontee), "{c:?}");
        }
        // Contre-épreuve : un nom qui COMMENCE par des points reste un nom.
        for c in [
            "/musique/..soundtracks",
            "/musique/... and more",
            "/musique/. /x",
        ] {
            assert_eq!(sans_remontee(c), Ok(()), "{c:?}");
        }
    }

    #[test]
    fn une_remontee_encodee_est_refusee() {
        // Décodée une fois par l'extracteur de requête : c'est un `..` clair.
        let decode = urlencoding::decode("/mnt/musique/%2e%2e/etc").unwrap();
        assert_eq!(
            racine_contenante(&decode, RACINES),
            Err(RefusChemin::Remontee)
        );
        let decode = urlencoding::decode("%2Fmnt%2Fmusique%2F%2E%2E%2Fetc").unwrap();
        assert_eq!(
            racine_contenante(&decode, RACINES),
            Err(RefusChemin::Remontee)
        );
        // Double encodage : `%2e%2e` reste littéral, refusé aussi.
        for c in [
            "/mnt/musique/%2e%2e/etc",
            "/mnt/musique/%2E%2E/etc",
            "/mnt/musique/.%2e/etc",
            "/mnt/musique/%2e./etc",
        ] {
            assert_eq!(
                racine_contenante(c, RACINES),
                Err(RefusChemin::Remontee),
                "{c}"
            );
        }
    }

    #[test]
    fn hors_racines_ou_relatif_est_refuse() {
        assert_eq!(
            racine_contenante("/etc", RACINES),
            Err(RefusChemin::HorsRacines)
        );
        // Préfixe de NOM : voisin, pas dessous.
        assert_eq!(
            racine_contenante("/mnt/musique2/x", RACINES),
            Err(RefusChemin::HorsRacines)
        );
        assert_eq!(
            racine_contenante(r"D:\Musique2", RACINES),
            Err(RefusChemin::HorsRacines)
        );
        assert_eq!(
            racine_contenante(r"C:\Musique", RACINES),
            Err(RefusChemin::HorsRacines)
        );
        assert_eq!(
            racine_contenante("mnt/musique/x", RACINES),
            Err(RefusChemin::Relatif)
        );
        assert_eq!(racine_contenante("  ", RACINES), Err(RefusChemin::Vide));
        let aucune: &[&str] = &[];
        assert_eq!(
            racine_contenante("/mnt/musique", aucune),
            Err(RefusChemin::HorsRacines)
        );
        // Une racine vide ne couvre rien.
        assert_eq!(
            racine_contenante("/etc", &[""]),
            Err(RefusChemin::HorsRacines)
        );
    }

    #[test]
    fn une_racine_porteuse_de_remontee_ne_couvre_rien() {
        assert!(!sous_la_racine("/etc/passwd", "/mnt/musique/../.."));
    }

    #[test]
    fn joindre_sous_reste_dans_la_base() {
        let base = Path::new("/var/cache/tune/artwork");
        assert_eq!(
            joindre_sous(base, "8865c2f2e1a6f89c34ab584ec5b8e158.jpg").unwrap(),
            base.join("8865c2f2e1a6f89c34ab584ec5b8e158.jpg")
        );
        for r in [
            "../../../etc/passwd",
            "..",
            "a/../../b",
            r"..\..\Windows\win.ini",
            "%2e%2e/etc",
            "%2E%2E%2Fetc", // `%2F` littéral : un seul segment `%2E%2E%2Fetc`, mais…
        ] {
            let refus = joindre_sous(base, r);
            if r == "%2E%2E%2Fetc" {
                // …qui reste DANS la base : nom de fichier littéral, sans danger.
                assert!(refus.is_ok_and(|p| p.starts_with(base)), "{r}");
            } else {
                assert_eq!(refus, Err(RefusChemin::Remontee), "{r}");
            }
        }
        for r in [
            "/etc/passwd",
            r"\etc\passwd",
            r"C:\Windows",
            "C:x",
            r"\\nas\partage",
        ] {
            assert_eq!(joindre_sous(base, r), Err(RefusChemin::Absolu), "{r}");
        }
        assert_eq!(joindre_sous(base, ""), Err(RefusChemin::Vide));
        assert_eq!(joindre_sous(base, "./"), Err(RefusChemin::Vide));
    }
}
