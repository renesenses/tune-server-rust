//! Banc de mesure de l'identification d'albums (#4805), sans réseau.
//!
//! Reprend la méthode du constat du 23/09/2026 : pour chaque album, la
//! requête de [`super::lookup_release_candidates`], puis le filtre
//! [`super::plausible`] ; un album est **identifié** quand au moins un
//! pressage plausible revient. Les albums que MusicBrainz a refusés (`503`)
//! sortent du dénominateur, comme au constat.
//!
//! Deux passes sur les MÊMES réponses enregistrées :
//!
//! * **avant** — la chaîne du lot `batch/feat-rc3-20261002` @ `a23ecca31`,
//!   rejouée telle quelle : artiste de l'album brut (à défaut celui de la
//!   première piste), second essai sur le seul nettoyage des suffixes, filtre
//!   contre le titre d'origine ;
//! * **après** — [`super::recherche_de_pressages`] tel qu'il est, avec
//!   [`super::artiste_de_requete`], comme l'appelle `identifier_album`.
//!
//! La base est **synthétique** : 40 albums du commerce, répartis par classe
//! d'échec connue. Les réponses viennent de MusicBrainz, enregistrées une fois
//! le 05/10/2026 (1 requête / 1,1 s, User-Agent Tune) et réduites aux champs
//! que lit [`super::parse_search_results`]. Pour les réenregistrer :
//!
//! ```text
//! TUNE_BANC_4805_ENREGISTRER=1 cargo test -p tune-core --lib \
//!     banc_4805::enregistrer -- --ignored --nocapture
//! ```

use std::cell::RefCell;
use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::{
    RefusMusicBrainz, artiste_de_requete, parse_search_results, recherche_de_pressages,
    requete_lucene, titre_de_requete,
};

/// Ce que `identifier_album` demande : 5 candidats, donc 15 à MusicBrainz.
const CANDIDATS: usize = 5;
const FETCH: usize = 15;

