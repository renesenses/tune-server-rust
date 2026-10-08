//! Banc de l'étape C (#4805) : le MBID des artistes que l'étape B n'a pas
//! rattachés, par la passe réseau de
//! [`crate::metadata::artistes_par_le_reseau`], sans réseau au rejeu.
//!
//! La bibliothèque est celle du banc de l'étape B
//! ([`super::bibliotheque_du_banc_b`] : 40 albums, 29 fiches), à laquelle
//! s'ajoute [`EXTENSION`] : des albums locaux **non identifiés**, dont les
//! artistes relèvent des deux cas que B ne couvre pas (nom local absent des
//! crédits MusicBrainz, artiste sans album identifié), plus trois témoins qui
//! ne doivent rien recevoir. B ne pose rien sur un album non identifié : la
//! mesure « après B » est donc celle du banc de B, sur la bibliothèque
//! élargie.
//!
//! Les réponses viennent de MusicBrainz, enregistrées une fois le 05/10/2026
//! depuis Shrek (1 requête / 1,1 s, User-Agent Tune, au plus
//! [`REQUETES_MAX`] requêtes), réduites aux champs lus. Pour réenregistrer :
//!
//! ```text
//! TUNE_BANC_4805C_ENREGISTRER=1 cargo test -p tune-core --lib \
//!     banc_c::enregistrer -- --ignored --nocapture
//! ```

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use serde_json::{Value, json};

use crate::db::artist_repo::cle_artiste;
use crate::db::backend::{DbBackend, ToSqlValue};
use crate::metadata::artistes_du_pressage::rattacher_les_artistes_de_l_album;
use crate::metadata::artistes_par_le_reseau::{BilanReseau, Entite, candidats, traiter_un_artiste};
use crate::metadata::musicbrainz_release::{MB_UA, RefusMusicBrainz};
use crate::metadata::reidentify::map_recording_ids;

/// Le plafond de l'enregistrement : au-delà, il s'arrête en échec.
const REQUETES_MAX: usize = 50;

/// `(classe, titre de l'album, artiste de l'album, pistes (titre, artiste
/// de la piste ; `None` = celui de l'album))`.
#[allow(clippy::type_complexity)]
const EXTENSION: &[(&str, &str, &str, &[(&str, Option<&str>)])] = &[
    // Nom local absent des crédits : alias, graphie française, translittération.
    (
        "alias",
        "Zombie",
        "Fela Anikulapo Kuti",
        &[("Zombie", None), ("Mr. Follow Follow", None)],
    ),
    (
        "translitteration",
        "async",
        "Ryuichi Sakamoto",
        &[("andata", None), ("solari", None)],
    ),
    (
        "alias",
        "Tchaïkovski - Casse-Noisette",
        "Tchaïkovski",
        &[("Valse des fleurs", None)],
    ),
    (
        "alias",
        "Bach - Suites pour violoncelle",
        "Mstislav Rostropovitch",
        &[("Suite No. 1 in G major, BWV 1007: I. Prélude", None)],
    ),
    // Aucun album identifié : un titre personnel, des pistes du commerce.
    (
        "sans_album",
        "Concert à Cologne",
        "Keith Jarrett",
        &[("Köln, January 24, 1975, Part I", None)],
    ),
    (
        "sans_album",
        "Best of perso",
        "Nina Simone",
        &[
            ("Feeling Good", None),
            ("My Baby Just Cares for Me", None),
            ("Sinnerman", None),
        ],
    ),
    (
        "sans_album",
        "Live perso",
        "Guns N\u{2019} Roses",
        &[("Sweet Child O' Mine", None)],
    ),
    // Artiste de piste seulement, sur une compilation locale.
    (
        "artiste_de_piste",
        "Ma compil jazz",
        "Various Artists",
        &[("My Funny Valentine", Some("Chet Baker"))],
    ),
    // Homonymes : le départage par la bibliothèque.
    (
        "homonyme",
        "Prince - Les tubes",
        "Prince",
        &[("When Doves Cry", None), ("Purple Rain", None)],
    ),
    // Témoins : rien ne doit être posé.
    (
        "temoin_homonymes",
        "Mes musiques de films",
        "John Williams",
        &[("Thème principal", None)],
    ),
    (
        "temoin_non_confirme",
        "Vacances 1998 (cassette)",
        "Daniel Guichard",
        &[("Piste 01", None), ("Piste 02", None)],
    ),
    (
        "temoin_inconnu",
        "Répétition 2003",
        "Les Copains du Lycée de Bligny",
        &[("Morceau 1", None)],
    ),
];

