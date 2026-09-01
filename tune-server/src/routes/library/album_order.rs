//! Ordre d'affichage des albums d'un dossier « Collections » (#2675).
//!
//! **Pourquoi en Rust et pas en SQL.** `GET /library/collections/{id}/albums`
//! ne fait aucune requête multi-lignes : la liste des albums est un tableau
//! d'identifiants rangé dans le réglage `collections`, et chaque album est
//! relu un par un (`AlbumRepo::get` → `WHERE a.id = ?`). Il n'y a donc aucun
//! `ORDER BY` où se raccrocher. Et même s'il y en avait un : `ORDER BY
//! LOWER(a.title)` ne rend pas le même ordre sur SQLite et sur PostgreSQL —
//! `LOWER` de SQLite ne replie que l'ASCII (« Édith » reste « Édith ») et la
//! collation PG dépend du `lc_collate` de l'installation. Trier ici garantit
//! le MÊME ordre sur les deux moteurs, quelles que soient leurs collations.
//!
//! **Portée.** Ce module n'est utilisé que par `collections.rs`. La
//! Bibliothèque et Oxygen trient en SQL (`album_repo::list_paged`,
//! `ORDER BY`), et rien ici ne les touche.

use tune_core::db::models::Album;
use unicode_normalization::UnicodeNormalization;

/// Critère de tri demandé par le client (`?sort=`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(super) enum CollectionSort {
    /// Artiste, puis année, puis titre. Le défaut : c'est ce que demande le
    /// testeur (« à partir du nom de l'artiste », fil 1591) et c'est le même
    /// tuple que le tri « artiste » de la Bibliothèque
    /// (`album_repo.rs`, `"artist" => LOWER(ar.name), a.year, LOWER(a.title), a.id`).
    #[default]
    Artist,
    /// Titre d'album, puis artiste.
    Title,
    /// Année, puis artiste, puis titre.
    Year,
    /// L'ordre d'ajout au dossier — le comportement historique, conservé pour
    /// qui a monté son dossier comme une séquence d'écoute.
    Added,
}

impl CollectionSort {
    /// Une valeur inconnue retombe sur le défaut plutôt que de rendre un ordre
    /// arbitraire : un client qui se trompe de mot-clé voit un ordre trié, pas
    /// l'ordre d'insertion déguisé en tri.
    pub(super) fn parse(raw: Option<&str>) -> Self {
        match raw.map(str::trim).unwrap_or_default() {
            "title" | "album" => Self::Title,
            "year" => Self::Year,
            "added" | "added_at" | "none" => Self::Added,
            _ => Self::Artist,
        }
    }
}

/// Un fragment de clé de tri : une suite de chiffres se compare comme un
/// nombre, le reste comme du texte replié.
///
/// L'ordre dérivé place `Num` avant `Text` (les chiffres avant les lettres,
/// comme en ASCII) et compare deux `Num` d'abord par leur longueur *sans
/// zéros de tête*, puis lexicographiquement — ce qui est exactement l'ordre
/// numérique, sans risque de débordement sur un « album » nommé avec
/// quarante chiffres.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum KeyPart {
    Num(usize, String),
    Text(String),
}

