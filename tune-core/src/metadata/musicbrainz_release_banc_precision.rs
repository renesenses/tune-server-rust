//! Banc de la PRÉCISION (#4805, étape D), sans réseau : le bon pressage, et
//! les bons enregistrements.
//!
//! Le banc du choix ([`super::banc_choix`]) juge l'ALBUM retenu sur une liste
//! de pistes synthétique (« Piste 1 », « Piste 2 »…). Celui-ci rejoue les
//! mêmes 61 recherches enregistrées (banc de #5823), mais lit les VRAIES
//! listes de pistes des pressages, enregistrées une fois
//! (`banc_precision_4805d.json`), pour juger aussi les `musicbrainz_recording_id`
//! que chaque chaîne écrirait.
//!
//! # La bibliothèque
//!
//! Pour chaque album, le **pressage de référence** est le premier candidat de
//! la recherche qui est le bon album (vérité de terrain de
//! [`super::banc_choix::ATTENDUS`], écrite à la main) ET qui a autant de
//! pistes que les fichiers. Les fichiers locaux sont calqués sur ses pistes :
//! même disque, même rang, même titre. L'enregistrement **juste** d'un fichier
//! est celui de sa piste dans ce pressage. Un album sans pressage de
//! référence a des fichiers synthétiques, et tout enregistrement écrit y est
//! faux.
//!
//! Les albums classiques portent une balise `COMPOSER` (`tracks.composer`).
//! Deux jeux :
//!
//! * **les 40** du banc de #5823, tels quels ;
//! * **classique sans préfixe** : sept albums classiques dont le titre ne nomme
//!   pas le compositeur (`Symphony No. 9` / Karajan), dont deux pièges : la
//!   neuvième de **Dvořák** et le `Requiem` de **Brahms**, que la recherche
//!   classe derrière Beethoven et Mozart. Leurs requêtes sont celles du second
//!   essai du banc de #5823 : aucune recherche nouvelle.
//!
//! # Les chaînes
//!
//! * **lot** — `e6f4e8d76` : le premier des cinq, enregistrements au rang ;
//! * **lot + garde** — la même, avec la garde des titres
//!   ([`enregistrements_si_les_titres_concordent`]) : l'effet de la garde seule ;
//! * **#5866** — [`identifier_le_pressage`] sans balise `COMPOSER`,
//!   enregistrements au rang ([`map_recording_ids`]) ;
//! * **#5866 + D** — avec la balise `COMPOSER` et la garde des titres.
//!
//! 🔴 « juste » se juge sur l'IDENTIFIANT. Un autre pressage du même album
//! porte souvent les mêmes enregistrements ; un remaster peut en porter
//! d'autres : il est compté à part (« autre, même titre »), ni juste ni faux.

use std::cell::RefCell;
use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::banc_4805::{ALBUMS, CANDIDATS, chemin_fixture, reduire, rejouer};
use super::banc_choix::{ATTENDUS, bon_album};
use super::{
    MBReleaseMatch, RefusMusicBrainz, artiste_de_requete, compositeur_en_prefixe,
    parse_release_detail, recherche_de_pressages, recherche_de_pressages_complete,
};
use crate::metadata::choix_de_pressage::{
    EntreeDIdentification, INC_DETAIL, IssueDuChoix, identifier_le_pressage,
};
use crate::metadata::reidentify::{
    LocalTrack, enregistrements_si_les_titres_concordent, map_recording_ids, titres_concordent,
};

/// La vérité de terrain d'un album : titres compacts du bon album (`=` :
/// exact) et un mot de son crédit.
type Attendu = Option<(&'static [&'static str], &'static str)>;

/// Un album du banc.
#[derive(Clone, Copy)]
struct Cas {
    jeu: &'static str,
    classe: &'static str,
    titre: &'static str,
    album: &'static str,
    pistes_artiste: Option<&'static str>,
    n: u32,
    /// La balise `COMPOSER` des fichiers.
    compositeur: Option<&'static str>,
    attendu: Attendu,
}

