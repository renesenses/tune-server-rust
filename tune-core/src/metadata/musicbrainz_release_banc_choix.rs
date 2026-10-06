//! Banc du CHOIX de pressage (#4805, étape D), sans réseau.
//!
//! Le banc #4805 ([`super::banc_4805`]) compte les albums pour lesquels un
//! pressage **plausible** revient. Il ne dit pas si le pressage retenu est le
//! bon. Celui-ci rejoue les MÊMES réponses enregistrées (05/10/2026, 61
//! requêtes) et juge le pressage que chaque chaîne **écrirait** :
//!
//! * **bon pressage** — le bon album, et autant de pistes que les fichiers ;
//! * **plausible** — le bon album, mais un autre nombre de pistes : une autre
//!   édition, dont la liste de pistes ne colle pas aux fichiers ;
//! * **faux positif** — un autre album (autre volume, autre œuvre, autre
//!   compositeur) : l'écriture remplacerait les clés par celles d'un album
//!   que l'utilisateur n'a pas ;
//! * **ambigu** — la chaîne s'abstient ; **introuvable** — rien de plausible.
//!
//! Trois chaînes :
//!
//! * **avant** — le pilote du lot @ `e6f4e8d76` : `lookup_release_candidates`
//!   (5 candidats), puis le premier (`RechercheDePressages::meilleur`) ;
//! * **MetaRust strict** — la règle de MetaRust appliquée telle quelle, pressage
//!   par pressage (`pick_confident_search_hit`, puis `pick_unique_title_hit`),
//!   sans garde : la mesure de ce qu'aurait coûté une reprise sans adaptation ;
//! * **après** — [`identifier_le_pressage`], tel que le pilote l'appelle.
//!
//! 🔴 La vérité de terrain ([`ATTENDUS`]) est écrite à la main, album par
//! album, à partir des titres et crédits : c'est un jugement, pas une mesure.
//! Les listes de pistes ne sont pas dans la fixture (elle est réduite aux
//! champs de la recherche) : la garde de complétude de « après » juge une
//! liste SYNTHÉTIQUE de `track-count` pistes sur un disque. Elle voit donc les
//! pistes en trop et la moitié manquante, pas les disques.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::banc_4805::{ALBUMS, CANDIDATS, chemin_fixture, rejouer};
use super::{
    MBReleaseMatch, RefusMusicBrainz, artiste_de_requete, normalize, normalize_compact,
    recherche_de_pressages, recherche_de_pressages_complete,
};
use crate::metadata::choix_de_pressage::{
    EntreeDIdentification, IssueDuChoix, identifier_le_pressage, score_nettement_devant,
};
use crate::metadata::reidentify::LocalTrack;

