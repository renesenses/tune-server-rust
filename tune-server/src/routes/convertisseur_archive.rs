//! #5482 — l'archive téléchargée porte le nom de l'album.
//!
//! Xavier Joly (Reivax66, 0.9.168) : « Renommer le zip par le nom de l'album
//! avant téléchargement pour plus de lisibilité. »
//!
//! L'archive s'appelait `tune-convert-<uuid>.zip`. Elle s'appelle désormais
//! « Artiste - Album (FORMAT).zip », avec des replis pour plusieurs albums ou
//! des fichiers hors bibliothèque, nettoyé pour les trois systèmes :
//!
//! - **Windows** : `< > : " / \ | ? *` et les caractères de contrôle sont
//!   interdits ; un nom ne finit ni par un point ni par une espace ; `CON`,
//!   `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9` sont réservés, même
//!   suivis d'une extension.
//! - **macOS** : `:` est le séparateur historique du Finder, `/` celui du
//!   système ; le nom est rendu en NFC (APFS conserve la forme reçue, et un
//!   nom NFD venu d'un tag se compare mal ailleurs).
//! - **Linux** : `/` et NUL ; un nom commençant par `.` serait caché ; 255
//!   octets au plus. On borne à 180 octets pour laisser au navigateur la place
//!   d'un « (1) ».
//!
//! `Content-Disposition` porte deux noms (RFC 6266, RFC 5987) : `filename=`
//! en ASCII pour les clients anciens, et `filename*=UTF-8''…` que tous les
//! navigateurs actuels préfèrent.

use unicode_normalization::UnicodeNormalization;

use super::pistes::PisteResolue;

/// Longueur maximale du nom, en octets UTF-8, extension comprise.
const OCTETS_MAX: usize = 180;

/// Le libellé du format entre parenthèses : « FLAC 24-176.4 », « MP3 320 »,
/// « ALAC 16-44.1 ». Tiré de la DEMANDE : deux conversions du même album dans
/// deux formats ne portent pas le même nom.
pub(super) fn libelle_du_format(
    format: &str,
    quality: Option<&str>,
    sample_rate: Option<u32>,
    bit_depth: Option<u16>,
) -> String {
    let mut libelle = format.to_uppercase();
    match format {
        "mp3" | "aac" | "opus" => {
            if let Some(q) = quality.filter(|q| !q.trim().is_empty()) {
                libelle.push(' ');
                libelle.push_str(&q.trim().to_uppercase());
            }
        }
        _ => match (bit_depth, sample_rate) {
            (Some(bd), Some(sr)) => libelle.push_str(&format!(" {bd}-{}", khz(sr))),
            (Some(bd), None) => libelle.push_str(&format!(" {bd}")),
            (None, Some(sr)) => libelle.push_str(&format!(" {}", khz(sr))),
            (None, None) => {}
        },
    }
    libelle
}

/// 44 100 → « 44.1 », 192 000 → « 192 ». Point décimal : c'est un nom de
/// fichier, pas un texte traduit.
fn khz(sr: u32) -> String {
    if sr.is_multiple_of(1000) {
        (sr / 1000).to_string()
    } else {
        let s = format!("{:.1}", f64::from(sr) / 1000.0);
        s.trim_end_matches(".0").to_string()
    }
}

/// Le nom de l'archive (avec `.zip`), nettoyé.
pub(super) fn nom_de_l_archive(pistes: &[PisteResolue], libelle_format: &str) -> String {
    // Albums distincts, dans l'ordre d'apparition.
    let mut albums: Vec<(Option<&str>, &str)> = Vec::new();
    for p in pistes {
        if let Some(album) = p.album.as_deref() {
            let cle = (p.artiste.as_deref(), album);
            if !albums.contains(&cle) {
                albums.push(cle);
            }
        }
    }
    let base = match albums.as_slice() {
        [] => "Tune - conversion".to_string(),
        [(Some(artiste), album)] => format!("{artiste} - {album}"),
        [(None, album)] => (*album).to_string(),
        plusieurs => {
            let premier = plusieurs[0].0;
            let meme_artiste = premier.is_some() && plusieurs.iter().all(|(a, _)| *a == premier);
            match (meme_artiste, premier) {
                (true, Some(artiste)) => format!("{artiste} - {} albums", plusieurs.len()),
                _ => format!("Tune - {} albums", plusieurs.len()),
            }
        }
    };
    nettoyer(&format!("{base} ({libelle_format})"), "zip")
}

