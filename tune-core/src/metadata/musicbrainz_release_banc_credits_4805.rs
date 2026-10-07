//! Banc : la couverture des crédits des albums identifiés (#4805, étape E).
//!
//! Idée : MetaRust (`credits.rs`), de Xavier Joly — les crédits vivent à
//! plusieurs niveaux de la réponse MusicBrainz, et une passe qui n'en lit
//! qu'un les perd sans bruit.
//!
//! Mêmes 40 albums et mêmes réponses de recherche que le banc #4805
//! ([`super::banc_4805`]). Pour chacun des albums identifiés, le pressage
//! retenu (le premier candidat, celui que garde `identifier_album`) est relu
//! dans une seconde fixture, `banc_credits_4805.json` : la réponse
//! `/release/{id}?inc=` [`super::INC_RELEASE_COMPLET`], celle que
//! l'identification garde en base depuis #5867. La passe des crédits la relit
//! donc sans requête.
//!
//! Mesure du 06/10/2026 (453 pistes, 36 albums, 0 requête) : 5 592 crédits
//! rangés sur les 5 772 que portent les réponses (96,9 %) avec la table des
//! rôles d'avant ; tous les perdus sont des relations d'ENREGISTREMENT de
//! types que `credits_mb::role_canonique` ignorait (`balance`, `editor`,
//! `chorus master`). Les relations d'œuvre (compositeur, parolier…) et celles
//! de la release étaient déjà toutes rangées. Après : 5 772 / 5 772.
//!
//! Pour réenregistrer la fixture (une requête par pressage, 1 / 1,1 s,
//! User-Agent Tune) :
//!
//! ```text
//! TUNE_BANC_4805E_ENREGISTRER=1 cargo test -p tune-core --lib \
//!     banc_credits_4805::enregistrer_les_releases -- --ignored --nocapture
//! ```

use std::collections::BTreeMap;

use serde_json::{Value, json};

use super::banc_4805::{ALBUMS, CANDIDATS, chemin_fixture, rejouer};
use super::{INC_RELEASE_COMPLET, LectureRelease, artiste_de_requete, recherche_de_pressages};

fn chemin_fixture_credits() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/musicbrainz/banc_credits_4805.json")
}

fn reponses_de_recherche() -> BTreeMap<String, Value> {
    let brut = std::fs::read_to_string(chemin_fixture()).expect("fixture du banc #4805");
    let fixture: Value = serde_json::from_str(&brut).expect("fixture JSON");
    serde_json::from_value(fixture["reponses"].clone()).expect("reponses")
}

/// Le pressage retenu pour chaque album identifié : `(index dans ALBUMS,
/// release_id)`.
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

// ── Réduction de la réponse ─────────────────────────────────────────────────

fn copier(src: &Value, cles: &[&str]) -> serde_json::Map<String, Value> {
    let mut o = serde_json::Map::new();
    for k in cles {
        if let Some(v) = src.get(*k) {
            o.insert((*k).to_string(), v.clone());
        }
    }
    o
}

fn reduire_artiste(a: &Value) -> Value {
    Value::Object(copier(a, &["id", "name"]))
}

fn reduire_credit(v: &Value) -> Option<Value> {
    let ac = v.get("artist-credit")?.as_array()?;
    Some(Value::Array(
        ac.iter()
            .map(|c| {
                let mut o = copier(c, &["name", "joinphrase"]);
                if let Some(a) = c.get("artist") {
                    o.insert("artist".into(), reduire_artiste(a));
                }
                Value::Object(o)
            })
            .collect(),
    ))
}

/// Les relations d'un objet, réduites à ce qu'un crédit peut lire : type,
/// sens, attributs et l'artiste ; ou l'œuvre interprétée avec SES relations
/// d'artistes. Les relations d'œuvre à œuvre (arrangement, parties, autre
/// version…) ne portent aucun artiste — MusicBrainz n’imbrique pas les
/// relations de l’œuvre liée (vérifié sur les 1 376 du banc) : elles sont
/// retirées.
fn reduire_relations(v: &Value, dans_une_oeuvre: bool) -> Option<Value> {
    let rels = v.get("relations")?.as_array()?;
    let gardees: Vec<Value> = rels
        .iter()
        .filter(|r| match r.get("target-type").and_then(Value::as_str) {
            Some("artist") => true,
            Some("work") => !dans_une_oeuvre,
            _ => false,
        })
        .map(|r| {
            let mut o = copier(r, &["type", "target-type", "direction", "attributes"]);
            if let Some(a) = r.get("artist") {
                o.insert("artist".into(), reduire_artiste(a));
            }
            if let Some(w) = r.get("work") {
                let mut ow = copier(w, &["id", "title"]);
                if let Some(rr) = reduire_relations(w, true) {
                    ow.insert("relations".into(), rr);
                }
                o.insert("work".into(), Value::Object(ow));
            }
            Value::Object(o)
        })
        .collect();
    Some(Value::Array(gardees))
}