/// Le nom complet qu'une balise `COMPOSER` porterait, par compositeur en tête
/// de titre.
fn nom_du_compositeur(cle: &str) -> &'static str {
    match cle {
        "beethoven" => "Ludwig van Beethoven",
        "chopin" => "Frédéric Chopin",
        "bach" => "Johann Sebastian Bach",
        "mozart" => "Wolfgang Amadeus Mozart",
        "vivaldi" => "Antonio Vivaldi",
        "debussy" => "Claude Debussy",
        "bizet" => "Georges Bizet",
        autre => panic!("compositeur sans nom complet : {autre}"),
    }
}

/// Le second jeu : classique, le titre ne nomme pas le compositeur.
#[allow(clippy::type_complexity)]
const CLASSIQUE_SANS_PREFIXE: &[(&str, &str, &str, u32, (&[&str], &str))] = &[
    (
        "Symphony No. 9",
        "Herbert von Karajan",
        "Ludwig van Beethoven",
        5,
        (&["symphonyno9"], "beethoven"),
    ),
    (
        "Nocturnes",
        "Maria João Pires",
        "Frédéric Chopin",
        21,
        (&["nocturnes"], "chopin"),
    ),
    (
        "Requiem",
        "Herbert von Karajan",
        "Wolfgang Amadeus Mozart",
        15,
        (&["=requiem"], "mozart"),
    ),
    (
        "The Four Seasons",
        "Nigel Kennedy",
        "Antonio Vivaldi",
        12,
        (&["fourseasons"], "vivaldi"),
    ),
    (
        "Préludes",
        "Krystian Zimerman",
        "Claude Debussy",
        24,
        (&["préludes", "preludes"], "debussy"),
    ),
    // Pièges : la recherche met Beethoven et Mozart devant.
    (
        "Symphony No. 9",
        "Herbert von Karajan",
        "Antonín Dvořák",
        4,
        (&["newworld"], "dvořák"),
    ),
    (
        "Requiem",
        "Herbert von Karajan",
        "Johannes Brahms",
        7,
        (&["deutschesrequiem", "germanrequiem"], "brahms"),
    ),
    // L'exemple du plan : sous `Berliner Philharmoniker`, la neuvième de
    // Dvořák (100, 4 pistes) passe « nettement devant » celles de Beethoven
    // (89). Une neuvième de Beethoven en quatre pistes, une par mouvement,
    // colle alors aux fichiers : #5866 l'écrit. La recherche n'a aucune
    // Beethoven en quatre pistes : pas de pressage de référence.
    (
        "Symphony No. 9",
        "Berliner Philharmoniker",
        "Ludwig van Beethoven",
        4,
        (&["symphonyno9"], "beethoven"),
    ),
    // Témoin : la Dvořák, elle, reste trouvée.
    (
        "Symphony No. 9",
        "Berliner Philharmoniker",
        "Antonín Dvořák",
        4,
        (&["symphonyno9"], "dvořák"),
    ),
    // Beethoven en cinq pistes (Abbado) : la balise écarte Dvořák, mais
    // quatre Beethoven à cinq pistes restent en lice.
    (
        "Symphony No. 9",
        "Berliner Philharmoniker",
        "Ludwig van Beethoven",
        5,
        (&["symphonyno9"], "beethoven"),
    ),
];

fn les_cas() -> Vec<Cas> {
    let mut cas: Vec<Cas> = ALBUMS
        .iter()
        .map(|&(classe, titre, album, pistes_artiste, n)| Cas {
            jeu: "les 40",
            classe,
            titre,
            album,
            pistes_artiste,
            n,
            compositeur: compositeur_en_prefixe(titre).map(|k| nom_du_compositeur(&k)),
            attendu: ATTENDUS
                .iter()
                .find(|(t, _)| *t == titre)
                .unwrap_or_else(|| panic!("vérité de terrain absente pour « {titre} »"))
                .1,
        })
        .collect();
    cas.extend(
        CLASSIQUE_SANS_PREFIXE
            .iter()
            .map(|&(titre, album, compositeur, n, attendu)| Cas {
                jeu: "classique sans préfixe",
                classe: "sans_prefixe",
                titre,
                album,
                pistes_artiste: None,
                n,
                compositeur: Some(compositeur),
                attendu: Some(attendu),
            }),
    );
    cas
}