/// La vérité de terrain : pour chaque titre local, les titres MusicBrainz
/// (compacts) qui désignent le BON album et un mot de son crédit d'artiste.
/// `=` en tête : titre exact (`Kid A` n'est pas `Kid A Mnesia`). `None` :
/// aucun album juste dans MusicBrainz, tout pressage retenu est faux.
#[allow(clippy::type_complexity)]
pub(super) const ATTENDUS: &[(&str, Option<(&[&str], &str)>)] = &[
    ("Kind of Blue", Some((&["kindofblue"], "miles davis"))),
    ("A Love Supreme", Some((&["alovesupreme"], "coltrane"))),
    (
        "The Dark Side of the Moon",
        Some((&["thedarksideofthemoon"], "pink floyd")),
    ),
    ("Abbey Road", Some((&["abbeyroad"], "beatles"))),
    ("Bright Size Life", Some((&["brightsizelife"], "metheny"))),
    (
        "In the Court of the Crimson King",
        Some((&["inthecourtofthecrimsonking"], "king crimson")),
    ),
    ("Mezzanine", Some((&["mezzanine"], "massive attack"))),
    ("Moon Safari", Some((&["moonsafari"], "air"))),
    ("Kid A", Some((&["=kida"], "radiohead"))),
    ("Rumours", Some((&["rumours"], "fleetwood mac"))),
    // `Horses/Horses` est l'édition Legacy du même album.
    ("Horses", Some((&["horses"], "patti smith"))),
    (
        "Beyond the Missouri Sky (Short Stories)",
        Some((&["beyondthemissourisky"], "haden")),
    ),
    (
        "Somethin' Else (192kHz/24bit)",
        Some((&["somethinelse"], "adderley")),
    ),
    (
        "My Favorite Things (96kHz/24bit)",
        Some((&["myfavoritethings"], "coltrane")),
    ),
    (
        "Wish You Were Here (Remastered)",
        Some((&["wishyouwerehere"], "pink floyd")),
    ),
    (
        "Nevermind (Deluxe Edition)",
        Some((&["nevermind"], "nirvana")),
    ),
    (
        "Blue Train (Rudy Van Gelder Edition)",
        Some((&["bluetrain"], "coltrane")),
    ),
    (
        "Tales Of Mystery And Imagination (Original 1976 Version)",
        Some((&["talesofmysteryandimagination"], "alan parsons")),
    ),
    ("Pink Floyd - The Wall", Some((&["=thewall"], "pink floyd"))),
    (
        "Miles Davis - Bitches Brew",
        Some((&["bitchesbrew"], "miles davis")),
    ),
    (
        "Radiohead - OK Computer",
        Some((&["okcomputer"], "radiohead")),
    ),
    (
        "Daft Punk - Discovery",
        Some((&["=discovery"], "daft punk")),
    ),
    ("Portishead - Dummy", Some((&["=dummy"], "portishead"))),
    // Toute neuvième de Beethoven par Karajan : la fixture ne dit pas quel
    // cycle (1962, 1977, 1983) les fichiers portent. Dvořák ou Bruckner sont
    // faux.
    (
        "Beethoven: Symphony No. 9",
        Some((&["symphonyno9"], "beethoven")),
    ),
    ("Chopin - Nocturnes", Some((&["nocturnes"], "chopin"))),
    (
        "Bach: Goldberg Variations",
        Some((&["goldbergvariations"], "gould")),
    ),
    // Brahms (`Ein deutsches Requiem`) est faux.
    ("Mozart: Requiem", Some((&["requiem"], "mozart"))),
    (
        "Vivaldi - The Four Seasons",
        Some((&["fourseasons"], "kennedy")),
    ),
    (
        "Debussy: Préludes",
        Some((&["préludes", "preludes"], "zimerman")),
    ),
    ("Bizet: Carmen", Some((&["=carmen"], "callas"))),
    // Le premier `Buddha-Bar` ; `XXIV`, `Ocean`, `Perception` sont d'autres
    // volumes.
    ("Buddha-Bar", Some((&["=buddhabar"], "various"))),
    ("Pulp Fiction", Some((&["pulpfiction"], "various"))),
    (
        "Saturday Night Fever",
        Some((&["saturdaynightfever"], "various")),
    ),
    ("Talkie Walkie", Some((&["talkiewalkie"], "air"))),
    ("Homogenic", Some((&["homogenic"], "björk"))),
    ("Dummy", Some((&["=dummy"], "portishead"))),
    ("Symphonies (intégrale)", None),
    ("Ma compil été 2004", None),
    ("Unknown Album", None),
    ("Disque 1", None),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Verdict {
    BonPressage,
    Plausible,
    FauxPositif,
    Ambigu,
    Introuvable,
    Refus,
}

fn attendu(titre: &str) -> Option<(&'static [&'static str], &'static str)> {
    ATTENDUS
        .iter()
        .find(|(t, _)| *t == titre)
        .unwrap_or_else(|| panic!("vérité de terrain absente pour « {titre} »"))
        .1
}

pub(super) fn bon_album(c: &MBReleaseMatch, attendu: Option<(&[&str], &str)>) -> bool {
    let Some((titres, mot)) = attendu else {
        return false;
    };
    let t = normalize_compact(&c.title);
    titres.iter().any(|a| match a.strip_prefix('=') {
        Some(exact) => t == exact,
        None => t.contains(a),
    }) && normalize(&c.artist).contains(mot)
}

fn juger(retenu: &MBReleaseMatch, titre: &str, n: u32) -> Verdict {
    if !bon_album(retenu, attendu(titre)) {
        Verdict::FauxPositif
    } else if retenu.track_count == Some(n) {
        Verdict::BonPressage
    } else {
        Verdict::Plausible
    }
}

