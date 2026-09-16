//! Le pont Roon, côté IMPORT — phase 2 du chantier (#3914).
//!
//! Le moissonneur (`tune-moissonneur-roon`, hors du binaire) a parcouru le
//! Core de Fabien le 16/09/2026 : 597 artistes, 1 266 albums, 15 512 pistes
//! avec leurs crédits, 589 images d'artistes. Ce module lit CE format — le
//! nôtre, écrit par le moissonneur — et décide à quoi chaque élément
//! correspond dans la bibliothèque. Il ne rend rien, n'écrit rien : la route
//! (`routes/system/import.rs`) applique.
//!
//! ## Ce que Roon apporte, et ce qu'il n'apporte pas
//!
//! La sonde brute du 16/09 a tranché : l'API Browse de Roon n'expose ni
//! biographie ni identifiant (MBID, UPC, ISRC) — les réponses brutes portent
//! exactement les cinq clés que la caisse rend. Ce qui reste, et qui est
//! réel : les **crédits par piste** (`subtitle` d'une piste dans l'album :
//! « 16 Horsepower, Hank Williams ») et les **images** — que l'export ne
//! porte encore que par leur clé (`image_key`), pas leurs octets.
//!
//! ## La règle des crédits
//!
//! Roon ne dit pas le RÔLE : la ligne mêle interprète et auteurs. On retire
//! les noms qui sont l'artiste de l'album ou de la piste — ceux-là sont des
//! interprètes, et Tune les connaît déjà — et le reste entre comme
//! `composer`, le rôle le plus proche de « a écrit ce morceau ». C'est une
//! approximation, et elle est DITE : la provenance est marquée sur la piste
//! (`track_metadata.credits_source = roon`), et rien n'est écrit sur une
//! piste qui a déjà des crédits — le vide seulement, comme partout.
//!
//! ## La garde, inchangée
//!
//! Ce qui vient de Roon reste LOCAL. `cloud::library_sync` ne pousse ni les
//! crédits ni les images — un témoin le tient (voir `routes/system/import.rs`).
use std::collections::HashMap;

use serde::Deserialize;