/// Les recherches que le banc de #5823 n'a pas : l'exemple même du plan
/// (`Symphony No. 9` / `Berliner Philharmoniker`, Dvořák classé devant
/// Beethoven). Enregistrées dans la fixture de ce banc (`recherches`).
const RECHERCHES_SUPPLEMENTAIRES: &[&str] =
    &[r#"release:"Symphony No. 9" AND artist:"Berliner Philharmoniker""#];

pub(super) fn chemin_fixture_precision() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/musicbrainz/banc_precision_4805d.json")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum VerdictAlbum {
    BonPressage,
    Plausible,
    FauxPositif,
    Ambigu,
    Introuvable,
}

fn juger_l_album(retenu: &MBReleaseMatch, cas: &Cas, pistes_du_pressage: usize) -> VerdictAlbum {
    if !bon_album(retenu, cas.attendu) {
        VerdictAlbum::FauxPositif
    } else if pistes_du_pressage == cas.n as usize {
        VerdictAlbum::BonPressage
    } else {
        VerdictAlbum::Plausible
    }
}

/// Ce qu'une chaîne a écrit pour un album.
#[derive(Debug, Default, Clone)]
struct Bilan {
    albums: BTreeMap<VerdictAlbum, usize>,
    /// Enregistrements écrits, égaux à la référence.
    justes: usize,
    /// Écrits, autres que la référence, mais de même titre (autre pressage).
    autres_meme_titre: usize,
    /// Écrits, et faux : une autre œuvre.
    faux: usize,
    /// Fichiers qui ont une référence (le maximum de `justes`).
    possibles: usize,
}

impl Bilan {
    fn n(&self, v: VerdictAlbum) -> usize {
        self.albums.get(&v).copied().unwrap_or(0)
    }
    fn ajouter(&mut self, autre: &Bilan) {
        for (v, n) in &autre.albums {
            *self.albums.entry(*v).or_default() += n;
        }
        self.justes += autre.justes;
        self.autres_meme_titre += autre.autres_meme_titre;
        self.faux += autre.faux;
        self.possibles += autre.possibles;
    }
    fn ligne_albums(&self) -> String {
        format!(
            "{} bon · {} plaus. · **{} faux** · {} amb. · {} introuv.",
            self.n(VerdictAlbum::BonPressage),
            self.n(VerdictAlbum::Plausible),
            self.n(VerdictAlbum::FauxPositif),
            self.n(VerdictAlbum::Ambigu),
            self.n(VerdictAlbum::Introuvable)
        )
    }
    fn ligne_pistes(&self) -> String {
        format!(
            "{} justes · {} autres (même titre) · **{} faux** / {}",
            self.justes, self.autres_meme_titre, self.faux, self.possibles
        )
    }
}

/// Les pistes d'un pressage enregistré, en fichiers locaux.
fn fichiers_du_pressage(detail: &Value) -> Vec<(LocalTrack, Option<String>)> {
    let d = parse_release_detail(detail).expect("détail de référence lisible");
    d.tracks
        .iter()
        .enumerate()
        .map(|(i, t)| {
            (
                LocalTrack {
                    id: i as i64 + 1,
                    disc: t.disc as i32,
                    position: t.position as i32,
                    title: t.title.clone(),
                },
                t.recording_id.clone(),
            )
        })
        .collect()
}

