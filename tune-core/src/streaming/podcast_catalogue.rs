//! Le catalogue des podcasts de la CARTE du client web (web#2450, #7194).
//!
//! La carte place les émissions par pays et par catégorie. Aucune source ne
//! donne cela d'un bloc : le seul catalogue que Tune lit déjà par pays est le
//! palmarès Apple (top 50 d'une boutique, [`super::podcasts::PodcastService::top_podcasts`]),
//! plus la sélection française écrite en dur. Ce module RÉUNIT les palmarès de
//! plusieurs boutiques et la sélection, sans doublon, en gardant pour chaque
//! émission son rang dans chaque pays où elle figure.
//!
//! Aucun fournisseur nouveau : les palmarès viennent des mêmes appels que
//! l'onglet « Populaires ». Fonction pure : aucun réseau ici, la route
//! `GET /podcasts/catalogue` fait les appels.
//!
//! # « Lisible »
//!
//! Une émission est lisible quand Tune connaît l'adresse de son flux : le
//! palmarès Apple ne la donne pas, Tune la résout (lookup Apple, puis les
//! annuaires, #6770/#7152). Une émission que personne ne sait résoudre garde
//! un `feed_url` vide ; `readable: false` le dit au client sans qu'il ait à
//! deviner ce qu'une chaîne vide signifie.

use std::collections::{BTreeMap, HashMap};

use serde::Serialize;

use super::podcasts::{Podcast, PodcastGenre};

/// Les boutiques lues par défaut : les dix-huit de l'étude du 26/10/2026
/// (prototype de la carte, 593 émissions). Codes ISO 3166-1 alpha-2, en
/// minuscules, comme les attend le chemin du palmarès Apple.
pub const PAYS_PAR_DEFAUT: &[&str] = &[
    "fr", "us", "gb", "de", "be", "ch", "ca", "it", "es", "nl", "se", "no", "dk", "at", "ie", "pt",
    "pl", "au",
];

/// Au-delà, une requête est refusée : chaque pays coûte un palmarès et ses
/// résolutions de flux.
pub const PAYS_MAX: usize = 24;

/// Une émission de la carte.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EmissionCatalogue {
    /// `apple-…` pour une émission des palmarès, `rf-…` pour la sélection.
    pub source_id: String,
    pub name: String,
    pub artist: String,
    pub cover_url: String,
    /// Vide quand aucun flux public n'est connu.
    pub feed_url: String,
    /// Tune peut ouvrir et lire cette émission (`feed_url` connu).
    pub readable: bool,
    /// La catégorie principale, telle qu'Apple la nomme (« News »…).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    /// L'identifiant de genre Apple de cette catégorie, quand il est connu :
    /// c'est lui que le client traduit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_id: Option<u32>,
    /// Le pays où l'émission est le mieux classée (en minuscules) ; celui de la
    /// sélection pour une émission qui n'est dans aucun palmarès lu.
    pub country: String,
    /// Le rang (1 = premier) dans chaque palmarès où elle figure.
    pub charts: BTreeMap<String, u32>,
    /// L'émission est dans la sélection éditoriale de Tune.
    pub curated: bool,
}

/// L'identifiant Apple d'une catégorie nommée en anglais, comme la nomment le
/// palmarès et la sélection. `None` pour un libellé inconnu : le client
/// affiche alors le libellé brut, il n'invente pas de traduction.
pub fn id_de_categorie(libelle: &str) -> Option<u32> {
    let l = libelle.trim();
    PodcastGenre::all()
        .iter()
        .find(|g| g.label().eq_ignore_ascii_case(l))
        .map(|g| *g as u32)
}

/// Les pays demandés par `?countries=fr,us,…` : codes de deux lettres, sans
/// doublon, dans l'ordre donné. Vide ou absent : [`PAYS_PAR_DEFAUT`]. `Err`
/// quand un code n'en est pas un ou qu'il y en a plus de [`PAYS_MAX`].
pub fn pays_demandes(brut: Option<&str>) -> Result<Vec<String>, String> {
    let Some(brut) = brut.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(PAYS_PAR_DEFAUT.iter().map(|s| s.to_string()).collect());
    };
    let mut pays: Vec<String> = Vec::new();
    for morceau in brut.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if morceau.len() != 2 || !morceau.chars().all(|c| c.is_ascii_alphabetic()) {
            return Err(format!("invalid country code: {morceau}"));
        }
        let cc = morceau.to_ascii_lowercase();
        if !pays.contains(&cc) {
            pays.push(cc);
        }
    }
    if pays.len() > PAYS_MAX {
        return Err(format!("at most {PAYS_MAX} countries"));
    }
    if pays.is_empty() {
        return Ok(PAYS_PAR_DEFAUT.iter().map(|s| s.to_string()).collect());
    }
    Ok(pays)
}