fn reduire_release(data: &Value) -> Value {
    let mut o = copier(data, &["id", "title"]);
    if let Some(ac) = reduire_credit(data) {
        o.insert("artist-credit".into(), ac);
    }
    if let Some(r) = reduire_relations(data, false) {
        o.insert("relations".into(), r);
    }
    let media: Vec<Value> = data
        .get("media")
        .and_then(Value::as_array)
        .map(|ms| {
            ms.iter()
                .map(|m| {
                    let pistes: Vec<Value> = m
                        .get("tracks")
                        .and_then(Value::as_array)
                        .map(|ts| {
                            ts.iter()
                                .map(|t| {
                                    let mut p = copier(t, &["position", "number", "title"]);
                                    if let Some(ac) = reduire_credit(t) {
                                        p.insert("artist-credit".into(), ac);
                                    }
                                    if let Some(r) = t.get("recording") {
                                        let mut or = copier(r, &["id", "title"]);
                                        if let Some(ac) = reduire_credit(r) {
                                            or.insert("artist-credit".into(), ac);
                                        }
                                        if let Some(rr) = reduire_relations(r, false) {
                                            or.insert("relations".into(), rr);
                                        }
                                        p.insert("recording".into(), Value::Object(or));
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

/// Enregistre les releases du banc auprès de MusicBrainz — À LA MAIN
/// seulement, `TUNE_BANC_4805E_ENREGISTRER=1`. Une requête / 1,1 s, la
/// lecture même de la passe ([`super::lire_release_brute`], User-Agent Tune) ;
/// un refus est retenté deux fois après 3 s.
#[tokio::test]
#[ignore = "réseau : réenregistre la fixture du banc des crédits #4805"]
async fn enregistrer_les_releases() {
    if std::env::var("TUNE_BANC_4805E_ENREGISTRER").as_deref() != Ok("1") {
        eprintln!("TUNE_BANC_4805E_ENREGISTRER=1 absent : rien n'est enregistré");
        return;
    }
    let retenus = pressages_retenus(&reponses_de_recherche()).await;
    let mut releases: BTreeMap<String, Value> = BTreeMap::new();
    for (_, id) in &retenus {
        if releases.contains_key(id) {
            continue;
        }
        let mut lue = None;
        for _ in 0..3 {
            tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
            match super::lire_release_brute(id, INC_RELEASE_COMPLET).await {
                LectureRelease::Lue(v) => {
                    lue = Some(reduire_release(&v));
                    break;
                }
                LectureRelease::Inconnue => break,
                LectureRelease::Panne(m) => {
                    eprintln!("panne sur {id} : {m}");
                    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                }
            }
        }
        let v = lue.unwrap_or_else(|| panic!("release {id} non lue"));
        eprintln!("enregistré : {id}");
        releases.insert(id.clone(), v);
    }
    let methode = format!(
        "Banc #4805, étape E : pour chaque album identifié par la chaîne d'après \
         (fixture banc_identification_4805.json), le pressage retenu lu par \
         /release/{{id}}?inc={INC_RELEASE_COMPLET}, la requête que l'identification \
         garde en base (#5867). 1 requête / 1,1 s, User-Agent TuneServer. Réponses \
         réduites aux champs des crédits : artist-credit, relations d'artiste à tous \
         les niveaux, œuvres interprétées et leurs relations d'artiste."
    );
    // Une release par ligne, JSON compact : 1,3 Mo au lieu de 3, et un
    // réenregistrement se relit release par release.
    let mut texte = format!(
        "{{\n\"enregistre_le\": {},\n\"methode\": {},\n\"releases\": {{\n",
        json!(chrono::Utc::now().format("%Y-%m-%d").to_string()),
        json!(methode)
    );
    let n = releases.len();
    for (i, (id, v)) in releases.iter().enumerate() {
        texte.push_str(&format!(
            "{}: {}{}\n",
            json!(id),
            serde_json::to_string(v).expect("JSON"),
            if i + 1 < n { "," } else { "" }
        ));
    }
    texte.push_str("}}\n");
    std::fs::write(chemin_fixture_credits(), texte).expect("écriture de la fixture");
}

// ── La mesure ───────────────────────────────────────────────────────────────

/// Le CONTRAT du banc : chaque type de relation d'artiste MusicBrainz qu'un
/// crédit doit porter, et le rôle attendu dans `track_credits`. Il est écrit
/// ici, à part du code mesuré, pour que la mesure ne se juge pas elle-même :
/// un type que la passe oublie compte comme PERDU.
const CONTRAT: &[(&str, &str)] = &[
    // Interprètes, instruments, chant, chef, orchestre.
    ("instrument", "performer"),
    ("performer", "performer"),
    ("performing orchestra", "performer"),
    ("vocal", "vocal"),
    ("conductor", "conductor"),
    ("chorus master", "conductor"),
    // Production.
    ("producer", "producer"),
    ("engineer", "engineer"),
    ("recording", "engineer"),
    ("audio", "engineer"),
    ("sound", "engineer"),
    ("balance", "engineer"),
    ("editor", "engineer"),
    ("mastering", "mastering"),
    ("mix", "mixer"),
    ("mix-DJ", "mixer"),
    ("remixer", "remixer"),
    ("programming", "programming"),
    // Auteurs.
    ("composer", "composer"),
    ("lyricist", "writer"),
    ("writer", "writer"),
    ("librettist", "writer"),
    ("arranger", "arranger"),
    ("instrument arranger", "arranger"),
    ("vocal arranger", "arranger"),
    ("orchestrator", "arranger"),
];

/// Types d'artiste qui ne sont PAS des crédits musicaux : hors dénominateur,
/// mais comptés. Tout type ni au contrat ni ici est imprimé « non classé ».
const HORS_CREDITS: &[&str] = &[
    "misc",
    "dedication",
    "publishing",
    "translator",
    "instrument technician",
    "art direction",
    "design",
    "design/illustration",
    "graphic design",
    "liner notes",
    "photography",
    "copyright",
    "phonographic copyright",
    "legal representation",
    "booking",
];

/// Un crédit attendu sur une piste : `(nom normalisé, rôle, instrument)`.
type Unite = (String, &'static str, Option<String>);

/// Les crédits qu'une piste de la réponse PORTE, par niveau, selon le
/// contrat. `release` vaut pour toutes ses pistes.
fn unites_de_la_piste(
    piste: &Value,
    de_la_release: &[Value],
    hors: &mut BTreeMap<String, usize>,
) -> Vec<(&'static str, Unite)> {
    use super::super::credits_mb::est_qualificatif;
    use super::super::instruments::{canoniser_instrument, normaliser};
    let mut out: Vec<(&'static str, Unite)> = Vec::new();
    let mut ajouter = |niveau: &'static str, rel: &Value, out: &mut Vec<(&'static str, Unite)>| {
        let Some(t) = rel.get("type").and_then(Value::as_str) else {
            return;
        };
        let Some(nom) = rel
            .get("artist")
            .and_then(|a| a.get("name"))
            .and_then(Value::as_str)
        else {
            return;
        };
        let Some(&(_, role)) = CONTRAT.iter().find(|(ty, _)| *ty == t) else {
            let cle = if HORS_CREDITS.contains(&t) {
                format!("hors crédits : {t}")
            } else {
                format!("NON CLASSÉ : {t}")
            };
            *hors.entry(cle).or_default() += 1;
            return;
        };
        let instruments: Vec<String> = rel
            .get("attributes")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !est_qualificatif(s))
                    .map(canoniser_instrument)
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let nom = normaliser(nom);
        if instruments.is_empty() || !matches!(role, "performer" | "vocal") {
            out.push((niveau, (nom, role, None)));
        } else {
            for i in instruments {
                out.push((niveau, (nom.clone(), role, Some(i))));
            }
        }
    };
    let rec = &piste["recording"];
    if let Some(ac) = rec.get("artist-credit").and_then(Value::as_array) {
        for c in ac {
            if let Some(n) = c
                .get("name")
                .or_else(|| c.get("artist").and_then(|a| a.get("name")))
                .and_then(Value::as_str)
            {
                out.push(("artist-credit", (normaliser(n), "artist", None)));
            }
        }
    }
    for rel in rec["relations"].as_array().into_iter().flatten() {
        if rel.get("target-type").and_then(Value::as_str) == Some("artist") {
            ajouter("enregistrement", rel, &mut out);
        }
        if let Some(w) = rel.get("work") {
            for wr in w["relations"].as_array().into_iter().flatten() {
                ajouter("œuvre", wr, &mut out);
            }
        }
    }
    for rel in de_la_release {
        ajouter("release", rel, &mut out);
    }
    // Une unité portée à deux niveaux compte une fois, au premier.
    let mut vues = std::collections::HashSet::new();
    out.retain(|(_, u)| vues.insert(u.clone()));
    out
}

/// 🔴 Le banc de l'étape E. Une bibliothèque synthétique : un album par
/// pressage retenu, ses pistes calquées sur le pressage avec le
/// `musicbrainz_recording_id` que l'identification y pose, la réponse gardée
/// en base comme après #5867. Puis la VRAIE passe des crédits, et la mesure :
/// la part des crédits que les réponses portent et que `track_credits` range.
#[tokio::test(start_paused = true)]
async fn banc_credits_4805_couverture() {
    use std::sync::Arc;

    use crate::db::backend::{DbBackend, ToSqlValue};
    use crate::metadata::instruments::normaliser;

    let brut = std::fs::read_to_string(chemin_fixture_credits()).expect("fixture des crédits");
    let fixture: Value = serde_json::from_str(&brut).expect("fixture JSON");
    let releases: BTreeMap<String, Value> =
        serde_json::from_value(fixture["releases"].clone()).expect("releases");
    let retenus = pressages_retenus(&reponses_de_recherche()).await;

    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db);

    // (track_id, album_id, piste de la réponse, relations de la release)
    let mut pistes_locales: Vec<(i64, i64, Value, Vec<Value>)> = Vec::new();
    let mut track_id = 1000i64;
    for (i, rid) in &retenus {
        let (_, titre, _, _, n) = ALBUMS[*i];
        let album_id = *i as i64 + 1;
        let release = releases
            .get(rid)
            .unwrap_or_else(|| panic!("release {rid} non enregistrée : réenregistrer"));
        backend
            .execute(
                "INSERT INTO albums (id, title, musicbrainz_release_id) VALUES (?, ?, ?)",
                &[
                    &album_id as &dyn ToSqlValue,
                    &titre as &dyn ToSqlValue,
                    &rid.as_str() as &dyn ToSqlValue,
                ],
            )
            .unwrap();
        super::super::musicbrainz_release_cache::ecrire(
            &backend,
            rid,
            INC_RELEASE_COMPLET,
            release,
            chrono::Utc::now(),
        );
        let de_la_release: Vec<Value> = release["relations"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|r| r.get("target-type").and_then(Value::as_str) == Some("artist"))
            .collect();
        let mut prises = 0u32;
        for (i_disque, m) in release["media"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let disque = m["position"].as_i64().unwrap_or(i_disque as i64 + 1);
            for (i_piste, t) in m["tracks"].as_array().into_iter().flatten().enumerate() {
                if prises == n {
                    break;
                }
                prises += 1;
                track_id += 1;
                let numero = t["position"].as_i64().unwrap_or(i_piste as i64 + 1);
                let titre_piste = t["title"].as_str().unwrap_or_default();
                let rec = t["recording"]["id"].as_str().unwrap_or_default();
                backend
                    .execute(
                        "INSERT INTO tracks (id, title, album_id, disc_number, track_number, \
                         musicbrainz_recording_id) VALUES (?, ?, ?, ?, ?, ?)",
                        &[
                            &track_id as &dyn ToSqlValue,
                            &titre_piste as &dyn ToSqlValue,
                            &album_id as &dyn ToSqlValue,
                            &disque as &dyn ToSqlValue,
                            &numero as &dyn ToSqlValue,
                            &rec as &dyn ToSqlValue,
                        ],
                    )
                    .unwrap();
                pistes_locales.push((track_id, album_id, t.clone(), de_la_release.clone()));
            }
        }
    }

    // La VRAIE passe : tout est en base, aucune requête ne doit partir.
    let requetes = std::cell::Cell::new(0usize);
    let av = super::super::credits_release::remplir_credits_par(
        backend.clone(),
        "banc-credits",
        &|_| {},
        |_id, _inc| {
            requetes.set(requetes.get() + 1);
            std::future::ready(LectureRelease::Panne("hors banc".into()))
        },
    )
    .await;

    // Ce que la base range, par piste.
    let ranges: Vec<(i64, String, String, Option<String>)> = backend
        .query_many(
            "SELECT track_id, artist_name, role, instrument FROM track_credits",
            &[],
        )
        .unwrap()
        .into_iter()
        .map(|r| {
            (
                r[0].as_i64().unwrap(),
                normaliser(&r[1].as_string().unwrap_or_default()),
                r[2].as_string().unwrap_or_default(),
                r[3].as_string(),
            )
        })
        .collect();
    let est_range = |tid: i64, u: &Unite| {
        ranges
            .iter()
            .any(|(t, n, r, i)| *t == tid && *n == u.0 && r == u.1 && (u.2.is_none() || *i == u.2))
    };

    // Comptes : par niveau, par rôle, et par famille demandée par l'étape E.
    let mut par_niveau: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut par_role: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut hors: BTreeMap<String, usize> = BTreeMap::new();
    let mut perdus: BTreeMap<String, usize> = BTreeMap::new();
    let (mut dispo, mut range) = (0usize, 0usize);
    let mut albums_credites = std::collections::BTreeSet::new();
    let (mut pistes_avec, mut pistes_creditees) = (0usize, 0usize);
    for (tid, album_id, piste, de_la_release) in &pistes_locales {
        let unites = unites_de_la_piste(piste, de_la_release, &mut hors);
        if !unites.is_empty() {
            pistes_avec += 1;
        }
        if ranges.iter().any(|(t, ..)| t == tid) {
            pistes_creditees += 1;
            albums_credites.insert(*album_id);
        }
        for (niveau, u) in &unites {
            let ok = est_range(*tid, u);
            dispo += 1;
            range += ok as usize;
            let e = par_niveau.entry(niveau).or_default();
            e.0 += 1;
            e.1 += ok as usize;
            let role = if u.2.is_some() {
                "instrument (sur le crédit)"
            } else {
                u.1
            };
            let e = par_role.entry(role).or_default();
            e.0 += 1;
            e.1 += ok as usize;
            if !ok {
                *perdus.entry(format!("{niveau} / {}", u.1)).or_default() += 1;
            }
        }
    }
    let pc = |a: usize, b: usize| {
        if b == 0 {
            0.0
        } else {
            100.0 * a as f64 / b as f64
        }
    };
    println!(
        "\nalbums identifiés : {} ; crédités : {} ; pistes : {} (porteuses de crédits : {}, \
         créditées : {}) ; requêtes : {}",
        retenus.len(),
        albums_credites.len(),
        pistes_locales.len(),
        pistes_avec,
        pistes_creditees,
        requetes.get()
    );
    println!("\n| niveau | portés | rangés | % |\n|---|---:|---:|---:|");
    for (k, (d, r)) in &par_niveau {
        println!("| {k} | {d} | {r} | {:.1} |", pc(*r, *d));
    }
    println!(
        "| **total** | **{dispo}** | **{range}** | **{:.1}** |",
        pc(range, dispo)
    );
    println!("\n| rôle | portés | rangés | % |\n|---|---:|---:|---:|");
    for (k, (d, r)) in &par_role {
        println!("| {k} | {d} | {r} | {:.1} |", pc(*r, *d));
    }
    println!("\nperdus (niveau / rôle attendu) : {perdus:?}");
    println!("hors contrat (relations) : {hors:?}");

    assert_eq!(requetes.get(), 0, "la passe a refait des requêtes");
    assert_eq!(av.errors, 0);
    assert!(
        !hors.keys().any(|k| k.starts_with("NON CLASSÉ")),
        "type de relation inconnu du contrat : {hors:?}"
    );
    assert_eq!(
        range, dispo,
        "crédits portés par les réponses mais absents de la base : {perdus:?}"
    );
}