/// Juge les enregistrements écrits contre la référence.
fn juger_les_pistes(
    ecrits: &[(i64, String)],
    reference: &[(LocalTrack, Option<String>)],
    detail_retenu: &Value,
    bilan: &mut Bilan,
) {
    let retenu = parse_release_detail(detail_retenu).expect("détail retenu lisible");
    for (id, rid) in ecrits {
        let juste = reference
            .iter()
            .find(|(l, _)| l.id == *id)
            .and_then(|(_, r)| r.as_deref());
        if juste == Some(rid.as_str()) {
            bilan.justes += 1;
            continue;
        }
        let titre_ecrit = retenu
            .tracks
            .iter()
            .find(|t| t.recording_id.as_deref() == Some(rid.as_str()))
            .map(|t| t.title.as_str())
            .unwrap_or_default();
        let titre_local = reference
            .iter()
            .find(|(l, _)| l.id == *id)
            .map(|(l, _)| l.title.as_str())
            .unwrap_or_default();
        if juste.is_some() && titres_concordent(titre_local, titre_ecrit) {
            bilan.autres_meme_titre += 1;
        } else {
            bilan.faux += 1;
        }
    }
}

/// Une chaîne, sur un album : ce qu'elle retient, et ce qu'elle écrirait.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Chaine {
    Lot,
    LotGarde,
    Pr5866,
    Pr5866D,
}

const CHAINES: [Chaine; 4] = [
    Chaine::Lot,
    Chaine::LotGarde,
    Chaine::Pr5866,
    Chaine::Pr5866D,
];

impl Chaine {
    fn nom(self) -> &'static str {
        match self {
            Chaine::Lot => "lot (e6f4e8d76)",
            Chaine::LotGarde => "lot + garde",
            Chaine::Pr5866 => "#5866",
            Chaine::Pr5866D => "#5866 + D",
        }
    }
}

