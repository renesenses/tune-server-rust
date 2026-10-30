//! Rapprocher un disque de la collection de la bibliothèque et des services.
//!
//! # Dans la bibliothèque (cadrage de #7195)
//!
//! Dans cet ordre, le premier qui répond l'emporte :
//!
//! 1. **MusicBrainz** : les identifiants Discogs reliés dans MusicBrainz — le
//!    pressage (release) ou l'album (groupe de parutions, par le master
//!    Discogs) — contre `musicbrainz_release_id` /
//!    `musicbrainz_release_group_id` de l'album local. C'est aussi ainsi qu'un
//!    CD extrait se retrouve : l'extraction le balise avec le pressage que
//!    l'identifiant MusicBrainz du disque a désigné.
//! 2. **Code-barre** : celui de la fiche Discogs contre celui de l'album.
//! 3. **Artiste + titre normalisés**, comme les albums manquants (#6288) :
//!    minuscules, sans accents ni ponctuation, sans les mentions entre
//!    parenthèses (« (Remastered) »). L'artiste ET le titre doivent
//!    concorder.
//!
//! Seuls les albums LOCAUX comptent (`source = local`) : un favori de service
//! n'est pas « dans ma bibliothèque ».
//!
//! # En streaming
//!
//! Qobuz et TIDAL, par code-barre (la recherche du service avec le code),
//! sinon par artiste + titre. Un album de service n'est retenu que si son
//! TITRE normalisé est celui du disque ; par artiste + titre, son artiste
//! aussi. Mieux vaut « nulle part » qu'un mauvais album.

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};
use tune_core::metadata::albums_manquants::titre_normalise;

use crate::discogs::{Disque, code_barre_normalise};

/// Un album local, tel que la comparaison le lit.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumLocal {
    pub id: i64,
    pub titre: String,
    pub artiste: Option<String>,
    pub code_barre: Option<String>,
    pub mb_release: Option<String>,
    pub mb_groupe: Option<String>,
    /// Le badge de qualité de l'album (`hi-res`, `cd`, `dsd`, `lossy`).
    pub qualite: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<i32>,
    pub bit_depth: Option<i32>,
}

/// Les identifiants MusicBrainz reliés à un disque Discogs.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiensMusicBrainz {
    pub releases: Vec<String>,
    pub groupes: Vec<String>,
}

/// Comment un rapprochement a été fait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Par {
    MusicbrainzPressage,
    MusicbrainzAlbum,
    CodeBarre,
    ArtisteTitre,
}

/// La clé d'artiste : « The Beatles » et « Beatles » se rejoignent, ainsi que
/// « Various » (Discogs) et « Various Artists ».
pub fn artiste_normalise(nom: &str) -> String {
    let n = titre_normalise(nom);
    let n = n.strip_prefix("the ").map(str::to_string).unwrap_or(n);
    match n.as_str() {
        "various" | "various artists" | "artistes divers" | "divers" => "various".to_string(),
        _ => n,
    }
}

fn mbid(m: &str) -> String {
    m.trim().to_ascii_lowercase()
}

/// L'index de la bibliothèque, construit une fois par requête.
pub struct Index<'a> {
    albums: &'a [AlbumLocal],
    par_release: HashMap<String, usize>,
    par_groupe: HashMap<String, usize>,
    par_code: HashMap<String, usize>,
    par_titre: HashMap<String, Vec<usize>>,
}

impl<'a> Index<'a> {
    pub fn new(albums: &'a [AlbumLocal]) -> Self {
        let mut par_release = HashMap::new();
        let mut par_groupe = HashMap::new();
        let mut par_code = HashMap::new();
        let mut par_titre: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, a) in albums.iter().enumerate() {
            if let Some(r) = a.mb_release.as_deref().map(mbid).filter(|r| !r.is_empty()) {
                par_release.entry(r).or_insert(i);
            }
            if let Some(g) = a.mb_groupe.as_deref().map(mbid).filter(|g| !g.is_empty()) {
                par_groupe.entry(g).or_insert(i);
            }
            if let Some(c) = a.code_barre.as_deref().and_then(code_barre_normalise) {
                par_code.entry(c).or_insert(i);
            }
            let t = titre_normalise(&a.titre);
            if !t.is_empty() {
                par_titre.entry(t).or_default().push(i);
            }
        }
        Self {
            albums,
            par_release,
            par_groupe,
            par_code,
            par_titre,
        }
    }

    /// L'album local du disque, et comment il a été trouvé.
    pub fn trouver(
        &self,
        d: &Disque,
        codes: &[String],
        mb: Option<&LiensMusicBrainz>,
    ) -> Option<(&'a AlbumLocal, Par)> {
        if let Some(mb) = mb {
            for r in &mb.releases {
                if let Some(&i) = self.par_release.get(&mbid(r)) {
                    return Some((&self.albums[i], Par::MusicbrainzPressage));
                }
            }
            for g in &mb.groupes {
                if let Some(&i) = self.par_groupe.get(&mbid(g)) {
                    return Some((&self.albums[i], Par::MusicbrainzAlbum));
                }
            }
        }
        for c in codes.iter().filter_map(|c| code_barre_normalise(c)) {
            if let Some(&i) = self.par_code.get(&c) {
                return Some((&self.albums[i], Par::CodeBarre));
            }
        }
        let t = titre_normalise(&d.titre);
        if t.is_empty() {
            return None;
        }
        let artistes = cles_d_artiste(d);
        if artistes.is_empty() {
            return None;
        }
        for &i in self.par_titre.get(&t).into_iter().flatten() {
            let a = &self.albums[i];
            if a.artiste
                .as_deref()
                .map(artiste_normalise)
                .is_some_and(|k| artistes.contains(&k))
            {
                return Some((a, Par::ArtisteTitre));
            }
        }
        None
    }
}