/// `(classe, titre, artiste de l'album, artiste des pistes, nombre de pistes)`.
const ALBUMS: &[(&str, &str, &str, Option<&str>, u32)] = &[
    // Titres propres : trouvés avant comme après.
    ("propre", "Kind of Blue", "Miles Davis", None, 5),
    ("propre", "A Love Supreme", "John Coltrane", None, 4),
    (
        "propre",
        "The Dark Side of the Moon",
        "Pink Floyd",
        None,
        10,
    ),
    ("propre", "Abbey Road", "The Beatles", None, 17),
    ("propre", "Bright Size Life", "Pat Metheny", None, 8),
    (
        "propre",
        "In the Court of the Crimson King",
        "King Crimson",
        None,
        5,
    ),
    ("propre", "Mezzanine", "Massive Attack", None, 11),
    ("propre", "Moon Safari", "Air", None, 10),
    ("propre", "Kid A", "Radiohead", None, 10),
    ("propre", "Rumours", "Fleetwood Mac", None, 11),
    ("propre", "Horses", "Patti Smith", None, 8),
    (
        "propre",
        "Beyond the Missouri Sky (Short Stories)",
        "Charlie Haden",
        None,
        13,
    ),
    // Suffixes de pressage : le correctif du 23/09 (#4812), déjà au lot.
    (
        "suffixe",
        "Somethin' Else (192kHz/24bit)",
        "Cannonball Adderley",
        None,
        5,
    ),
    (
        "suffixe",
        "My Favorite Things (96kHz/24bit)",
        "John Coltrane",
        None,
        4,
    ),
    (
        "suffixe",
        "Wish You Were Here (Remastered)",
        "Pink Floyd",
        None,
        5,
    ),
    ("suffixe", "Nevermind (Deluxe Edition)", "Nirvana", None, 12),
    (
        "suffixe",
        "Blue Train (Rudy Van Gelder Edition)",
        "John Coltrane",
        None,
        7,
    ),
    (
        "suffixe",
        "Tales Of Mystery And Imagination (Original 1976 Version)",
        "The Alan Parsons Project",
        None,
        11,
    ),
    // `Artiste - Album` : le nom du dossier recopié dans la balise d'album.
    (
        "prefixe_artiste",
        "Pink Floyd - The Wall",
        "Pink Floyd",
        None,
        26,
    ),
    (
        "prefixe_artiste",
        "Miles Davis - Bitches Brew",
        "Miles Davis",
        None,
        6,
    ),
    (
        "prefixe_artiste",
        "Radiohead - OK Computer",
        "Radiohead",
        None,
        12,
    ),
    (
        "prefixe_artiste",
        "Daft Punk - Discovery",
        "Daft Punk",
        None,
        14,
    ),
    (
        "prefixe_artiste",
        "Portishead - Dummy",
        "Portishead",
        None,
        11,
    ),
    // `Compositeur: Œuvre`, artiste = l'interprète : le style classique.
    (
        "prefixe_compositeur",
        "Beethoven: Symphony No. 9",
        "Herbert von Karajan",
        None,
        5,
    ),
    (
        "prefixe_compositeur",
        "Chopin - Nocturnes",
        "Maria João Pires",
        None,
        21,
    ),
    (
        "prefixe_compositeur",
        "Bach: Goldberg Variations",
        "Glenn Gould",
        None,
        32,
    ),
    (
        "prefixe_compositeur",
        "Mozart: Requiem",
        "Herbert von Karajan",
        None,
        14,
    ),
    (
        "prefixe_compositeur",
        "Vivaldi - The Four Seasons",
        "Nigel Kennedy",
        None,
        12,
    ),
    (
        "prefixe_compositeur",
        "Debussy: Préludes",
        "Krystian Zimerman",
        None,
        24,
    ),
    (
        "prefixe_compositeur",
        "Bizet: Carmen",
        "Maria Callas",
        None,
        30,
    ),
    // Compilations sous un autre nom que `Various Artists`.
    ("compilation_alias", "Buddha-Bar", "VA", None, 26),
    (
        "compilation_alias",
        "Pulp Fiction",
        "Artistes divers",
        None,
        16,
    ),
    (
        "compilation_alias",
        "Saturday Night Fever",
        "V.A.",
        None,
        17,
    ),
    // Artiste d'album fictif, pistes balisées.
    (
        "artiste_fictif",
        "Talkie Walkie",
        "Unknown Artist",
        Some("Air"),
        10,
    ),
    (
        "artiste_fictif",
        "Homogenic",
        "Artiste inconnu",
        Some("Björk"),
        10,
    ),
    (
        "artiste_fictif",
        "Dummy",
        "Unknown Artist",
        Some("Portishead"),
        11,
    ),
    // Irréductibles : aucune réécriture de requête n'y peut rien.
    (
        "irreductible",
        "Symphonies (intégrale)",
        "Herbert von Karajan",
        None,
        40,
    ),
    (
        "irreductible",
        "Ma compil été 2004",
        "Various Artists",
        None,
        18,
    ),
    (
        "irreductible",
        "Unknown Album",
        "Unknown Artist",
        Some("Unknown Artist"),
        9,
    ),
    ("irreductible", "Disque 1", "Unknown Artist", None, 12),
];

fn chemin_fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/musicbrainz/banc_identification_4805.json")
}

/// L'artiste que la chaîne d'AVANT interrogeait : celui de l'album, à défaut
/// celui de la première piste (`identifier_album` @ `a23ecca31`).
fn artiste_brut(album: &str, pistes: Option<&str>) -> String {
    if album.is_empty() {
        pistes.unwrap_or_default().to_string()
    } else {
        album.to_string()
    }
}

/// Réduit une réponse `/release?query=` aux champs que lit
/// [`parse_search_results`] : la fixture reste lisible et petite.
fn reduire(data: &Value) -> Value {
    let garder = |r: &Value| {
        let mut o = serde_json::Map::new();
        for k in [
            "id",
            "score",
            "title",
            "status",
            "date",
            "country",
            "track-count",
        ] {
            if let Some(v) = r.get(k) {
                o.insert(k.to_string(), v.clone());
            }
        }
        if let Some(ac) = r.get("artist-credit").and_then(|a| a.as_array()) {
            let ac: Vec<Value> = ac
                .iter()
                .map(|c| {
                    json!({
                        "name": c.get("name").cloned().unwrap_or(Value::Null),
                        "joinphrase": c.get("joinphrase").cloned().unwrap_or(json!("")),
                    })
                })
                .collect();
            o.insert("artist-credit".into(), Value::Array(ac));
        }
        if let Some(id) = r.get("release-group").and_then(|g| g.get("id")) {
            o.insert("release-group".into(), json!({ "id": id }));
        }
        Value::Object(o)
    };
    let releases: Vec<Value> = data
        .get("releases")
        .and_then(|r| r.as_array())
        .map(|rs| rs.iter().map(garder).collect())
        .unwrap_or_default();
    json!({ "releases": releases })
}