/// Évalue un album pour les quatre chaînes. `lire` sert les détails
/// (`release/{id}`) : la fixture au banc, MusicBrainz à l'enregistrement.
async fn evaluer<L, FutL>(
    cas: &Cas,
    recherches: &BTreeMap<String, Value>,
    mut lire: L,
) -> ([Bilan; 4], String)
where
    L: FnMut(String, &'static str) -> FutL,
    FutL: std::future::Future<Output = Result<Option<Value>, RefusMusicBrainz>>,
{
    let artiste = artiste_de_requete(Some(cas.album), cas.pistes_artiste);

    // Le pressage de référence : le premier candidat juste, au bon nombre de
    // pistes.
    let tous =
        recherche_de_pressages_complete(cas.titre, &artiste, Some(cas.n), 15, rejouer(recherches))
            .await;
    let reference_id = tous
        .candidats
        .iter()
        .find(|c| bon_album(c, cas.attendu) && c.track_count == Some(cas.n))
        .map(|c| c.release_id.clone());
    let reference: Vec<(LocalTrack, Option<String>)> = match &reference_id {
        Some(id) => {
            let d = lire(format!("release/{id}"), INC_DETAIL)
                .await
                .expect("référence lue")
                .expect("référence connue de MusicBrainz");
            fichiers_du_pressage(&d)
        }
        None => (1..=cas.n as i32)
            .map(|i| {
                (
                    LocalTrack {
                        id: i as i64,
                        disc: 1,
                        position: i,
                        title: format!("Piste {i}"),
                    },
                    None,
                )
            })
            .collect(),
    };
    let locales: Vec<LocalTrack> = reference.iter().map(|(l, _)| l.clone()).collect();
    let compositeurs: Vec<String> = cas
        .compositeur
        .map(|c| vec![c.to_string(); locales.len()])
        .unwrap_or_default();

    let mut bilans: [Bilan; 4] = Default::default();
    let mut trace = Vec::new();
    for (k, chaine) in CHAINES.into_iter().enumerate() {
        let bilan = &mut bilans[k];
        bilan.possibles = reference.iter().filter(|(_, r)| r.is_some()).count();
        let retenu: Option<(MBReleaseMatch, Value)> = match chaine {
            Chaine::Lot | Chaine::LotGarde => {
                let r = recherche_de_pressages(
                    cas.titre,
                    &artiste,
                    Some(cas.n),
                    CANDIDATS,
                    rejouer(recherches),
                )
                .await;
                match r.candidats.first() {
                    Some(c) => {
                        let d = lire(format!("release/{}", c.release_id), INC_DETAIL)
                            .await
                            .expect("détail lu")
                            .expect("détail connu");
                        Some((c.clone(), d))
                    }
                    None => None,
                }
            }
            Chaine::Pr5866 | Chaine::Pr5866D => {
                let avec_balise = chaine == Chaine::Pr5866D;
                // Le détail retenu passe par `lire` : on le garde au passage.
                let vu: RefCell<BTreeMap<String, Value>> = RefCell::new(BTreeMap::new());
                let issue = identifier_le_pressage(
                    EntreeDIdentification {
                        titre: cas.titre,
                        artiste: &artiste,
                        pistes: &locales,
                        releases_des_balises: &[],
                        enregistrements_des_balises: &[],
                        codes_barres: &[],
                        compositeurs_des_balises: if avec_balise { &compositeurs } else { &[] },
                    },
                    rejouer(recherches),
                    |chemin: String, inc: &'static str| {
                        let f = lire(chemin.clone(), inc);
                        let vu = &vu;
                        async move {
                            let r = f.await;
                            if let Ok(Some(v)) = &r {
                                vu.borrow_mut().insert(chemin, v.clone());
                            }
                            r
                        }
                    },
                )
                .await;
                match issue {
                    IssueDuChoix::Retenu { pressage, .. } => {
                        let d = vu
                            .borrow()
                            .get(&format!("release/{}", pressage.release_id))
                            .cloned()
                            .expect("détail du pressage retenu");
                        Some((pressage, d))
                    }
                    IssueDuChoix::Ambigu { .. } => {
                        *bilan.albums.entry(VerdictAlbum::Ambigu).or_default() += 1;
                        trace.push(format!("{}=ambigu", chaine.nom()));
                        continue;
                    }
                    IssueDuChoix::Introuvable | IssueDuChoix::Refus(_) => None,
                }
            }
        };
        let Some((pressage, detail)) = retenu else {
            *bilan.albums.entry(VerdictAlbum::Introuvable).or_default() += 1;
            trace.push(format!("{}=introuvable", chaine.nom()));
            continue;
        };
        let pistes_mb = parse_release_detail(&detail)
            .map(|d| d.tracks)
            .unwrap_or_default();
        let verdict = juger_l_album(&pressage, cas, pistes_mb.len());
        *bilan.albums.entry(verdict).or_default() += 1;
        let (ecrits, concordance) = match chaine {
            Chaine::Lot | Chaine::Pr5866 => {
                (map_recording_ids(&locales, &pistes_mb), String::new())
            }
            Chaine::LotGarde | Chaine::Pr5866D => {
                let (e, c) = enregistrements_si_les_titres_concordent(&locales, &pistes_mb);
                (e, format!(" (titres {}/{})", c.concordants, c.fichiers))
            }
        };
        let avant = (bilan.justes, bilan.autres_meme_titre, bilan.faux);
        juger_les_pistes(&ecrits, &reference, &detail, bilan);
        trace.push(format!(
            "{}={verdict:?}[{} p.] {}/{}/{}{concordance}",
            chaine.nom(),
            pistes_mb.len(),
            bilan.justes - avant.0,
            bilan.autres_meme_titre - avant.1,
            bilan.faux - avant.2
        ));
    }
    (bilans, trace.join(" · "))
}