/// L'export du moissonneur, tel qu'il l'écrit.
#[derive(Debug, Clone, Deserialize)]
pub struct ExportRoon {
    pub source: String,
    #[serde(default)]
    pub releve: String,
    #[serde(default)]
    pub core: String,
    #[serde(default)]
    pub artistes: Vec<ArtisteRoon>,
    #[serde(default)]
    pub absent_de_l_api: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ArtisteRoon {
    pub nom: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub albums: Vec<AlbumRoon>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AlbumRoon {
    pub titre: String,
    #[serde(default)]
    pub sous_titre: Option<String>,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub pistes: Vec<PisteRoon>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PisteRoon {
    /// `"1. Hutterite Mile"` — le numéro est DANS le titre, c'est Roon.
    pub titre: String,
    /// `"16 Horsepower, David Eugene Edwards"` — à virgules, sans rôle.
    #[serde(default)]
    pub credits: Option<String>,
}

impl ExportRoon {
    /// Lit un export et refuse ce qui n'en est pas un : un autre JSON, ou un
    /// export d'une autre source, ne doit pas entrer par cette porte.
    pub fn lire(texte: &str) -> Result<Self, String> {
        let e: ExportRoon = serde_json::from_str(texte).map_err(|e| format!("json: {e}"))?;
        if e.source != "roon" {
            return Err(format!(
                "source « {} » : ce n'est pas un export du pont Roon",
                e.source
            ));
        }
        Ok(e)
    }
}

/// Replie un nom pour l'appariement : accents, casse, espaces, article.
/// « The Wailers » et « Wailers, The » et « the wailers » sont le même.
pub fn plier(nom: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    let base: String = nom
        .nfd()
        .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
        .flat_map(char::to_lowercase)
        .collect();
    let base = base.split_whitespace().collect::<Vec<_>>().join(" ");
    let base = base.trim_end_matches(", the").to_string();
    base.strip_prefix("the ")
        .map(str::to_string)
        .unwrap_or(base)
}

/// Le numéro qu'un titre Roon porte en tête, quand il en porte un.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Numero {
    /// `1-7 Titre` : le disque, sur un album à plusieurs disques.
    pub disque: Option<u32>,
    pub piste: u32,
}

/// `"12. Titre"` → piste 12 ; `"1-7 Titre"` → disque 1, piste 7 (la forme des
/// albums à plusieurs disques, mesurée sur 3 208 des 15 512 pistes de
/// l'export de Fabien) ; `"Titre"` → aucun numéro.
pub fn numero_et_titre(titre: &str) -> (Option<Numero>, String) {
    let t = titre.trim();
    let chiffres = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    if let Some((num, reste)) = t.split_once(". ")
        && chiffres(num)
        && let Ok(n) = num.parse::<u32>()
    {
        return (
            Some(Numero {
                disque: None,
                piste: n,
            }),
            reste.trim().to_string(),
        );
    }
    if let Some((tete, reste)) = t.split_once(' ')
        && let Some((d, n)) = tete.split_once('-')
        && chiffres(d)
        && chiffres(n)
        && let (Ok(d), Ok(n)) = (d.parse::<u32>(), n.parse::<u32>())
    {
        return (
            Some(Numero {
                disque: Some(d),
                piste: n,
            }),
            reste.trim().to_string(),
        );
    }
    (None, t.to_string())
}

/// Suffixes qu'une virgule ne sépare pas : « Grover Washington, Jr. » est un
/// seul nom — le cas réel du corpus de Fabien (#4015).
const SUFFIXES: &[&str] = &["jr", "jr.", "sr", "sr.", "ii", "iii", "iv"];

/// Les noms d'une ligne de crédits Roon, dans l'ordre, dédoublonnés.
pub fn noms_des_credits(credits: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for morceau in credits.split(',') {
        let m = morceau.trim();
        if m.is_empty() {
            continue;
        }
        if SUFFIXES.contains(&m.to_lowercase().as_str())
            && let Some(dernier) = out.last_mut()
        {
            *dernier = format!("{dernier}, {m}");
            continue;
        }
        if !out.iter().any(|x| plier(x) == plier(m)) {
            out.push(m.to_string());
        }
    }
    out
}

/// Les crédits à écrire pour une piste : la ligne Roon MOINS les interprètes
/// que Tune connaît déjà (artiste de l'album, artiste de la piste). Vide quand
/// la ligne ne dit rien de plus que ce qu'on sait.
pub fn credits_a_ecrire(credits: &str, interpretes: &[&str]) -> Vec<String> {
    let connus: Vec<String> = interpretes.iter().map(|i| plier(i)).collect();
    noms_des_credits(credits)
        .into_iter()
        .filter(|n| !connus.contains(&plier(n)))
        .collect()
}

/// Ce que l'appariement d'un export contre une bibliothèque a trouvé — les
/// comptes que l'aperçu et le rapport affichent.
#[derive(Debug, Default, Clone, serde::Serialize, PartialEq, Eq)]
pub struct Rapport {
    pub artistes_total: usize,
    pub artistes_apparies: usize,
    pub artistes_inconnus: Vec<String>,
    pub albums_total: usize,
    pub albums_apparies: usize,
    pub albums_inconnus: Vec<String>,
    pub pistes_total: usize,
    pub pistes_appariees: usize,
    /// Pistes dont la ligne Roon apporte au moins un nom de plus.
    pub credits_a_ecrire: usize,
    /// … dont Tune a déjà des crédits : on n'y touche pas.
    pub credits_deja_presents: usize,
    pub credits_ecrits: usize,
    /// Images : combien l'export en NOMME (clés), et combien il en PORTE.
    pub images_nommees: usize,
    pub images_portees: usize,
    /// Artistes appariés, sans image chez Tune, dont l'archive porte l'image.
    pub images_artistes_a_poser: usize,
    pub images_artistes_posees: usize,
    /// Albums appariés, sans pochette chez Tune, dont l'archive porte l'image.
    pub images_albums_a_poser: usize,
    pub images_albums_posees: usize,
}

/// Une piste de Tune, réduite à ce que l'appariement lit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PisteLocale {
    pub id: i64,
    pub titre: String,
    pub numero: Option<i32>,
    pub disque: Option<i32>,
    pub artiste: Option<String>,
    pub a_des_credits: bool,
}

/// Retrouve la piste locale d'une piste Roon : par NUMÉRO quand Roon en donne
/// un et qu'une seule piste le porte, sinon par titre replié.
pub fn apparier_piste<'a>(roon: &PisteRoon, locales: &'a [PisteLocale]) -> Option<&'a PisteLocale> {
    let (num, titre) = numero_et_titre(&roon.titre);
    if let Some(n) = num {
        // Le disque compte quand Roon le donne : la piste 1 du disque 2 n'est
        // pas la piste 1 du disque 1.
        let memes: Vec<&PisteLocale> = locales
            .iter()
            .filter(|p| p.numero == Some(n.piste as i32))
            .filter(|p| {
                n.disque
                    .is_none_or(|d| p.disque.is_none_or(|pd| pd == d as i32))
            })
            .collect();
        if let [seule] = memes.as_slice() {
            return Some(seule);
        }
    }
    let voulu = plier(&titre);
    locales.iter().find(|p| plier(&p.titre) == voulu)
}