/// Le verdict d'un album : `Some(true)` identifié, `Some(false)` rien de
/// plausible, `None` refusé par MusicBrainz (hors dénominateur).
type Verdict = Option<bool>;

/// La chaîne d'AVANT, rejouée pas à pas sur `interroger`.
async fn avant<F, Fut>(titre: &str, artiste: &str, mut interroger: F) -> Verdict
where
    F: FnMut(String, usize) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RefusMusicBrainz>>,
{
    let data = interroger(requete_lucene(titre, artiste), FETCH)
        .await
        .ok()?;
    let mut trouves = parse_search_results(&data, titre, artiste);
    if trouves.is_empty() {
        if let Some(nettoye) = titre_de_requete(titre) {
            let data = interroger(requete_lucene(&nettoye, artiste), FETCH)
                .await
                .ok()?;
            trouves = parse_search_results(&data, titre, artiste);
        }
    }
    Some(!trouves.is_empty())
}

/// La chaîne d'APRÈS : celle du module, telle qu'`identifier_album` l'appelle.
async fn apres<F, Fut>(
    titre: &str,
    album: &str,
    pistes: Option<&str>,
    n: u32,
    interroger: F,
) -> (Verdict, Option<String>)
where
    F: FnMut(String, usize) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RefusMusicBrainz>>,
{
    let artiste = artiste_de_requete(Some(album), pistes);
    let r = recherche_de_pressages(titre, &artiste, Some(n), CANDIDATS, interroger).await;
    if r.service_refuse() {
        return (None, None);
    }
    let meilleur = r
        .candidats
        .first()
        .map(|m| format!("{} | {}", m.title, m.artist));
    (Some(!r.candidats.is_empty()), meilleur)
}

fn rejouer(
    reponses: &BTreeMap<String, Value>,
) -> impl FnMut(String, usize) -> std::future::Ready<Result<Value, RefusMusicBrainz>> + '_ {
    move |requete, _fetch| {
        let Some(v) = reponses.get(&requete) else {
            panic!(
                "réponse non enregistrée pour « {requete} » : réenregistrer le banc \
                 (voir l'en-tête du module)"
            );
        };
        std::future::ready(match v.get("refus").and_then(|c| c.as_u64()) {
            Some(code) => Err(RefusMusicBrainz::Statut(code as u16)),
            None => Ok(v.clone()),
        })
    }
}

#[derive(Default)]
struct Compte {
    identifies: usize,
    repondus: usize,
}

impl Compte {
    fn ajouter(&mut self, v: Verdict) {
        if let Some(trouve) = v {
            self.repondus += 1;
            if trouve {
                self.identifies += 1;
            }
        }
    }
    fn taux(&self) -> f64 {
        if self.repondus == 0 {
            0.0
        } else {
            100.0 * self.identifies as f64 / self.repondus as f64
        }
    }
}