fn charger(chemin: &std::path::Path, cle: &str) -> BTreeMap<String, Value> {
    let brut = std::fs::read_to_string(chemin)
        .unwrap_or_else(|e| panic!("fixture {} : {e}", chemin.display()));
    let v: Value = serde_json::from_str(&brut).expect("fixture JSON");
    if v[cle].is_null() {
        return BTreeMap::new();
    }
    serde_json::from_value(v[cle].clone()).expect("table de la fixture")
}

/// 🔴 Le banc. Imprime, par jeu et par chaîne, les albums et les
/// enregistrements, puis garde les propriétés de l'étape D :
///
/// 1. avec la garde des titres, **aucun enregistrement faux** n'est écrit, et
///    la garde ne coûte aucun enregistrement juste sur un bon pressage ;
/// 2. la balise `COMPOSER` fait baisser les faux positifs du jeu classique,
///    jusqu'à zéro, sans perdre de bon pressage nulle part ;
/// 3. sur les 40, « #5866 + D » ne perd aucun bon pressage de « #5866 ».
#[tokio::test(start_paused = true)]
async fn banc_precision_4805d_avant_apres() {
    let mut recherches = charger(&chemin_fixture(), "reponses");
    recherches.extend(charger(&chemin_fixture_precision(), "recherches"));
    let releases = charger(&chemin_fixture_precision(), "releases");
    let lire = |chemin: String, _inc: &'static str| {
        let id = chemin.trim_start_matches("release/");
        let v = releases.get(id).cloned().unwrap_or_else(|| {
            panic!("release non enregistrée : {id} — réenregistrer (en-tête du module)")
        });
        std::future::ready(Ok::<_, RefusMusicBrainz>(Some(v)))
    };

    let mut par_jeu: BTreeMap<&str, [Bilan; 4]> = BTreeMap::new();
    for cas in les_cas() {
        let (bilans, trace) = evaluer(&cas, &recherches, lire).await;
        println!(
            "{:<22} {:<20} {:<45} {:<24} {:<22} {:>2} p. {}",
            cas.jeu,
            cas.classe,
            cas.titre,
            cas.album,
            cas.compositeur.unwrap_or("-"),
            cas.n,
            trace
        );
        let e = par_jeu.entry(cas.jeu).or_default();
        for k in 0..4 {
            e[k].ajouter(&bilans[k]);
        }
    }

    for (jeu, b) in &par_jeu {
        println!("\n**{jeu}**\n\n| chaîne | albums | enregistrements écrits |\n|---|---|---|");
        for (k, chaine) in CHAINES.into_iter().enumerate() {
            println!(
                "| {} | {} | {} |",
                chaine.nom(),
                b[k].ligne_albums(),
                b[k].ligne_pistes()
            );
        }
    }

    let classique = &par_jeu["classique sans préfixe"];
    let quarante = &par_jeu["les 40"];
    let (lot, lot_garde, pr, pr_d) = (0, 1, 2, 3);
    for b in par_jeu.values() {
        assert_eq!(
            b[pr_d].faux,
            0,
            "#5866 + D écrit encore des enregistrements faux : {}",
            b[pr_d].ligne_pistes()
        );
        assert_eq!(
            b[lot_garde].faux,
            0,
            "la garde des titres laisse passer des enregistrements faux : {}",
            b[lot_garde].ligne_pistes()
        );
    }
    assert!(
        quarante[lot_garde].faux < quarante[lot].faux,
        "la garde n'a rien retenu sur les 40 : lot {}, lot + garde {}",
        quarante[lot].ligne_pistes(),
        quarante[lot_garde].ligne_pistes()
    );
    assert!(
        classique[pr_d].n(VerdictAlbum::FauxPositif) < classique[pr].n(VerdictAlbum::FauxPositif),
        "la balise COMPOSER n'a pas fait baisser les faux positifs du classique : \
         #5866 {}, #5866 + D {}",
        classique[pr].ligne_albums(),
        classique[pr_d].ligne_albums()
    );
    assert_eq!(
        classique[pr_d].n(VerdictAlbum::FauxPositif),
        0,
        "faux positifs restants : {}",
        classique[pr_d].ligne_albums()
    );
    for b in par_jeu.values() {
        assert!(
            b[pr_d].n(VerdictAlbum::BonPressage) >= b[pr].n(VerdictAlbum::BonPressage),
            "D a perdu des bons pressages : #5866 {}, #5866 + D {}",
            b[pr].ligne_albums(),
            b[pr_d].ligne_albums()
        );
        assert!(
            b[pr_d].justes >= b[pr].justes,
            "la garde a retenu des enregistrements justes : #5866 {}, #5866 + D {}",
            b[pr].ligne_pistes(),
            b[pr_d].ligne_pistes()
        );
    }
}