/// La règle de MetaRust telle quelle, pressage par pressage : un seul hit, ou
/// `pick_confident_search_hit`, ou `pick_unique_title_hit` (titre compact,
/// puis inclusion). Sans regroupement, sans garde.
fn metarust_strict(candidats: &[MBReleaseMatch], titre: &str) -> Option<usize> {
    if candidats.is_empty() {
        return None;
    }
    if candidats.len() == 1 {
        return Some(0);
    }
    let mut rang: Vec<usize> = (0..candidats.len()).collect();
    rang.sort_by_key(|i| std::cmp::Reverse(candidats[*i].score));
    if score_nettement_devant(candidats[rang[0]].score, candidats[rang[1]].score) {
        return Some(rang[0]);
    }
    // `split_edition_suffix` : la dernière parenthèse, sauf « feat. ».
    let base = match (titre.trim().ends_with(')'), titre.rfind('(')) {
        (true, Some(p)) if p > 0 => titre[..p].trim(),
        _ => titre.trim(),
    };
    let aiguille = normalize_compact(base);
    let exacts: Vec<usize> = (0..candidats.len())
        .filter(|i| normalize_compact(&candidats[*i].title) == aiguille)
        .collect();
    let filtres = if exacts.is_empty() {
        (0..candidats.len())
            .filter(|i| {
                let t = normalize_compact(&candidats[*i].title);
                t.contains(&aiguille) || aiguille.contains(&t)
            })
            .collect()
    } else {
        exacts
    };
    (filtres.len() == 1).then(|| filtres[0])
}

fn pistes_locales(n: u32) -> Vec<LocalTrack> {
    (1..=n as i32)
        .map(|i| LocalTrack {
            id: i as i64,
            disc: 1,
            position: i,
            title: format!("Piste {i}"),
        })
        .collect()
}

/// Une réponse `/release/{id}` synthétique : `track-count` pistes sur un
/// disque, tirée du hit de recherche enregistré.
fn detail_synthetique(hits: &BTreeMap<String, Value>, chemin: &str) -> Option<Value> {
    let id = chemin.strip_prefix("release/")?;
    let hit = hits.get(id)?;
    let n = hit.get("track-count").and_then(|t| t.as_u64()).unwrap_or(0);
    let pistes: Vec<Value> = (1..=n)
        .map(|i| json!({ "position": i, "title": format!("Piste {i}"), "recording": { "id": format!("rec-{id}-{i}") } }))
        .collect();
    Some(json!({
        "id": id,
        "title": hit.get("title").cloned().unwrap_or(json!("")),
        "artist-credit": hit.get("artist-credit").cloned().unwrap_or(json!([])),
        "release-group": hit.get("release-group").cloned().unwrap_or(json!({})),
        "status": hit.get("status").cloned().unwrap_or(Value::Null),
        "media": [{ "position": 1, "tracks": pistes }]
    }))
}

#[derive(Default, Clone)]
struct Compte(BTreeMap<Verdict, usize>);

impl Compte {
    fn ajouter(&mut self, v: Verdict) {
        *self.0.entry(v).or_default() += 1;
    }
    fn n(&self, v: Verdict) -> usize {
        self.0.get(&v).copied().unwrap_or(0)
    }
    fn identifies(&self) -> usize {
        self.n(Verdict::BonPressage) + self.n(Verdict::Plausible) + self.n(Verdict::FauxPositif)
    }
    fn ligne(&self) -> String {
        format!(
            "{} ({} bon, {} plaus., {} faux) · {} ambigus · {} introuv.",
            self.identifies(),
            self.n(Verdict::BonPressage),
            self.n(Verdict::Plausible),
            self.n(Verdict::FauxPositif),
            self.n(Verdict::Ambigu),
            self.n(Verdict::Introuvable)
        )
    }
}