fn nouvelle(p: &Podcast, pays: &str, curated: bool) -> EmissionCatalogue {
    EmissionCatalogue {
        source_id: p.source_id.clone(),
        name: p.name.clone(),
        artist: p.artist.clone(),
        cover_url: p.cover_url.clone(),
        feed_url: p.feed_url.clone(),
        readable: !p.feed_url.trim().is_empty(),
        category: p.category.clone(),
        category_id: p.category.as_deref().and_then(id_de_categorie),
        country: pays.to_string(),
        charts: BTreeMap::new(),
        curated,
    }
}

/// Complète `e` de ce que `p` sait et que `e` ignore. Rien n'est écrasé.
fn completer(e: &mut EmissionCatalogue, p: &Podcast) {
    if e.feed_url.trim().is_empty() && !p.feed_url.trim().is_empty() {
        e.feed_url = p.feed_url.clone();
        e.readable = true;
    }
    if e.cover_url.is_empty() {
        e.cover_url = p.cover_url.clone();
    }
    if e.artist.is_empty() {
        e.artist = p.artist.clone();
    }
    if e.category.is_none() && p.category.is_some() {
        e.category = p.category.clone();
        e.category_id = p.category.as_deref().and_then(id_de_categorie);
    }
}

/// Réunit les palmarès (`(pays, top 50 dans l'ordre)`) et la sélection
/// éditoriale (`(pays de la sélection, émissions)`).
///
/// * Une émission présente dans plusieurs palmarès n'apparaît qu'UNE fois,
///   avec son rang dans chacun ; son `country` est celui de son meilleur rang
///   (à rang égal, le premier pays lu).
/// * Deux fiches sont la même émission quand elles ont le même `source_id`,
///   ou le même flux connu.
/// * Une émission de la sélection déjà présente dans un palmarès y est
///   marquée `curated` ; sinon elle s'ajoute, rattachée au pays de la
///   sélection.
///
/// L'ordre de sortie est stable : par ordre de première apparition.
pub fn fusionner(
    palmares: &[(String, Vec<Podcast>)],
    selection: &[(String, Vec<Podcast>)],
) -> Vec<EmissionCatalogue> {
    let mut sortie: Vec<EmissionCatalogue> = Vec::new();
    let mut par_id: HashMap<String, usize> = HashMap::new();
    let mut par_flux: HashMap<String, usize> = HashMap::new();

    let retrouver = |par_id: &HashMap<String, usize>,
                     par_flux: &HashMap<String, usize>,
                     p: &Podcast|
     -> Option<usize> {
        let flux = p.feed_url.trim();
        par_id.get(&p.source_id).copied().or_else(|| {
            (!flux.is_empty())
                .then(|| par_flux.get(flux).copied())
                .flatten()
        })
    };

    for (pays, liste) in palmares {
        let pays = pays.to_ascii_lowercase();
        for (rang0, p) in liste.iter().enumerate() {
            let rang = u32::try_from(rang0 + 1).unwrap_or(u32::MAX);
            let i = match retrouver(&par_id, &par_flux, p) {
                Some(i) => {
                    completer(&mut sortie[i], p);
                    i
                }
                None => {
                    sortie.push(nouvelle(p, &pays, false));
                    sortie.len() - 1
                }
            };
            let e = &mut sortie[i];
            let deja = e.charts.get(&pays).copied();
            if deja.is_none_or(|r| rang < r) {
                e.charts.insert(pays.clone(), rang);
            }
            let meilleur = e.charts.get(&e.country).copied();
            if meilleur.is_none_or(|r| rang < r) {
                e.country = pays.clone();
            }
            par_id.entry(p.source_id.clone()).or_insert(i);
            let flux = sortie[i].feed_url.trim().to_string();
            if !flux.is_empty() {
                par_flux.entry(flux).or_insert(i);
            }
        }
    }

    for (pays, liste) in selection {
        let pays = pays.to_ascii_lowercase();
        for p in liste {
            let i = match retrouver(&par_id, &par_flux, p) {
                Some(i) => {
                    completer(&mut sortie[i], p);
                    sortie[i].curated = true;
                    i
                }
                None => {
                    sortie.push(nouvelle(p, &pays, true));
                    sortie.len() - 1
                }
            };
            par_id.entry(p.source_id.clone()).or_insert(i);
            let flux = sortie[i].feed_url.trim().to_string();
            if !flux.is_empty() {
                par_flux.entry(flux).or_insert(i);
            }
        }
    }
    sortie
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(id: &str, nom: &str, flux: &str, cat: Option<&str>) -> Podcast {
        Podcast {
            name: nom.into(),
            artist: "Auteur".into(),
            feed_url: flux.into(),
            cover_url: format!("https://img.example/{id}.jpg"),
            description: String::new(),
            episode_count: 0,
            source_id: id.into(),
            category: cat.map(String::from),
        }
    }

    #[test]
    fn une_emission_de_deux_palmares_n_apparait_qu_une_fois_avec_ses_deux_rangs() {
        let palmares = vec![
            (
                "fr".to_string(),
                vec![
                    p("apple-1", "Un", "https://f/1", Some("News")),
                    p("apple-2", "Deux", "", Some("Comedy")),
                ],
            ),
            (
                "BE".to_string(),
                vec![
                    p("apple-2", "Deux", "", Some("Comedy")),
                    p("apple-3", "Trois", "https://f/3", None),
                ],
            ),
        ];
        let c = fusionner(&palmares, &[]);
        assert_eq!(c.len(), 3, "{c:#?}");
        let deux = c.iter().find(|e| e.source_id == "apple-2").unwrap();
        assert_eq!(deux.charts.get("fr"), Some(&2));
        assert_eq!(deux.charts.get("be"), Some(&1));
        assert_eq!(deux.country, "be", "le pays du meilleur rang");
        assert!(
            !deux.readable,
            "sans flux connu, l'émission n'est pas lisible"
        );
        assert_eq!(deux.category_id, Some(1303));
        let un = c.iter().find(|e| e.source_id == "apple-1").unwrap();
        assert!(un.readable);
        assert_eq!(un.category_id, Some(1311));
        assert_eq!(c[0].source_id, "apple-1", "ordre de première apparition");
    }

    #[test]
    fn un_flux_trouve_dans_un_autre_pays_rend_l_emission_lisible() {
        let palmares = vec![
            ("fr".to_string(), vec![p("apple-9", "Neuf", "", None)]),
            (
                "us".to_string(),
                vec![p("apple-9", "Neuf", "https://f/9", None)],
            ),
        ];
        let c = fusionner(&palmares, &[]);
        assert_eq!(c.len(), 1);
        assert!(c[0].readable);
        assert_eq!(c[0].feed_url, "https://f/9");
        assert_eq!(c[0].country, "fr", "à rang égal, le premier pays lu");
    }

    #[test]
    fn la_selection_rejoint_l_emission_du_meme_flux_ou_s_ajoute() {
        let palmares = vec![(
            "fr".to_string(),
            vec![p(
                "apple-5",
                "Les pieds sur terre",
                "https://rf/5.xml",
                None,
            )],
        )];
        let selection = vec![(
            "fr".to_string(),
            vec![
                p(
                    "rf-pieds",
                    "Les pieds sur terre",
                    "https://rf/5.xml",
                    Some("Society & Culture"),
                ),
                p(
                    "rf-masque",
                    "Le Masque et la Plume",
                    "https://rf/6.xml",
                    Some("Arts"),
                ),
            ],
        )];
        let c = fusionner(&palmares, &selection);
        assert_eq!(c.len(), 2, "{c:#?}");
        let pieds = c.iter().find(|e| e.source_id == "apple-5").unwrap();
        assert!(pieds.curated);
        assert_eq!(
            pieds.category.as_deref(),
            Some("Society & Culture"),
            "complétée"
        );
        assert_eq!(pieds.category_id, Some(1324));
        let masque = c.iter().find(|e| e.source_id == "rf-masque").unwrap();
        assert!(masque.curated && masque.readable);
        assert!(masque.charts.is_empty());
        assert_eq!(masque.country, "fr");
    }

    #[test]
    fn les_pays_demandes_sont_valides_dedoublonnes_et_bornes() {
        assert_eq!(pays_demandes(None).unwrap().len(), PAYS_PAR_DEFAUT.len());
        assert_eq!(
            pays_demandes(Some(" ")).unwrap().len(),
            PAYS_PAR_DEFAUT.len()
        );
        assert_eq!(pays_demandes(Some("FR, us,fr")).unwrap(), vec!["fr", "us"]);
        assert!(pays_demandes(Some("fr,usa")).is_err());
        assert!(pays_demandes(Some("f1")).is_err());
        let trop: Vec<String> = (0..=PAYS_MAX)
            .map(|i| {
                let a = (b'a' + (i / 26) as u8) as char;
                let b = (b'a' + (i % 26) as u8) as char;
                format!("{a}{b}")
            })
            .collect();
        assert!(pays_demandes(Some(&trop.join(","))).is_err());
    }

    #[test]
    fn une_categorie_inconnue_n_a_pas_d_identifiant() {
        assert_eq!(id_de_categorie("True Crime"), Some(1325));
        assert_eq!(id_de_categorie("society & culture"), Some(1324));
        assert_eq!(id_de_categorie("Actualités"), None);
    }
}