/// Index des titres d'album d'un artiste local, repliés.
pub fn index_par_titre<T>(items: &[T], titre: impl Fn(&T) -> &str) -> HashMap<String, usize> {
    let mut m = HashMap::new();
    for (i, it) in items.iter().enumerate() {
        m.entry(plier(titre(it))).or_insert(i);
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn le_format_du_moissonneur_se_lit_et_les_autres_sont_refuses() {
        let e = ExportRoon::lire(r#"{"source":"roon","artistes":[{"nom":"16 Horsepower","image":"ce1d","albums":[{"titre":"Folklore","pistes":[{"titre":"1. Hutterite Mile","credits":"16 Horsepower, David Eugene Edwards"}]}]}],"absent_de_l_api":["biographies"]}"#).unwrap();
        assert_eq!(e.artistes.len(), 1);
        assert_eq!(e.artistes[0].albums[0].pistes[0].titre, "1. Hutterite Mile");
        assert!(ExportRoon::lire(r#"{"source":"plex","artistes":[]}"#).is_err());
        assert!(ExportRoon::lire("Title,Artist\nA,B").is_err());
    }

    #[test]
    fn le_numero_sort_du_titre() {
        let n = |piste| {
            Some(Numero {
                disque: None,
                piste,
            })
        };
        assert_eq!(
            numero_et_titre("1. Hutterite Mile"),
            (n(1), "Hutterite Mile".into())
        );
        assert_eq!(
            numero_et_titre("12. Mr. Blue Sky"),
            (n(12), "Mr. Blue Sky".into())
        );
        assert_eq!(
            numero_et_titre("Mr. Blue Sky"),
            (None, "Mr. Blue Sky".into())
        );
        assert_eq!(
            numero_et_titre("2001. A Space Odyssey"),
            (n(2001), "A Space Odyssey".into())
        );
        // Plusieurs disques : `1-7 Titre`, sans point — la forme réelle.
        let dn = |disque, piste| {
            Some(Numero {
                disque: Some(disque),
                piste,
            })
        };
        assert_eq!(
            numero_et_titre("1-7 Hutterite Mile (2002, \" Folklore \" )"),
            (dn(1, 7), "Hutterite Mile (2002, \" Folklore \" )".into())
        );
        assert_eq!(
            numero_et_titre("2-10 Low Estate"),
            (dn(2, 10), "Low Estate".into())
        );
        assert_eq!(numero_et_titre("1-A Titre"), (None, "1-A Titre".into()));
    }

    #[test]
    fn les_credits_se_decoupent_sans_couper_un_jr() {
        assert_eq!(
            noms_des_credits("16 Horsepower, Hank Williams"),
            vec!["16 Horsepower", "Hank Williams"]
        );
        assert_eq!(
            noms_des_credits("Grover Washington, Jr., Bill Withers"),
            vec!["Grover Washington, Jr.", "Bill Withers"]
        );
        assert_eq!(noms_des_credits("A, a, A "), vec!["A"]);
        assert_eq!(noms_des_credits(""), Vec::<String>::new());
    }

    #[test]
    fn les_interpretes_connus_sont_retires_et_le_reste_est_a_ecrire() {
        assert_eq!(
            credits_a_ecrire("16 Horsepower, Hank Williams", &["16 Horsepower"]),
            vec!["Hank Williams"]
        );
        assert_eq!(
            credits_a_ecrire("The Wailers, Bob Marley", &["Wailers, The", "bob marley"]),
            Vec::<String>::new()
        );
        assert_eq!(
            credits_a_ecrire("Édith Piaf", &["edith piaf"]),
            Vec::<String>::new()
        );
    }

    #[test]
    fn une_piste_s_apparie_par_numero_puis_par_titre() {
        let l = |id, titre: &str, numero, disque| PisteLocale {
            id,
            titre: titre.into(),
            numero: Some(numero),
            disque: Some(disque),
            artiste: None,
            a_des_credits: false,
        };
        let locales = vec![
            l(10, "Hutterite Mile", 1, 1),
            l(11, "Outlaw Song", 2, 1),
            l(12, "Outlaw Song (live)", 2, 1),
            l(20, "Black Soul Choir", 1, 2),
        ];
        let r = |t: &str| PisteRoon {
            titre: t.into(),
            credits: None,
        };
        // Deux pistes 1 (disque 1 et disque 2) : sans disque, le titre
        // tranche ; avec le disque, le numéro suffit.
        assert_eq!(
            apparier_piste(&r("1. Hutterite Mile"), &locales).map(|p| p.id),
            Some(10)
        );
        assert_eq!(
            apparier_piste(&r("2-1 Black Soul Choir"), &locales).map(|p| p.id),
            Some(20)
        );
        assert_eq!(
            apparier_piste(&r("1-1 Hutterite Mile"), &locales).map(|p| p.id),
            Some(10)
        );
        // Deux pistes portent le 2 : le numéro ne tranche pas, le titre oui.
        assert_eq!(
            apparier_piste(&r("2. Outlaw Song"), &locales).map(|p| p.id),
            Some(11)
        );
        assert_eq!(
            apparier_piste(&r("Outlaw Song"), &locales).map(|p| p.id),
            Some(11)
        );
        assert_eq!(apparier_piste(&r("9. Inconnue"), &locales), None);
    }
    /// L'export RÉEL de Fabien (16/09/2026), quand on l'a sous la main :
    /// `TUNE_EXPORT_ROON=/chemin/export.json cargo test … -- --ignored`.
    /// Ce qu'il doit donner : 597 artistes, 1 266 albums, 15 512 pistes.
    #[test]
    #[ignore]
    fn l_export_reel_de_fabien_se_lit() {
        let chemin = std::env::var("TUNE_EXPORT_ROON").expect("TUNE_EXPORT_ROON");
        let texte = std::fs::read_to_string(chemin).unwrap();
        let e = ExportRoon::lire(&texte).unwrap();
        let albums: usize = e.artistes.iter().map(|a| a.albums.len()).sum();
        let pistes: usize = e
            .artistes
            .iter()
            .flat_map(|a| &a.albums)
            .map(|a| a.pistes.len())
            .sum();
        let numerotees = e
            .artistes
            .iter()
            .flat_map(|a| &a.albums)
            .flat_map(|a| &a.pistes)
            .filter(|p| numero_et_titre(&p.titre).0.is_some())
            .count();
        eprintln!(
            "artistes={} albums={albums} pistes={pistes} numerotees={numerotees}",
            e.artistes.len()
        );
        assert_eq!(e.artistes.len(), 597);
        assert_eq!(albums, 1266);
        assert_eq!(pistes, 15512);
        assert!(
            numerotees * 100 / pistes >= 95,
            "{numerotees}/{pistes} pistes numérotées"
        );
    }
}