/// 🔴 Le banc. Imprime le tableau avant / après par classe, puis garde trois
/// propriétés : le taux monte, aucun album trouvé avant n'est perdu après, et
/// les classes visées passent de rien à quelque chose.
#[tokio::test(start_paused = true)]
async fn banc_identification_4805_avant_apres() {
    let brut = std::fs::read_to_string(chemin_fixture()).expect("fixture du banc #4805");
    let fixture: Value = serde_json::from_str(&brut).expect("fixture JSON");
    let reponses: BTreeMap<String, Value> =
        serde_json::from_value(fixture["reponses"].clone()).expect("reponses");

    let mut par_classe: BTreeMap<&str, (Compte, Compte)> = BTreeMap::new();
    let (mut total_avant, mut total_apres) = (Compte::default(), Compte::default());
    let mut perdus = Vec::new();

    for &(classe, titre, album, pistes, n) in ALBUMS {
        let v_avant = avant(titre, &artiste_brut(album, pistes), rejouer(&reponses)).await;
        let (v_apres, meilleur) = apres(titre, album, pistes, n, rejouer(&reponses)).await;
        if v_avant == Some(true) && v_apres == Some(false) {
            perdus.push(titre);
        }
        println!(
            "{classe:<20} {titre:<58} avant={:<12} après={:<12} {}",
            format!("{v_avant:?}"),
            format!("{v_apres:?}"),
            meilleur.unwrap_or_default()
        );
        let e = par_classe.entry(classe).or_default();
        e.0.ajouter(v_avant);
        e.1.ajouter(v_apres);
        total_avant.ajouter(v_avant);
        total_apres.ajouter(v_apres);
    }

    println!("\n| classe | avant | après |\n|---|---:|---:|");
    for (classe, (a, p)) in &par_classe {
        println!(
            "| {classe} | {}/{} | {}/{} |",
            a.identifies, a.repondus, p.identifies, p.repondus
        );
    }
    println!(
        "| **total** | **{}/{} ({:.1} %)** | **{}/{} ({:.1} %)** |",
        total_avant.identifies,
        total_avant.repondus,
        total_avant.taux(),
        total_apres.identifies,
        total_apres.repondus,
        total_apres.taux()
    );

    assert!(
        perdus.is_empty(),
        "albums trouvés avant et perdus après : {perdus:?}"
    );
    assert!(
        total_apres.identifies > total_avant.identifies,
        "le taux d'identification n'a pas monté : avant {}/{}, après {}/{}",
        total_avant.identifies,
        total_avant.repondus,
        total_apres.identifies,
        total_apres.repondus
    );
    for visee in [
        "prefixe_artiste",
        "prefixe_compositeur",
        "compilation_alias",
        "artiste_fictif",
    ] {
        let (a, p) = &par_classe[visee];
        assert!(
            p.identifies > a.identifies,
            "classe « {visee} » : avant {}/{}, après {}/{} — la réécriture de requête \
             n'y a rien gagné",
            a.identifies,
            a.repondus,
            p.identifies,
            p.repondus
        );
    }
}

/// Enregistre les réponses du banc auprès de MusicBrainz — À LA MAIN
/// seulement, `TUNE_BANC_4805_ENREGISTRER=1`. Une requête / 1,1 s, User-Agent
/// Tune, une réponse par requête distincte (les deux passes partagent leur
/// premier essai). Un `503` est retenté deux fois après 3 s ; un refus qui
/// persiste est enregistré comme tel.
#[tokio::test]
#[ignore = "réseau : réenregistre la fixture du banc #4805"]
async fn enregistrer() {
    if std::env::var("TUNE_BANC_4805_ENREGISTRER").as_deref() != Ok("1") {
        eprintln!("TUNE_BANC_4805_ENREGISTRER=1 absent : rien n'est enregistré");
        return;
    }
    let reponses: RefCell<BTreeMap<String, Value>> = RefCell::new(BTreeMap::new());

    async fn interroger_mb(requete: &str) -> Value {
        for essai in 0..3 {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
            match super::mb_get(
                "release",
                &[
                    ("query", requete.to_string()),
                    ("limit", FETCH.to_string()),
                    ("fmt", "json".to_string()),
                ],
            )
            .await
            {
                Ok(data) => return reduire(&data),
                Err(RefusMusicBrainz::Statut(503)) if essai < 2 => {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
                Err(RefusMusicBrainz::Statut(code)) => return json!({ "refus": code }),
                Err(_) => return json!({ "refus": 0 }),
            }
        }
        json!({ "refus": 503 })
    }

    for &(_, titre, album, pistes, n) in ALBUMS {
        // Les deux passes, sur un enregistreur commun : chaque requête
        // distincte part une fois.
        for passe in 0..2 {
            let enregistrer = |requete: String, _fetch: usize| {
                let deja = reponses.borrow().get(&requete).cloned();
                let reponses = &reponses;
                async move {
                    let v = match deja {
                        Some(v) => v,
                        None => {
                            let v = interroger_mb(&requete).await;
                            eprintln!("enregistré : {requete}");
                            reponses.borrow_mut().insert(requete, v.clone());
                            v
                        }
                    };
                    match v.get("refus").and_then(|c| c.as_u64()) {
                        Some(code) => Err(RefusMusicBrainz::Statut(code as u16)),
                        None => Ok(v),
                    }
                }
            };
            if passe == 0 {
                avant(titre, &artiste_brut(album, pistes), enregistrer).await;
            } else {
                apres(titre, album, pistes, n, enregistrer).await;
            }
        }
    }

    let fixture = json!({
        "methode": "Banc #4805 : requête de lookup_release_candidates (limit=15), filtre \
                    plausible(). Enregistré auprès de MusicBrainz, 1 requête / 1,1 s, \
                    User-Agent TuneServer. Réponses réduites aux champs lus par \
                    parse_search_results. Albums : base synthétique, voir ALBUMS.",
        "enregistre_le": "2026-10-05",
        "reponses": reponses.into_inner(),
    });
    std::fs::write(
        chemin_fixture(),
        serde_json::to_string_pretty(&fixture).expect("JSON") + "\n",
    )
    .expect("écriture de la fixture");
}

// ---- Étape B : le MBID des artistes, tiré du pressage identifié ----------
//
// Sur les MÊMES albums et les MÊMES réponses de recherche : pour chaque album
// identifié par la chaîne d'après, le pressage retenu (le premier candidat,
// celui que garde `identifier_album`) est relu dans une seconde fixture, qui
// porte la réponse `/release/{id}?inc=recordings+artist-credits+labels` —
// exactement la requête que `identifier_album` fait déjà. Aucune requête de
// plus en service : la fixture ne fait que figer cette réponse.
//
// Pour la réenregistrer (une requête par album identifié, 1 / 1,1 s) :
//
// ```text
// TUNE_BANC_4805B_ENREGISTRER=1 cargo test -p tune-core --lib \
//     banc_4805::enregistrer_les_pressages -- --ignored --nocapture
// ```

fn chemin_fixture_pressages() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/musicbrainz/banc_artistes_4805.json")
}