/// Nettoie `base` pour Windows, macOS et Linux, borne sa longueur, et ajoute
/// `.ext`.
pub(super) fn nettoyer(base: &str, ext: &str) -> String {
    let mut propre = String::with_capacity(base.len());
    for c in base.nfc() {
        match c {
            '/' | '\\' | '|' => propre.push('-'),
            ':' => propre.push_str(" - "),
            '"' => propre.push('\''),
            '<' | '>' | '?' | '*' => {}
            // Une tabulation ou un saut de ligne dans un tag : une espace.
            c if c.is_whitespace() => propre.push(' '),
            c if c.is_control() => {}
            c => propre.push(c),
        }
    }
    // Espaces multiples (dont celles qu'a créées « : ») ramenées à une seule.
    let mut propre = propre.split_whitespace().collect::<Vec<_>>().join(" ");
    propre = propre
        .trim_start_matches(['.', ' '])
        .trim_end_matches(['.', ' '])
        .to_string();
    if propre.is_empty() {
        propre = "Tune".to_string();
    }
    let radical = propre.split('.').next().unwrap_or("").trim().to_uppercase();
    let reserve = matches!(radical.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((radical.starts_with("COM") || radical.starts_with("LPT"))
            && radical.len() == 4
            && radical[3..].chars().all(|c| ('1'..='9').contains(&c)));
    if reserve {
        propre.insert(0, '_');
    }
    let place = OCTETS_MAX - ext.len() - 1;
    if propre.len() > place {
        let mut coupe = place;
        while !propre.is_char_boundary(coupe) {
            coupe -= 1;
        }
        propre.truncate(coupe);
        propre = propre.trim_end_matches(['.', ' ']).to_string();
    }
    format!("{propre}.{ext}")
}

/// La valeur de `Content-Disposition` : repli ASCII et nom UTF-8.
pub(super) fn content_disposition(nom: &str) -> String {
    // Repli ASCII : les lettres accentuées perdent leur accent (NFD, puis on
    // retire les marques), le reste de l'hors-ASCII devient « _ ».
    let ascii: String = nom
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .map(|c| {
            if c.is_ascii() && !c.is_ascii_control() && c != '"' && c != '\\' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let mut encode = String::with_capacity(nom.len() * 3);
    for octet in nom.as_bytes() {
        // attr-char de la RFC 5987.
        if octet.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(octet) {
            encode.push(char::from(*octet));
        } else {
            encode.push_str(&format!("%{octet:02X}"));
        }
    }
    format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encode}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn piste(artiste: Option<&str>, album: Option<&str>) -> PisteResolue {
        PisteResolue {
            chemin: PathBuf::from("/m/x.flac"),
            album: album.map(str::to_string),
            artiste: artiste.map(str::to_string),
        }
    }

    #[test]
    fn un_album_donne_artiste_tiret_album() {
        let p = [
            piste(Some("Miles Davis"), Some("Miles Smiles")),
            piste(Some("Miles Davis"), Some("Miles Smiles")),
        ];
        let f = libelle_du_format("flac", Some("5"), Some(176_400), Some(24));
        assert_eq!(f, "FLAC 24-176.4");
        assert_eq!(
            nom_de_l_archive(&p, &f),
            "Miles Davis - Miles Smiles (FLAC 24-176.4).zip"
        );
    }

    #[test]
    fn les_libelles_de_format_distinguent_les_preglages() {
        assert_eq!(
            libelle_du_format("flac", Some("5"), Some(44_100), Some(16)),
            "FLAC 16-44.1"
        );
        assert_eq!(
            libelle_du_format("flac", Some("5"), None, Some(24)),
            "FLAC 24"
        );
        assert_eq!(
            libelle_du_format("flac", None, Some(192_000), Some(24)),
            "FLAC 24-192"
        );
        assert_eq!(libelle_du_format("mp3", Some("v0"), None, None), "MP3 V0");
        assert_eq!(
            libelle_du_format("opus", Some("128"), None, None),
            "OPUS 128"
        );
        assert_eq!(libelle_du_format("wav", None, None, None), "WAV");
    }

    #[test]
    fn plusieurs_albums_se_comptent() {
        let meme = [
            piste(Some("Miles Davis"), Some("Miles Smiles")),
            piste(Some("Miles Davis"), Some("Nefertiti")),
        ];
        assert_eq!(
            nom_de_l_archive(&meme, "MP3 320"),
            "Miles Davis - 2 albums (MP3 320).zip"
        );
        let divers = [
            piste(Some("Miles Davis"), Some("Miles Smiles")),
            piste(Some("Bill Evans"), Some("Explorations")),
            piste(None, None),
        ];
        assert_eq!(
            nom_de_l_archive(&divers, "WAV"),
            "Tune - 2 albums (WAV).zip"
        );
        assert_eq!(
            nom_de_l_archive(&[piste(None, None)], "WAV"),
            "Tune - conversion (WAV).zip"
        );
    }

    #[test]
    fn le_nom_est_nettoye_pour_les_trois_systemes() {
        assert_eq!(
            nettoyer("AC/DC - Back in Black", "zip"),
            "AC-DC - Back in Black.zip"
        );
        assert_eq!(
            nettoyer("Star Wars: A New Hope <OST> \"Live\"?*|x\\y", "zip"),
            "Star Wars - A New Hope OST 'Live'-x-y.zip"
        );
        assert_eq!(nettoyer("Fin de phrase...  ", "zip"), "Fin de phrase.zip");
        assert_eq!(nettoyer(".cache", "zip"), "cache.zip");
        assert_eq!(nettoyer("CON", "zip"), "_CON.zip");
        assert_eq!(nettoyer("com1.tar", "zip"), "_com1.tar.zip");
        assert_eq!(nettoyer("COM10", "zip"), "COM10.zip");
        assert_eq!(nettoyer("a\u{0}b\u{7}c\td", "zip"), "abc d.zip");
        assert_eq!(nettoyer("///", "zip"), "---.zip");
        assert_eq!(nettoyer("   ", "zip"), "Tune.zip");
        // NFD (un tag écrit sous macOS) → NFC.
        assert_eq!(nettoyer("Beyonce\u{0301}", "zip"), "Beyoncé.zip");
    }

    #[test]
    fn un_nom_trop_long_est_coupe_sur_une_frontiere_de_caractere() {
        let long = "é".repeat(200);
        let n = nettoyer(&long, "zip");
        assert!(n.len() <= OCTETS_MAX, "{} octets", n.len());
        assert!(n.ends_with(".zip"));
        assert!(n.trim_end_matches(".zip").chars().all(|c| c == 'é'));
    }

    #[test]
    fn content_disposition_porte_un_repli_ascii_et_le_nom_utf8() {
        let v = content_disposition("Beyoncé - Lemonade (FLAC 24).zip");
        assert_eq!(
            v,
            "attachment; filename=\"Beyonce - Lemonade (FLAC 24).zip\"; \
             filename*=UTF-8''Beyonc%C3%A9%20-%20Lemonade%20%28FLAC%2024%29.zip"
        );
        // Un en-tête HTTP n'accepte que de l'ASCII visible : la valeur doit
        // passer tel quel dans un `HeaderValue`.
        assert!(axum::http::HeaderValue::from_str(&v).is_ok());
        let v = content_disposition("坂本龍一 - async (FLAC).zip");
        assert!(
            v.starts_with("attachment; filename=\"____ - async (FLAC).zip\";"),
            "{v}"
        );
        assert!(axum::http::HeaderValue::from_str(&v).is_ok());
    }
}