/// Réduit une réponse `/release/{id}` aux champs que lisent
/// `parse_release_detail` et la cascade.
fn reduire_release(data: &Value) -> Value {
    let credit = |v: &Value| -> Value {
        Value::Array(
            v.get("artist-credit")
                .and_then(|a| a.as_array())
                .map(|a| {
                    a.iter()
                        .map(|c| {
                            json!({
                                "name": c.get("name").cloned().unwrap_or(Value::Null),
                                "joinphrase": c.get("joinphrase").cloned().unwrap_or(json!("")),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        )
    };
    let media: Vec<Value> = data
        .get("media")
        .and_then(|m| m.as_array())
        .map(|ms| {
            ms.iter()
                .map(|m| {
                    let pistes: Vec<Value> = m
                        .get("tracks")
                        .and_then(|t| t.as_array())
                        .map(|ts| {
                            ts.iter()
                                .map(|t| {
                                    json!({
                                        "position": t.get("position").cloned().unwrap_or(Value::Null),
                                        "number": t.get("number").cloned().unwrap_or(Value::Null),
                                        "title": t.get("title").cloned().unwrap_or(Value::Null),
                                        "recording": {
                                            "id": t.get("recording").and_then(|r| r.get("id")).cloned().unwrap_or(Value::Null),
                                        },
                                    })
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({ "position": m.get("position").cloned().unwrap_or(Value::Null), "tracks": pistes })
                })
                .collect()
        })
        .unwrap_or_default();
    let mut o = serde_json::Map::new();
    for k in ["id", "title", "status", "date", "country", "disambiguation"] {
        if let Some(v) = data.get(k).filter(|v| !v.is_null()) {
            o.insert(k.to_string(), v.clone());
        }
    }
    o.insert("artist-credit".into(), credit(data));
    if let Some(id) = data.get("release-group").and_then(|g| g.get("id")) {
        o.insert("release-group".into(), json!({ "id": id }));
    }
    o.insert("media".into(), Value::Array(media));
    Value::Object(o)
}

/// Enregistre les détails de pressage du banc — À LA MAIN seulement :
///
/// ```text
/// TUNE_BANC_4805D_ENREGISTRER=1 [TUNE_BANC_4805D_GRAINE=<fixture de #5864>] \
///   cargo test -p tune-core --lib banc_precision::enregistrer -- --ignored
/// ```
///
/// Ne part vers MusicBrainz que pour un pressage absent de la fixture (et de
/// la graine, la fixture `banc_artistes_4805.json` de #5864, enregistrée le
/// 05/10 avec le même `inc` à `release-groups` près). Une requête / 1,1 s,
/// User-Agent Tune ; un `503` est retenté deux fois après 3 s.
#[tokio::test]
#[ignore = "réseau : réenregistre la fixture du banc de précision #4805 D"]
async fn enregistrer() {
    if std::env::var("TUNE_BANC_4805D_ENREGISTRER").as_deref() != Ok("1") {
        eprintln!("TUNE_BANC_4805D_ENREGISTRER=1 absent : rien n'est enregistré");
        return;
    }
    let mut recherches = charger(&chemin_fixture(), "reponses");
    let mut supplementaires: BTreeMap<String, Value> = if chemin_fixture_precision().exists() {
        charger(&chemin_fixture_precision(), "recherches")
    } else {
        BTreeMap::new()
    };
    for requete in RECHERCHES_SUPPLEMENTAIRES {
        if supplementaires.contains_key(*requete) {
            continue;
        }
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
        let v = super::mb_get(
            "release",
            &[
                ("query", requete.to_string()),
                ("limit", "15".to_string()),
                ("fmt", "json".to_string()),
            ],
        )
        .await
        .unwrap_or_else(|e| panic!("recherche {requete} : {e}"));
        eprintln!("enregistré : {requete}");
        supplementaires.insert(requete.to_string(), reduire(&v));
    }
    recherches.extend(supplementaires.clone());
    let mut depart: BTreeMap<String, Value> = if chemin_fixture_precision().exists() {
        charger(&chemin_fixture_precision(), "releases")
    } else {
        BTreeMap::new()
    };
    let mut graine: BTreeMap<String, Value> = match std::env::var("TUNE_BANC_4805D_GRAINE") {
        Ok(p) => charger(std::path::Path::new(&p), "pressages"),
        Err(_) => BTreeMap::new(),
    };
    let releases: RefCell<BTreeMap<String, Value>> = RefCell::new(std::mem::take(&mut depart));
    let requetes = std::cell::Cell::new(0usize);
    let de_la_graine = std::cell::Cell::new(0usize);

    async fn lire_mb(id: &str) -> Option<Value> {
        for essai in 0..3 {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
            match super::mb_get(
                &format!("release/{id}"),
                &[("inc", INC_DETAIL.to_string()), ("fmt", "json".to_string())],
            )
            .await
            {
                Ok(v) => return Some(v),
                Err(RefusMusicBrainz::Statut(503)) if essai < 2 => {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
                Err(e) => panic!("release {id} : {e}"),
            }
        }
        panic!("release {id} : 503 persistant")
    }

    for cas in les_cas() {
        let lire = |chemin: String, _inc: &'static str| {
            let id = chemin.trim_start_matches("release/").to_string();
            let deja = releases.borrow().get(&id).cloned();
            let semee = graine.remove(&id);
            let releases = &releases;
            let requetes = &requetes;
            let de_la_graine = &de_la_graine;
            async move {
                let v = match (deja, semee) {
                    (Some(v), _) => v,
                    (None, Some(g)) => {
                        de_la_graine.set(de_la_graine.get() + 1);
                        let v = reduire_release(&g);
                        releases.borrow_mut().insert(id, v.clone());
                        v
                    }
                    (None, None) => {
                        requetes.set(requetes.get() + 1);
                        let v = reduire_release(&lire_mb(&id).await.expect("release"));
                        eprintln!("enregistré : release/{id}");
                        releases.borrow_mut().insert(id, v.clone());
                        v
                    }
                };
                Ok::<_, RefusMusicBrainz>(Some(v))
            }
        };
        evaluer(&cas, &recherches, lire).await;
    }
    eprintln!(
        "{} requête(s) MusicBrainz, {} pressage(s) repris de la graine",
        requetes.get(),
        de_la_graine.get()
    );

    let fixture = json!({
        "methode": "Banc #4805 D (précision) : les pressages que lisent les quatre chaînes \
                    et le pressage de référence de chaque album, lus par \
                    /release/{id}?inc=recordings+artist-credits+labels+release-groups. \
                    Graine : banc_artistes_4805.json (#5864, 05/10/2026). Le reste \
                    enregistré auprès de MusicBrainz, 1 requête / 1,1 s, User-Agent \
                    TuneServer. Réponses réduites aux champs lus par parse_release_detail.",
        "enregistre_le": "2026-10-06",
        "recherches": supplementaires,
        "releases": releases.into_inner(),
    });
    std::fs::write(
        chemin_fixture_precision(),
        serde_json::to_string_pretty(&fixture).expect("JSON") + "\n",
    )
    .expect("écriture de la fixture");
}