/// Réduit un `artist-credit` aux champs que lit le rattachement.
fn reduire_credit(v: &Value) -> Option<Value> {
    let ac = v.get("artist-credit")?.as_array()?;
    Some(Value::Array(
        ac.iter()
            .map(|c| {
                let a = c.get("artist").cloned().unwrap_or(Value::Null);
                json!({
                    "name": c.get("name").cloned().unwrap_or(Value::Null),
                    "joinphrase": c.get("joinphrase").cloned().unwrap_or(json!("")),
                    "artist": {
                        "id": a.get("id").cloned().unwrap_or(Value::Null),
                        "name": a.get("name").cloned().unwrap_or(Value::Null),
                        "sort-name": a.get("sort-name").cloned().unwrap_or(Value::Null),
                    },
                })
            })
            .collect(),
    ))
}

/// Réduit une réponse `/release/{id}` aux champs que lisent
/// [`super::parse_release_detail`] et le rattachement des artistes.
fn reduire_detail(data: &Value) -> Value {
    let mut o = serde_json::Map::new();
    for k in ["id", "title", "date", "country"] {
        if let Some(v) = data.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    if let Some(ac) = reduire_credit(data) {
        o.insert("artist-credit".into(), ac);
    }
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
                                    let mut p = serde_json::Map::new();
                                    for k in ["position", "number", "title"] {
                                        if let Some(v) = t.get(k) {
                                            p.insert(k.into(), v.clone());
                                        }
                                    }
                                    if let Some(ac) = reduire_credit(t) {
                                        p.insert("artist-credit".into(), ac);
                                    }
                                    if let Some(r) = t.get("recording") {
                                        p.insert(
                                            "recording".into(),
                                            json!({ "id": r.get("id"), "title": r.get("title") }),
                                        );
                                    }
                                    Value::Object(p)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    json!({ "position": m.get("position"), "tracks": pistes })
                })
                .collect()
        })
        .unwrap_or_default();
    o.insert("media".into(), Value::Array(media));
    Value::Object(o)
}

/// Le pressage retenu pour chaque album identifié par la chaîne d'après,
/// rejouée sur la fixture de recherche : `(index dans ALBUMS, release_id)`.
async fn pressages_retenus(reponses: &BTreeMap<String, Value>) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for (i, &(_, titre, album, pistes, n)) in ALBUMS.iter().enumerate() {
        let artiste = artiste_de_requete(Some(album), pistes);
        let r =
            recherche_de_pressages(titre, &artiste, Some(n), CANDIDATS, rejouer(reponses)).await;
        if let Some(m) = r.meilleur() {
            out.push((i, m.release_id));
        }
    }
    out
}