/// 🔴 Le banc. Imprime, par classe et au total, ce que chaque chaîne
/// écrirait, puis garde quatre propriétés : les faux positifs baissent, aucun
/// album jugé « bon pressage » avant ne devient faux après, aucun faux positif
/// ne subsiste après, et l'abstention ne fait perdre aucun bon pressage au
/// total.
#[tokio::test(start_paused = true)]
async fn banc_choix_de_l_edition_avant_apres() {
    let brut = std::fs::read_to_string(chemin_fixture()).expect("fixture du banc #4805");
    let fixture: Value = serde_json::from_str(&brut).expect("fixture JSON");
    let reponses: BTreeMap<String, Value> =
        serde_json::from_value(fixture["reponses"].clone()).expect("reponses");
    // Tous les hits enregistrés, par identifiant : la matière des détails
    // synthétiques.
    let mut hits: BTreeMap<String, Value> = BTreeMap::new();
    for r in reponses.values() {
        for h in r
            .get("releases")
            .and_then(|x| x.as_array())
            .into_iter()
            .flatten()
        {
            if let Some(id) = h.get("id").and_then(|i| i.as_str()) {
                hits.entry(id.to_string()).or_insert_with(|| h.clone());
            }
        }
    }

    let mut par_classe: BTreeMap<&str, [Compte; 3]> = BTreeMap::new();
    let mut totaux: [Compte; 3] = Default::default();
    let mut degrades = Vec::new();

    for &(classe, titre, album, pistes, n) in ALBUMS {
        let artiste = artiste_de_requete(Some(album), pistes);

        // Avant : le premier des cinq.
        let r =
            recherche_de_pressages(titre, &artiste, Some(n), CANDIDATS, rejouer(&reponses)).await;
        let avant = match (&r.refus, r.candidats.first()) {
            (Some(_), _) => Verdict::Refus,
            (None, Some(c)) => juger(c, titre, n),
            (None, None) => Verdict::Introuvable,
        };

        // MetaRust strict, sur la liste complète.
        let r =
            recherche_de_pressages_complete(titre, &artiste, Some(n), 15, rejouer(&reponses)).await;
        let strict = if r.refus.is_some() {
            Verdict::Refus
        } else if r.candidats.is_empty() {
            Verdict::Introuvable
        } else {
            match metarust_strict(&r.candidats, titre) {
                Some(i) => juger(&r.candidats[i], titre, n),
                None => Verdict::Ambigu,
            }
        };

        // Après : la cascade du pilote (aucune balise dans ce banc).
        let locales = pistes_locales(n);
        let issue = identifier_le_pressage(
            EntreeDIdentification {
                titre,
                artiste: &artiste,
                pistes: &locales,
                releases_des_balises: &[],
                enregistrements_des_balises: &[],
                codes_barres: &[],
                compositeurs_des_balises: &[],
            },
            rejouer(&reponses),
            |chemin: String, _inc: &'static str| {
                std::future::ready(Ok::<_, RefusMusicBrainz>(detail_synthetique(
                    &hits, &chemin,
                )))
            },
        )
        .await;
        let (apres, retenu) = match &issue {
            IssueDuChoix::Retenu { pressage, .. } => (
                juger(pressage, titre, n),
                format!(
                    "{} [{} p.]",
                    pressage.title,
                    pressage.track_count.unwrap_or(0)
                ),
            ),
            IssueDuChoix::Ambigu { raison, .. } => (Verdict::Ambigu, raison.as_str().to_string()),
            IssueDuChoix::Introuvable => (Verdict::Introuvable, String::new()),
            IssueDuChoix::Refus(_) => (Verdict::Refus, String::new()),
        };

        if avant == Verdict::BonPressage && apres == Verdict::FauxPositif {
            degrades.push(titre);
        }
        println!(
            "{classe:<20} {titre:<58} avant={avant:<12?} strict={strict:<12?} après={apres:<12?} {retenu}"
        );
        let e = par_classe.entry(classe).or_default();
        for (i, v) in [avant, strict, apres].into_iter().enumerate() {
            e[i].ajouter(v);
            totaux[i].ajouter(v);
        }
    }

    println!("\n| classe | avant | MetaRust strict | après |\n|---|---|---|---|");
    for (classe, c) in &par_classe {
        println!(
            "| {classe} | {} | {} | {} |",
            c[0].ligne(),
            c[1].ligne(),
            c[2].ligne()
        );
    }
    println!(
        "| **total** | {} | {} | {} |",
        totaux[0].ligne(),
        totaux[1].ligne(),
        totaux[2].ligne()
    );

    let [avant, _strict, apres] = &totaux;
    assert!(
        degrades.is_empty(),
        "bon pressage avant, faux positif après : {degrades:?}"
    );
    assert!(
        apres.n(Verdict::FauxPositif) < avant.n(Verdict::FauxPositif),
        "les faux positifs n'ont pas baissé : avant {}, après {}",
        avant.n(Verdict::FauxPositif),
        apres.n(Verdict::FauxPositif)
    );
    assert_eq!(
        apres.n(Verdict::FauxPositif),
        0,
        "faux positifs restants après : {}",
        apres.ligne()
    );
    assert!(
        apres.n(Verdict::BonPressage) >= avant.n(Verdict::BonPressage),
        "l'abstention a mangé des bons pressages : avant {}, après {}",
        avant.ligne(),
        apres.ligne()
    );
}
