//! Les identifiants que voient les clients Subsonic (#7855).
//!
//! Un client les traite comme des chaînes opaques ; Tune les veut stables
//! (un favori ou une file mémorisés côté client restent valables d'une
//! session à l'autre) et sans ambiguïté de type : un même nombre désigne un
//! artiste, un album ou une piste selon son préfixe. Les étapes 2 et 3
//! (navigation, lecture, favoris) les décodent ici, jamais ailleurs.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Genre {
    Artiste,
    Album,
    Piste,
}

impl Genre {
    fn prefixe(self) -> &'static str {
        match self {
            Genre::Artiste => "ar-",
            Genre::Album => "al-",
            Genre::Piste => "tr-",
        }
    }
}

/// L'identifiant opaque d'un objet de la bibliothèque.
pub fn encoder(genre: Genre, id: i64) -> String {
    format!("{}{id}", genre.prefixe())
}

/// Décode un identifiant reçu d'un client : `None` s'il n'a pas le préfixe
/// attendu ou si son numéro n'est pas un identifiant positif.
pub fn decoder(genre: Genre, texte: &str) -> Option<i64> {
    let reste = texte.strip_prefix(genre.prefixe())?;
    if reste.is_empty() || !reste.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    reste.parse::<i64>().ok().filter(|id| *id > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_identifiant_se_relit_tel_quel_7855() {
        for genre in [Genre::Artiste, Genre::Album, Genre::Piste] {
            for id in [1, 42, i64::MAX] {
                assert_eq!(decoder(genre, &encoder(genre, id)), Some(id));
            }
        }
        assert_eq!(encoder(Genre::Artiste, 7), "ar-7");
        assert_eq!(encoder(Genre::Album, 7), "al-7");
        assert_eq!(encoder(Genre::Piste, 7), "tr-7");
    }

    #[test]
    fn un_identifiant_d_un_autre_genre_ou_mal_forme_est_refuse_7855() {
        assert_eq!(decoder(Genre::Album, "ar-7"), None);
        assert_eq!(decoder(Genre::Piste, "7"), None);
        assert_eq!(decoder(Genre::Piste, "tr-"), None);
        assert_eq!(decoder(Genre::Piste, "tr-+7"), None);
        assert_eq!(decoder(Genre::Piste, "tr--7"), None);
        assert_eq!(decoder(Genre::Piste, "tr-0"), None);
        assert_eq!(decoder(Genre::Piste, "tr-7x"), None);
        assert_eq!(decoder(Genre::Piste, "tr-99999999999999999999"), None);
    }
}