fn reponses_de_recherche() -> BTreeMap<String, Value> {
    let brut = std::fs::read_to_string(chemin_fixture()).expect("fixture du banc #4805");
    let fixture: Value = serde_json::from_str(&brut).expect("fixture JSON");
    serde_json::from_value(fixture["reponses"].clone()).expect("reponses")
}

/// MBID connus, relevés À LA MAIN sur musicbrainz.org et non tirés de la
/// fixture : ils vérifient que le rattachement pose le BON identifiant, et
/// pas seulement un identifiant.
const MBID_ATTENDUS: &[(&str, &str)] = &[
    ("Miles Davis", "561d854a-6a28-4aa7-8c99-323e6ce46c2a"),
    ("John Coltrane", "b625448e-bf4a-41c3-a421-72ad46cdb831"),
    ("Pink Floyd", "83d91898-7763-47d7-b03b-b92132375c47"),
    ("The Beatles", "b10bbbfc-cf9e-42e0-be17-e2c3e1d2600d"),
    ("Radiohead", "a74b1b7f-71a5-4011-9441-d0b5e4122711"),
    ("Nirvana", "5b11f4ce-a62d-471e-81fc-a69a8278c7da"),
];

/// 🔴 Le banc de l'étape B. Une bibliothèque synthétique — une fiche par
/// artiste distinct (au sens de `cle_artiste`), un album par ligne de
/// `ALBUMS`, ses pistes calquées sur le pressage (titres et rangs) — puis, pour
/// chaque album identifié, le rattachement tel que l'appelle
/// `identifier_album`. Mesure : la part des fiches munies d'un MBID, avant et
/// après.
/// La bibliothèque synthétique du banc de l'étape B, AVANT rattachement :
/// une fiche par artiste distinct (au sens de `cle_artiste`), un album par
/// ligne de `ALBUMS`, et pour chaque album identifié ses pistes calquées sur
/// le pressage retenu. Rend la base et, par album identifié, de quoi
/// rejouer le rattachement : `(album_id, pressage, pistes locales)`.
/// Partagée avec le banc de l'étape C.
pub(super) async fn bibliotheque_du_banc_b() -> (
    std::sync::Arc<dyn crate::db::backend::DbBackend>,
    Vec<(
        i64,
        super::MBReleaseDetail,
        Vec<crate::metadata::reidentify::LocalTrack>,
    )>,
) {
    use std::sync::Arc;

    use crate::db::artist_repo::cle_artiste;
    use crate::db::backend::{DbBackend, ToSqlValue};
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::metadata::reidentify::LocalTrack;

    let brut = std::fs::read_to_string(chemin_fixture_pressages()).expect("fixture des pressages");
    let fixture: Value = serde_json::from_str(&brut).expect("fixture JSON");
    let pressages: BTreeMap<String, Value> =
        serde_json::from_value(fixture["pressages"].clone()).expect("pressages");
    let retenus = pressages_retenus(&reponses_de_recherche()).await;

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);

    // Une fiche par artiste distinct, comme le scan les dédoublonne.
    let mut fiches: BTreeMap<String, i64> = BTreeMap::new();
    let mut fiche = |nom: &str| -> i64 {
        let n = fiches.len() as i64 + 1;
        let id = *fiches.entry(cle_artiste(nom)).or_insert(n);
        if id == n {
            backend
                .execute(
                    "INSERT INTO artists (id, name) VALUES (?, ?)",
                    &[&id as &dyn ToSqlValue, &nom.to_string() as &dyn ToSqlValue],
                )
                .unwrap();
        }
        id
    };

    // Les albums, et pour les identifiés leurs pistes calquées sur le pressage.
    let mut details = Vec::new();
    let mut piste_id = 1000i64;
    for (i, &(_, titre, album, pistes, n)) in ALBUMS.iter().enumerate() {
        let album_id = i as i64 + 1;
        let aid = fiche(album);
        let pid = pistes.map(&mut fiche).unwrap_or(aid);
        backend
            .execute(
                "INSERT INTO albums (id, title, artist_id) VALUES (?, ?, ?)",
                &[
                    &album_id as &dyn ToSqlValue,
                    &titre.to_string() as &dyn ToSqlValue,
                    &aid as &dyn ToSqlValue,
                ],
            )
            .unwrap();
        let Some((_, rid)) = retenus.iter().find(|(j, _)| *j == i) else {
            continue;
        };
        let data = pressages
            .get(rid)
            .unwrap_or_else(|| panic!("pressage {rid} non enregistré : réenregistrer"));
        let detail = super::parse_release_detail(data).expect("détail");
        let mut locales = Vec::new();
        for t in detail.tracks.iter().take(n as usize) {
            piste_id += 1;
            backend
                .execute(
                    "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number) \
                     VALUES (?, ?, ?, ?, ?, ?)",
                    &[
                        &piste_id as &dyn ToSqlValue,
                        &t.title as &dyn ToSqlValue,
                        &album_id as &dyn ToSqlValue,
                        &pid as &dyn ToSqlValue,
                        &(t.disc as i64) as &dyn ToSqlValue,
                        &(t.position as i64) as &dyn ToSqlValue,
                    ],
                )
                .unwrap();
            locales.push(LocalTrack {
                id: piste_id,
                disc: t.disc as i32,
                position: t.position as i32,
                title: t.title.clone(),
            });
        }
        details.push((album_id, detail, locales));
    }
    (backend, details)
}