/// Les clés d'artiste d'un disque : l'affichage complet, et chacun de ses
/// artistes (« Miles Davis Quintet » ou « Miles Davis » côté bibliothèque).
pub fn cles_d_artiste(d: &Disque) -> HashSet<String> {
    let mut cles: HashSet<String> = d.artistes.iter().map(|a| artiste_normalise(a)).collect();
    cles.insert(artiste_normalise(&d.artiste));
    cles.retain(|k| !k.is_empty());
    cles
}

/// Un album proposé par un service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AlbumDeService {
    pub id: String,
    pub titre: String,
    pub artiste: String,
}

/// L'album d'un service retenu pour un disque, parmi les résultats d'une
/// recherche. Par code-barre, le titre suffit (le code a déjà désigné le
/// disque ; l'artiste s'écrit souvent autrement) ; par artiste + titre, les
/// deux doivent concorder.
pub fn choisir_sur_le_service<'a>(
    d: &Disque,
    candidats: &'a [AlbumDeService],
    par_code_barre: bool,
) -> Option<&'a AlbumDeService> {
    let t = titre_normalise(&d.titre);
    if t.is_empty() {
        return None;
    }
    let artistes = cles_d_artiste(d);
    candidats.iter().find(|c| {
        titre_normalise(&c.titre) == t
            && (par_code_barre || artistes.contains(&artiste_normalise(&c.artiste)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discogs::Famille;

    fn disque(artiste: &str, titre: &str) -> Disque {
        Disque {
            instance_id: 1,
            release_id: 10,
            master_id: None,
            artistes: vec![artiste.to_string()],
            artiste: artiste.to_string(),
            titre: titre.to_string(),
            annee: None,
            labels: vec![],
            numeros_catalogue: vec![],
            formats: vec!["Vinyl".into()],
            precisions: vec![],
            famille: Famille::Vinyle,
            ajoute_le: None,
        }
    }

    fn album(id: i64, artiste: &str, titre: &str) -> AlbumLocal {
        AlbumLocal {
            id,
            titre: titre.into(),
            artiste: Some(artiste.into()),
            ..Default::default()
        }
    }

    #[test]
    fn artiste_et_titre_doivent_concorder_tous_les_deux() {
        let albums = vec![
            album(1, "Miles Davis", "Kind of Blue (Legacy Edition)"),
            album(2, "Bill Evans", "Kind of Blue"),
        ];
        let idx = Index::new(&albums);
        let (a, par) = idx
            .trouver(&disque("Miles Davis", "Kind Of Blue"), &[], None)
            .unwrap();
        assert_eq!((a.id, par), (1, Par::ArtisteTitre));
        assert!(
            idx.trouver(&disque("John Coltrane", "Kind of Blue"), &[], None)
                .is_none(),
            "même titre, autre artiste : pas le même disque"
        );
    }

    #[test]
    fn musicbrainz_passe_avant_le_code_barre_et_le_titre() {
        let mut a1 = album(1, "X", "Autre titre");
        a1.mb_release = Some("AAAAAAAA-0000-0000-0000-000000000001".into());
        let mut a2 = album(2, "X", "Titre");
        a2.code_barre = Some("0 12345 67890 5".into());
        let albums = vec![a1, a2];
        let idx = Index::new(&albums);
        let mb = LiensMusicBrainz {
            releases: vec!["aaaaaaaa-0000-0000-0000-000000000001".into()],
            groupes: vec![],
        };
        let d = disque("X", "Titre");
        let codes = vec!["012345678905".to_string()];
        assert_eq!(
            idx.trouver(&d, &codes, Some(&mb)).map(|(a, p)| (a.id, p)),
            Some((1, Par::MusicbrainzPressage))
        );
        assert_eq!(
            idx.trouver(&d, &codes, None).map(|(a, p)| (a.id, p)),
            Some((2, Par::CodeBarre))
        );
    }

    #[test]
    fn the_et_various_se_rejoignent() {
        assert_eq!(
            artiste_normalise("The Beatles"),
            artiste_normalise("Beatles")
        );
        assert_eq!(
            artiste_normalise("Various"),
            artiste_normalise("Various Artists")
        );
    }

    #[test]
    fn un_service_n_est_retenu_que_sur_un_titre_identique() {
        let d = disque("Miles Davis", "Kind of Blue");
        let c = vec![
            AlbumDeService {
                id: "a".into(),
                titre: "Kind of Blue Live".into(),
                artiste: "Miles Davis".into(),
            },
            AlbumDeService {
                id: "b".into(),
                titre: "Kind Of Blue (Remastered)".into(),
                artiste: "Miles Davis".into(),
            },
        ];
        assert_eq!(
            choisir_sur_le_service(&d, &c, false).map(|a| a.id.as_str()),
            Some("b")
        );
        let autre = vec![AlbumDeService {
            id: "c".into(),
            titre: "Kind of Blue".into(),
            artiste: "Tribute Band".into(),
        }];
        assert!(choisir_sur_le_service(&d, &autre, false).is_none());
        assert_eq!(
            choisir_sur_le_service(&d, &autre, true).map(|a| a.id.as_str()),
            Some("c"),
            "par code-barre, le titre suffit"
        );
    }
}