fn chemin_fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/musicbrainz/banc_artistes_reseau_4805.json")
}

fn cle_de_requete(entite: Entite, requete: &str) -> String {
    format!("{}?{requete}", entite.chemin())
}

/// La bibliothèque du banc de B, B appliquée, puis l'extension. Rend la base
/// et, par fiche de l'extension, sa classe.
async fn bibliotheque() -> (Arc<dyn DbBackend>, BTreeMap<i64, &'static str>) {
    let (backend, details) = super::bibliotheque_du_banc_b().await;
    for (album_id, detail, locales) in &details {
        let recordings = map_recording_ids(locales, &detail.tracks);
        rattacher_les_artistes_de_l_album(&backend, *album_id, detail, &recordings)
            .expect("rattachement de l'étape B");
    }

    let un = |sql: &str| -> i64 {
        backend
            .query_one(sql, &[])
            .unwrap()
            .and_then(|l| l.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
    };
    let mut prochaine_fiche = un("SELECT MAX(id) FROM artists") + 1;
    let mut album_id = un("SELECT MAX(id) FROM albums") + 1;
    let mut piste_id = un("SELECT MAX(id) FROM tracks") + 1;
    let mut classes = BTreeMap::new();

    // Le scan dédoublonne les fiches par `cle_artiste`.
    let mut fiche = |nom: &str, classe: &'static str| -> i64 {
        let cle = cle_artiste(nom);
        let existantes = backend
            .query_many("SELECT id, name FROM artists", &[])
            .unwrap();
        if let Some(id) = existantes.iter().find_map(|l| {
            let n = l.get(1)?.as_str()?;
            (cle_artiste(n) == cle).then(|| l.first()?.as_i64())?
        }) {
            return id;
        }
        let id = prochaine_fiche;
        prochaine_fiche += 1;
        backend
            .execute(
                "INSERT INTO artists (id, name) VALUES (?, ?)",
                &[&id as &dyn ToSqlValue, &nom.to_string() as &dyn ToSqlValue],
            )
            .unwrap();
        classes.insert(id, classe);
        id
    };

    for &(classe, titre, artiste_album, pistes) in EXTENSION {
        let aid = fiche(artiste_album, classe);
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
        for (rang, (titre_piste, artiste_piste)) in pistes.iter().enumerate() {
            let pid = artiste_piste.map(|a| fiche(a, classe)).unwrap_or(aid);
            backend
                .execute(
                    "INSERT INTO tracks (id, title, album_id, artist_id, disc_number, track_number) \
                     VALUES (?, ?, ?, ?, 1, ?)",
                    &[
                        &piste_id as &dyn ToSqlValue,
                        &titre_piste.to_string() as &dyn ToSqlValue,
                        &album_id as &dyn ToSqlValue,
                        &pid as &dyn ToSqlValue,
                        &(rang as i64 + 1) as &dyn ToSqlValue,
                    ],
                )
                .unwrap();
            piste_id += 1;
        }
        album_id += 1;
    }
    (backend, classes)
}

fn compter(backend: &Arc<dyn DbBackend>) -> (i64, i64) {
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
}

fn mbid_par_fiche(backend: &Arc<dyn DbBackend>) -> BTreeMap<i64, (String, Option<String>)> {
    backend
        .query_many(
            "SELECT id, name, musicbrainz_id FROM artists ORDER BY id",
            &[],
        )
        .unwrap()
        .iter()
        .filter_map(|l| {
            Some((
                l.first()?.as_i64()?,
                (
                    l.get(1)?.as_string()?,
                    l.get(2)
                        .and_then(|v| v.as_string())
                        .filter(|m| !m.trim().is_empty()),
                ),
            ))
        })
        .collect()
}