#[tokio::test(start_paused = true)]
async fn banc_artistes_4805_avant_apres() {
    use std::sync::Arc;

    use crate::db::artist_repo::cle_artiste;
    use crate::db::backend::{DbBackend, ToSqlValue};
    use crate::metadata::artistes_du_pressage::rattacher_les_artistes_de_l_album;
    use crate::metadata::reidentify::map_recording_ids;

    let (backend, details) = bibliotheque_du_banc_b().await;

    let compter = |backend: &Arc<dyn DbBackend>| -> (i64, i64) {
        let l = backend
            .query_one(
                "SELECT COUNT(*), SUM(CASE WHEN TRIM(COALESCE(musicbrainz_id, '')) <> '' \
                 THEN 1 ELSE 0 END) FROM artists",
                &[],
            )
            .unwrap()
            .unwrap();
        (
            l[0].as_i64().unwrap_or(0),
            l.get(1).and_then(|v| v.as_i64()).unwrap_or(0),
        )
    };
    let (total, avant) = compter(&backend);

    // « Avant » : la chaîne d'avant identifie l'album et ne touche à aucune
    // fiche d'artiste. « Après » : le rattachement, album par album.
    let (mut ambigus, mut desaccords, mut deja, mut sans, mut ecartes, mut refuses) =
        (0, 0, 0, 0, 0, 0);
    for (album_id, detail, locales) in &details {
        let recordings = map_recording_ids(locales, &detail.tracks);
        let b = rattacher_les_artistes_de_l_album(&backend, *album_id, detail, &recordings)
            .expect("rattachement");
        let titre = ALBUMS[*album_id as usize - 1].1;
        println!(
            "{titre:<58} écrits={} déjà={} désaccords={} ambigus={} sans={} écartés={} refusés={}",
            b.ecrits,
            b.deja_poses,
            b.desaccords,
            b.ambigus,
            b.sans_correspondance,
            b.ecartes,
            b.refuses_par_la_base
        );
        ambigus += b.ambigus;
        desaccords += b.desaccords;
        deja += b.deja_poses;
        sans += b.sans_correspondance;
        ecartes += b.ecartes;
        refuses += b.refuses_par_la_base;
    }
    let (_, apres) = compter(&backend);

    let taux = |n: i64| 100.0 * n as f64 / total.max(1) as f64;
    println!(
        "\nalbums identifiés : {}/{}\nfiches d'artiste munies d'un MBID : avant {avant}/{total} \
         ({:.1} %), après {apres}/{total} ({:.1} %)\nconfirmations (album suivant du même \
         artiste) : {deja} · désaccords : {desaccords} · ambigus : {ambigus} · sans \
         correspondance : {sans} · écartés : {ecartes} · refusés par la base : {refuses}",
        details.len(),
        ALBUMS.len(),
        taux(avant),
        taux(apres),
    );

    // Les fiches restées sans MBID, nommément : c'est le reliquat de l'étape C.
    let restes = backend
        .query_many(
            "SELECT name FROM artists WHERE TRIM(COALESCE(musicbrainz_id, '')) = '' ORDER BY id",
            &[],
        )
        .unwrap();
    let restes: Vec<String> = restes
        .iter()
        .filter_map(|l| l.first().and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    println!("restées sans MBID : {restes:?}");

    assert_eq!(avant, 0, "la bibliothèque synthétique part sans aucun MBID");
    assert!(
        apres > avant,
        "le taux d'artistes munis d'un MBID n'a pas monté : avant {avant}/{total}, après \
         {apres}/{total}"
    );
    for (nom, attendu) in MBID_ATTENDUS {
        let l = backend
            .query_one(
                "SELECT musicbrainz_id FROM artists WHERE name = ?",
                &[&nom.to_string() as &dyn ToSqlValue],
            )
            .unwrap()
            .unwrap_or_else(|| panic!("fiche {nom} absente du banc"));
        assert_eq!(
            l.first().and_then(|v| v.as_str()),
            Some(*attendu),
            "{nom} : mauvais MBID, ou aucun"
        );
    }
    // Les fiches fictives et les alias de compilation ne reçoivent rien.
    let munies = backend
        .query_many(
            "SELECT name FROM artists WHERE TRIM(COALESCE(musicbrainz_id, '')) <> ''",
            &[],
        )
        .unwrap();
    for l in &munies {
        let nom = l.first().and_then(|v| v.as_str()).unwrap_or_default();
        assert!(
            !super::est_un_artiste_fictif(nom)
                && !["va", "artistes divers", "various artists"]
                    .contains(&cle_artiste(nom).as_str()),
            "{nom} a reçu un MBID"
        );
    }
}

/// Enregistre le détail du pressage retenu pour chaque album identifié — À LA
/// MAIN seulement, `TUNE_BANC_4805B_ENREGISTRER=1`. Une requête / 1,1 s,
/// User-Agent Tune, la requête même d'`identifier_album`.
#[tokio::test]
#[ignore = "réseau : réenregistre la fixture des pressages du banc #4805"]
async fn enregistrer_les_pressages() {
    if std::env::var("TUNE_BANC_4805B_ENREGISTRER").as_deref() != Ok("1") {
        eprintln!("TUNE_BANC_4805B_ENREGISTRER=1 absent : rien n'est enregistré");
        return;
    }
    let retenus = pressages_retenus(&reponses_de_recherche()).await;
    let mut pressages: BTreeMap<String, Value> = BTreeMap::new();
    for (_, rid) in retenus {
        if pressages.contains_key(&rid) {
            continue;
        }
        let mut v = json!({ "refus": 503 });
        for essai in 0..3 {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
            match super::mb_get(
                &format!("release/{rid}"),
                &[
                    ("inc", "recordings+artist-credits+labels".to_string()),
                    ("fmt", "json".to_string()),
                ],
            )
            .await
            {
                Ok(data) => {
                    v = reduire_detail(&data);
                    break;
                }
                Err(RefusMusicBrainz::Statut(503)) if essai < 2 => {
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
                Err(e) => {
                    v = json!({ "refus": e.to_string() });
                    break;
                }
            }
        }
        eprintln!("enregistré : release/{rid}");
        pressages.insert(rid, v);
    }
    let fixture = json!({
        "methode": "Banc #4805, étape B : pour chaque album identifié par la chaîne d'après \
                    (fixture banc_identification_4805.json), le pressage retenu lu par \
                    /release/{id}?inc=recordings+artist-credits+labels, la requête même \
                    d'identifier_album. 1 requête / 1,1 s, User-Agent TuneServer. Réponses \
                    réduites aux champs lus par parse_release_detail.",
        "enregistre_le": "2026-10-05",
        "pressages": pressages,
    });
    std::fs::write(
        chemin_fixture_pressages(),
        serde_json::to_string_pretty(&fixture).expect("JSON") + "\n",
    )
    .expect("écriture de la fixture");
}

// #4805, étape C : la passe artistes par le réseau, sur cette bibliothèque.
#[path = "musicbrainz_release_banc_4805_c.rs"]
mod banc_c;