/// Replie accents et casse : « Édith » et « edith » deviennent la même clé.
///
/// NFD décompose `É` en `E` + accent aigu combinant, que l'on jette ; les
/// lettres sans décomposition canonique (`Ø`, `Æ`, `ß`) restent telles
/// quelles et se comparent après l'ASCII — assumé.
fn fold(s: &str) -> String {
    s.nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Range le tampon courant en fragment de clé et le vide.
fn push_part(buf: &mut String, digits: bool, parts: &mut Vec<KeyPart>) {
    if buf.is_empty() {
        return;
    }
    if digits {
        let trimmed = buf.trim_start_matches('0');
        parts.push(KeyPart::Num(trimmed.len(), trimmed.to_string()));
        buf.clear();
    } else {
        parts.push(KeyPart::Text(std::mem::take(buf)));
    }
}

/// Clé de tri « naturelle » : `CD2` passe après `CD1` mais avant `CD10`.
fn natural_key(s: &str) -> Vec<KeyPart> {
    let folded = fold(s.trim());
    let mut parts: Vec<KeyPart> = Vec::new();
    let mut buf = String::new();
    let mut in_digits = false;

    for c in folded.chars() {
        let digit = c.is_ascii_digit();
        if !buf.is_empty() && digit != in_digits {
            push_part(&mut buf, in_digits, &mut parts);
        }
        in_digits = digit;
        buf.push(c);
    }
    push_part(&mut buf, in_digits, &mut parts);
    parts
}

/// Clé texte avec « valeur manquante en dernier » : un album sans artiste ne
/// vient pas s'installer en tête de la grille.
fn text_key(s: Option<&str>) -> (bool, Vec<KeyPart>) {
    let raw = s.unwrap_or_default().trim();
    if raw.is_empty() {
        (true, Vec::new())
    } else {
        (false, natural_key(raw))
    }
}

/// Année avec « valeur manquante en dernier », comme le `NULLS LAST` du SQL.
fn year_key(y: Option<i32>) -> (bool, i32) {
    (y.is_none(), y.unwrap_or(i32::MAX))
}

/// Trie en place. L'identifiant sert toujours de dernier départage, pour que
/// deux exécutions — et les deux moteurs de base — rendent le même ordre.
pub(super) fn sort_albums(albums: &mut [Album], sort: CollectionSort) {
    match sort {
        CollectionSort::Added => {}
        CollectionSort::Artist => albums.sort_by_cached_key(|a| {
            (
                text_key(a.artist_name.as_deref()),
                year_key(a.year),
                natural_key(&a.title),
                a.id.unwrap_or(i64::MAX),
            )
        }),
        CollectionSort::Title => albums.sort_by_cached_key(|a| {
            (
                natural_key(&a.title),
                text_key(a.artist_name.as_deref()),
                a.id.unwrap_or(i64::MAX),
            )
        }),
        CollectionSort::Year => albums.sort_by_cached_key(|a| {
            (
                year_key(a.year),
                text_key(a.artist_name.as_deref()),
                natural_key(&a.title),
                a.id.unwrap_or(i64::MAX),
            )
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: i64, artist: Option<&str>, title: &str, year: Option<i32>) -> Album {
        let mut a = Album::new(title.to_string());
        a.id = Some(id);
        a.artist_name = artist.map(str::to_string);
        a.year = year;
        a
    }

    fn titles(albums: &[Album]) -> Vec<&str> {
        albums.iter().map(|a| a.title.as_str()).collect()
    }

    #[test]
    fn cd2_passe_avant_cd10() {
        assert!(natural_key("CD2") < natural_key("CD10"));
        assert!(natural_key("CD1") < natural_key("CD2"));
        assert!(natural_key("CD9") < natural_key("CD10"));
    }

    #[test]
    fn les_zeros_de_tete_ne_changent_pas_la_valeur() {
        assert_eq!(natural_key("CD007"), natural_key("CD7"));
        assert!(natural_key("CD007") < natural_key("CD10"));
    }

    #[test]
    fn un_nombre_enorme_ne_deborde_pas() {
        let enorme = "0".repeat(3) + &"9".repeat(40);
        assert!(natural_key("1") < natural_key(&enorme));
    }

    #[test]
    fn accents_et_casse_sont_replies() {
        assert_eq!(natural_key("Édith"), natural_key("edith"));
        assert_eq!(natural_key("ÀÉÎÕÜ"), natural_key("aeiou"));
        assert!(natural_key("eagles") < natural_key("Édith Piaf"));
        assert!(natural_key("Édith Piaf") < natural_key("Ella Fitzgerald"));
    }

    #[test]
    fn tri_par_artiste_puis_annee_puis_titre() {
        let mut a = vec![
            album(1, Some("Wagner"), "Der Ring CD10", Some(1970)),
            album(2, Some("ABBA"), "Arrival", Some(1976)),
            album(3, Some("Wagner"), "Der Ring CD2", Some(1970)),
            album(4, Some("Wagner"), "Parsifal", Some(1962)),
        ];
        sort_albums(&mut a, CollectionSort::Artist);
        assert_eq!(
            titles(&a),
            vec!["Arrival", "Parsifal", "Der Ring CD2", "Der Ring CD10"]
        );
    }

    #[test]
    fn artiste_manquant_passe_en_dernier() {
        let mut a = vec![
            album(1, None, "Sans artiste", None),
            album(2, Some(""), "Artiste vide", None),
            album(3, Some("Zappa"), "Hot Rats", None),
        ];
        sort_albums(&mut a, CollectionSort::Artist);
        assert_eq!(a[0].title, "Hot Rats");
        // Les deux « sans artiste » se départagent ensuite par titre.
        assert_eq!(titles(&a[1..]), vec!["Artiste vide", "Sans artiste"]);
    }

    #[test]
    fn annee_manquante_passe_en_dernier() {
        let mut a = vec![
            album(1, Some("X"), "Sans annee", None),
            album(2, Some("X"), "1999", Some(1999)),
        ];
        sort_albums(&mut a, CollectionSort::Artist);
        assert_eq!(titles(&a), vec!["1999", "Sans annee"]);
    }

    #[test]
    fn un_seul_album_reste_un_seul_album() {
        let mut a = vec![album(1, Some("Nina Simone"), "Pastel Blues", None)];
        sort_albums(&mut a, CollectionSort::Artist);
        assert_eq!(titles(&a), vec!["Pastel Blues"]);
    }

    #[test]
    fn added_ne_touche_a_rien() {
        let mut a = vec![
            album(1, Some("Zappa"), "Hot Rats", None),
            album(2, Some("ABBA"), "Arrival", None),
        ];
        sort_albums(&mut a, CollectionSort::Added);
        assert_eq!(titles(&a), vec!["Hot Rats", "Arrival"]);
    }

    #[test]
    fn parse_du_parametre_sort() {
        assert_eq!(CollectionSort::parse(None), CollectionSort::Artist);
        assert_eq!(CollectionSort::parse(Some("")), CollectionSort::Artist);
        assert_eq!(
            CollectionSort::parse(Some("artist")),
            CollectionSort::Artist
        );
        assert_eq!(
            CollectionSort::parse(Some(" title ")),
            CollectionSort::Title
        );
        assert_eq!(CollectionSort::parse(Some("album")), CollectionSort::Title);
        assert_eq!(CollectionSort::parse(Some("year")), CollectionSort::Year);
        assert_eq!(CollectionSort::parse(Some("added")), CollectionSort::Added);
        // Mot-clé inconnu : le défaut, pas l'ordre d'insertion.
        assert_eq!(CollectionSort::parse(Some("zzz")), CollectionSort::Artist);
    }
}