/// Le verdict d'une fiche, tiré de ce qu'elle a fait bouger au bilan.
fn verdict(avant: &BilanReseau, apres: &BilanReseau) -> &'static str {
    let d = |a: usize, b: usize| b > a;
    if d(avant.departages, apres.departages) {
        "posé (départage)"
    } else if d(avant.poses, apres.poses) {
        "posé"
    } else if d(avant.refuses_par_la_base, apres.refuses_par_la_base) {
        "refusé par la base"
    } else if d(avant.ambigus, apres.ambigus) {
        "ambigu"
    } else if d(avant.non_confirmes, apres.non_confirmes) {
        "non confirmé"
    } else if d(avant.sans_correspondance, apres.sans_correspondance) {
        "sans correspondance"
    } else if d(avant.ecartes, apres.ecartes) {
        "écarté"
    } else {
        "panne"
    }
}

/// La passe C, telle que le pilote l'enchaîne : la sélection réelle, puis
/// chaque fiche par `traiter_un_artiste`. Rend le bilan et le verdict par
/// fiche.
async fn passe_c<F, Fut>(
    backend: &Arc<dyn DbBackend>,
    mut interroger: F,
) -> (BilanReseau, BTreeMap<i64, &'static str>)
where
    F: FnMut(Entite, String) -> Fut,
    Fut: std::future::Future<Output = Result<Value, RefusMusicBrainz>>,
{
    let mut bilan = BilanReseau::default();
    let mut verdicts = BTreeMap::new();
    for (id, nom) in candidats(backend, 0, 1000).expect("sélection") {
        let avant = bilan.clone();
        traiter_un_artiste(backend, id, &nom, &mut interroger, &mut bilan)
            .await
            .expect("passe C");
        verdicts.insert(id, verdict(&avant, &bilan));
    }
    (bilan, verdicts)
}

fn reponses() -> BTreeMap<String, Value> {
    let brut = std::fs::read_to_string(chemin_fixture()).expect("fixture du banc C");
    let fixture: Value = serde_json::from_str(&brut).expect("fixture JSON");
    serde_json::from_value(fixture["reponses"].clone()).expect("reponses")
}

/// 🔴 Le banc de l'étape C. Mesure la part des fiches munies d'un MBID après
/// B, puis après B+C, sur la même bibliothèque ; garde que le taux monte, que
/// les témoins ne reçoivent rien, et qu'aucun MBID posé par B ne bouge.
#[tokio::test(start_paused = true)]
async fn banc_artistes_reseau_4805_c() {
    let reponses = reponses();
    let (backend, classes) = bibliotheque().await;
    let (total, apres_b) = compter(&backend);
    let fiches_b = mbid_par_fiche(&backend);

    let (bilan, verdicts) = passe_c(&backend, |entite, requete| {
        let cle = cle_de_requete(entite, &requete);
        let Some(v) = reponses.get(&cle) else {
            panic!("réponse non enregistrée pour « {cle} » : réenregistrer le banc C");
        };
        std::future::ready(match v.get("refus").and_then(|c| c.as_u64()) {
            Some(code) => Err(RefusMusicBrainz::Statut(code as u16)),
            None => Ok(v.clone()),
        })
    })
    .await;
    let (_, apres_c) = compter(&backend);
    let fiches_c = mbid_par_fiche(&backend);

    println!("{:<20} {:<32} {:<22} MBID", "classe", "fiche", "verdict C");
    for (id, (nom, mbid)) in &fiches_c {
        let Some(v) = verdicts.get(id) else { continue };
        let nom_mb = mbid.as_deref().and_then(|m| nom_musicbrainz(&reponses, m));
        println!(
            "{:<20} {:<32} {:<22} {} {}",
            classes.get(id).copied().unwrap_or("banc B"),
            nom,
            v,
            mbid.as_deref().unwrap_or("-"),
            nom_mb.map(|n| format!("({n})")).unwrap_or_default()
        );
    }
    let taux = |n: i64| 100.0 * n as f64 / total.max(1) as f64;
    let interroges = bilan.traites - bilan.ecartes;
    println!(
        "\nfiches munies d'un MBID : après B {apres_b}/{total} ({:.1} %), après B+C \
         {apres_c}/{total} ({:.1} %)\nC : {} posés (dont {} par départage), {} ambigus, {} non \
         confirmés, {} sans correspondance, {} écartés sans requête, {} refusés par la base, {} \
         pannes\nrequêtes : {} pour {interroges} fiches interrogées ({:.2} par fiche)",
        taux(apres_b),
        taux(apres_c),
        bilan.poses,
        bilan.departages,
        bilan.ambigus,
        bilan.non_confirmes,
        bilan.sans_correspondance,
        bilan.ecartes,
        bilan.refuses_par_la_base,
        bilan.pannes,
        bilan.requetes,
        bilan.requetes as f64 / interroges.max(1) as f64,
    );

    assert!(
        apres_c > apres_b,
        "le taux n'a pas monté : après B {apres_b}/{total}, après B+C {apres_c}/{total}"
    );
    assert_eq!(bilan.pannes, 0, "le rejeu ne doit rencontrer aucun refus");
    for (id, (nom, mbid_b)) in &fiches_b {
        if let Some(m) = mbid_b {
            assert_eq!(
                fiches_c[id].1.as_ref(),
                Some(m),
                "{nom} : le MBID posé par B a bougé"
            );
        }
    }
    for (id, classe) in &classes {
        if classe.starts_with("temoin") {
            assert_eq!(
                fiches_c[id].1, None,
                "témoin {} ({classe}) : il a reçu un MBID",
                fiches_c[id].0
            );
        }
    }
    for (nom, mbid) in fiches_c.values() {
        assert!(
            mbid.is_none()
                || !(super::super::est_un_artiste_fictif(nom)
                    || ["va", "artistes divers", "various artists"]
                        .contains(&cle_artiste(nom).as_str())),
            "{nom} a reçu un MBID"
        );
    }
}

/// Le nom MusicBrainz d'un MBID, relu dans les réponses de recherche
/// enregistrées.
fn nom_musicbrainz(reponses: &BTreeMap<String, Value>, mbid: &str) -> Option<String> {
    reponses.values().find_map(|v| {
        v.get("artists")?.as_array()?.iter().find_map(|a| {
            (a.get("id")?.as_str()? == mbid).then(|| a.get("name")?.as_str().map(str::to_string))?
        })
    })
}

/// Réduit une réponse aux champs que lit la passe.
fn reduire(entite: Entite, data: &Value) -> Value {
    let credits = |r: &Value| -> Value {
        Value::Array(
            r.get("artist-credit")
                .and_then(|c| c.as_array())
                .map(|cs| {
                    cs.iter()
                        .map(|c| {
                            json!({
                                "name": c.get("name"),
                                "artist": {
                                    "id": c.pointer("/artist/id"),
                                    "name": c.pointer("/artist/name"),
                                },
                            })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        )
    };
    match entite {
        Entite::Artiste => json!({
            "artists": data.get("artists").and_then(|a| a.as_array()).map(|l| l.iter().map(|a| json!({
                "id": a.get("id"),
                "name": a.get("name"),
                "sort-name": a.get("sort-name"),
                "score": a.get("score"),
                "disambiguation": a.get("disambiguation"),
                "aliases": a.get("aliases").and_then(|x| x.as_array()).map(|al| al.iter().map(|x| json!({
                    "name": x.get("name"),
                    "sort-name": x.get("sort-name"),
                })).collect::<Vec<_>>()).unwrap_or_default(),
            })).collect::<Vec<_>>()).unwrap_or_default(),
        }),
        _ => {
            let cle = if entite == Entite::GroupeDeSortie {
                "release-groups"
            } else {
                "recordings"
            };
            json!({ cle: data.get(cle).and_then(|a| a.as_array()).map(|l| l.iter().map(|r| json!({
                "id": r.get("id"),
                "title": r.get("title"),
                "score": r.get("score"),
                "artist-credit": credits(r),
            })).collect::<Vec<_>>()).unwrap_or_default() })
        }
    }
}

/// Enregistre les réponses — À LA MAIN seulement,
/// `TUNE_BANC_4805C_ENREGISTRER=1`. Rejoue la passe C sur la bibliothèque du
/// banc ; chaque requête absente de la fixture part vers MusicBrainz (1,1 s
/// d'écart, User-Agent Tune), au plus [`REQUETES_MAX`].
#[tokio::test]
#[ignore = "réseau : réenregistre la fixture du banc C (#4805)"]
async fn enregistrer() {
    if std::env::var("TUNE_BANC_4805C_ENREGISTRER").as_deref() != Ok("1") {
        eprintln!("TUNE_BANC_4805C_ENREGISTRER=1 absent : rien n'est enregistré");
        return;
    }
    let (backend, _) = bibliotheque().await;
    let enregistrees: Rc<RefCell<BTreeMap<String, Value>>> = Rc::default();
    let envoyees = Rc::new(RefCell::new(0usize));
    let client = crate::http::client::shared();
    let (bilan, _) = passe_c(&backend, |entite, requete| {
        let enregistrees = enregistrees.clone();
        let envoyees = envoyees.clone();
        let client = client.clone();
        async move {
            let cle = cle_de_requete(entite, &requete);
            if let Some(v) = enregistrees.borrow().get(&cle) {
                return Ok(v.clone());
            }
            let mut essai = 0;
            let resp = loop {
                // Le plafond ne fait pas échouer l'enregistrement : la requête
                // n'est pas envoyée, la fiche compte une panne, et la fixture
                // est écrite avec ce qui a été reçu.
                if *envoyees.borrow() >= REQUETES_MAX {
                    eprintln!("plafond de {REQUETES_MAX} requêtes : {cle} non envoyée");
                    return Err(RefusMusicBrainz::Transport);
                }
                *envoyees.borrow_mut() += 1;
                tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
                let resp = client
                    .get(format!("https://musicbrainz.org/ws/2/{}", entite.chemin()))
                    .query(&[
                        ("query", requete.as_str()),
                        ("limit", entite.limite().to_string().as_str()),
                        ("fmt", "json"),
                    ])
                    .header("User-Agent", MB_UA)
                    .timeout(std::time::Duration::from_secs(15))
                    .send()
                    .await
                    .map_err(|_| RefusMusicBrainz::Transport)?;
                let statut = resp.status().as_u16();
                eprintln!("{statut} {cle}");
                if statut == 503 && essai == 0 {
                    essai += 1;
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    continue;
                }
                if !resp.status().is_success() {
                    enregistrees
                        .borrow_mut()
                        .insert(cle, json!({ "refus": statut }));
                    return Err(RefusMusicBrainz::Statut(statut));
                }
                break resp;
            };
            let data: Value = resp
                .json()
                .await
                .map_err(|_| RefusMusicBrainz::CorpsIllisible)?;
            let v = reduire(entite, &data);
            enregistrees.borrow_mut().insert(cle, v.clone());
            Ok(v)
        }
    })
    .await;
    eprintln!("{} requêtes envoyées ; bilan {bilan:?}", envoyees.borrow());
    let fixture = json!({
        "methode": "Banc #4805, étape C : la passe artistes par le réseau rejouée sur la \
                    bibliothèque du banc de l'étape B élargie (EXTENSION). Recherche \
                    /artist?query= (limit 10), confirmations /release-group et /recording \
                    (arid:, limit 25). 1 requête / 1,1 s, User-Agent TuneServer, depuis Shrek. \
                    Réponses réduites aux champs lus.",
        "enregistre_le": "2026-10-05",
        "requetes": *envoyees.borrow(),
        "reponses": &*enregistrees.borrow(),
    });
    std::fs::write(
        chemin_fixture(),
        serde_json::to_string_pretty(&fixture).expect("JSON") + "\n",
    )
    .expect("écriture de la fixture");
}
