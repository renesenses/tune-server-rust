//! #7767 — `GET /library/artists/{id}/albums?sections=1` rend EXACTEMENT le
//! contrat de la route artiste (`coordination/seb-lot2/contrat-route-artiste.md`),
//! contre lequel le client web (tune-web-client#2788) est déjà écrit : les
//! deux exemples JSON du contrat (musicienne de jazz, chef d'orchestre) sont
//! recopiés tels quels dans `fixtures/contrat_route_artiste_7767.json` — la
//! même fixture que les tests vitest de la PR web — et chaque bibliothèque
//! est reconstruite dans une base SQLite en mémoire pour que le VRAI routeur
//! rende la réponse comparée champ par champ à l'exemple.
//!
//! Ce que les deux exemples prouvent (scénarios d'Aède réécrits) :
//! - un invité (contrebasse sur tout *Fixture Jazz Session*) n'est pas dans
//!   la discographie : il est dans « Apparaît sur » ;
//! - un crédit d'écriture seul (*Lena Park Sings*, *Nordic Songs*) n'est ni
//!   discographie ni apparition : « Écriture et production » (et « Reprises »
//!   pour la composition sans rôle audible) ;
//! - une apparition en compilation (*Fixture Sampler*) n'est ni discographie
//!   ni invité : « Compilations » ;
//! - un remixeur (*Okapi Reworks Brahms*) est AUDIBLE : « Apparaît sur » ;
//! - un arrangeur seul (*Nordic Songs*) va dans « Écriture et production »,
//!   PAS dans « Apparaît sur » ;
//! - `credit_counts` compte TOUT, discographie comprise (20 / 15, 25 / 4) ;
//! - `writing` est absente sans crédit d'écriture, `credit_counts` est
//!   toujours là, zéros compris ; sans `sections=1`, le tableau nu.
//!
//! Deux écarts entre l'exemple et ce que le schéma permet, dits ici plutôt
//! que cachés : un album a toujours une fiche d'artiste d'album (`albums.
//! artist_id`, pas de nom libre), *Okafor Plays Standards* porte donc la
//! fiche de Sam Okafor et non `null` ; et les groupes de `collaborations`
//! sont rangés par nom (règle de #4862, inchangée), l'exemple du chef les
//! donne dans l'ordre inverse. Le client web n'ordonne pas les groupes et ne
//! lit `artist_id` que pour le lien.
//!
//! Cible `[[test]]` propre (`autotests = false`), hors de `server_contracts`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use tune_server::state::AppState;

const CONTRAT: &str = include_str!("fixtures/contrat_route_artiste_7767.json");

async fn get(app: &axum::Router, path: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::get(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn exec(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

fn artiste(state: &AppState, id: i64, nom: &str) {
    exec(
        state,
        &format!("INSERT INTO artists (id, name) VALUES ({id}, '{nom}')"),
    );
}

/// Un album avec les colonnes que l'exemple donne : le JSON rendu doit lui
/// correspondre champ par champ.
#[allow(clippy::too_many_arguments)]
fn album(
    state: &AppState,
    id: i64,
    titre: &str,
    artiste: i64,
    annee: i32,
    genre: &str,
    pistes: i64,
    format: &str,
    cadence: i64,
    bits: Option<i64>,
    compilation: bool,
    secondaires: Option<&str>,
) {
    let bits = bits.map_or("NULL".to_string(), |b| b.to_string());
    let secondaires = secondaires.map_or("NULL".to_string(), |s| format!("'{s}'"));
    exec(
        state,
        &format!(
            "INSERT INTO albums (id, title, artist_id, year, genre, genres, disc_count, track_count, \
             cover_path, source, format, sample_rate, bit_depth, release_date, is_compilation, \
             release_type, release_secondary_types) \
             VALUES ({id}, '{titre}', {artiste}, {annee}, '{genre}', '[\"{genre}\"]', 1, {pistes}, \
             '/var/lib/tune/covers/{id}.jpg', 'local', '{format}', {cadence}, {bits}, '{annee}', \
             {}, 'album', {secondaires})",
            i32::from(compilation)
        ),
    );
}

/// Les pistes `album*10 + 1 ..= album*10 + n` (l'exemple numérote ainsi :
/// album 910 → pistes 9101 à 9107), toutes à l'artiste donné.
fn pistes(state: &AppState, album: i64, n: i64, artiste: Option<i64>) {
    for i in 1..=n {
        let id = album * 10 + i;
        let artiste = artiste.map_or("NULL".to_string(), |a| a.to_string());
        exec(
            state,
            &format!(
                "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, format, disc_number, track_number) \
                 VALUES ({id}, 'Piste {id}', {album}, {artiste}, '/m/{id}.flac', 200000, 'flac', 1, {i})"
            ),
        );
    }
}

fn credit(
    state: &AppState,
    piste: i64,
    artiste: Option<i64>,
    nom: &str,
    role: &str,
    instrument: Option<&str>,
) {
    let artiste = artiste.map_or("NULL".to_string(), |a| a.to_string());
    let instrument = instrument.map_or("NULL".to_string(), |i| format!("'{i}'"));
    exec(
        state,
        &format!(
            "INSERT INTO track_credits (track_id, artist_id, artist_name, role, instrument, position, source) \
             VALUES ({piste}, {artiste}, '{nom}', '{role}', {instrument}, 0, 'musicbrainz')"
        ),
    );
}

/// § 6.1 du contrat : Ilse Brandt (fiche 412), contrebassiste.
fn bibliotheque_jazz() -> AppState {
    let s = AppState::new(":memory:", 0, Default::default()).unwrap();
    artiste(&s, 412, "Ilse Brandt");
    artiste(&s, 3, "Various Artists");
    artiste(&s, 401, "Nora Vidal Quartet");
    artiste(&s, 402, "Nora Vidal");
    artiste(&s, 403, "Sam Okafor");
    artiste(&s, 520, "Lena Park");

    // Discographie : Low Tide, 8 pistes, elle en signe 4.
    album(
        &s,
        900,
        "Low Tide",
        412,
        2019,
        "Jazz",
        8,
        "flac",
        96000,
        Some(24),
        false,
        None,
    );
    pistes(&s, 900, 8, Some(412));
    for p in 9001..=9004 {
        credit(&s, p, Some(412), "Ilse Brandt", "composer", None);
    }
    // Compilation : une piste à son nom.
    album(
        &s,
        950,
        "Fixture Sampler Vol. 1",
        3,
        2021,
        "Jazz",
        16,
        "flac",
        44100,
        Some(16),
        true,
        None,
    );
    pistes(&s, 950, 16, None);
    exec(&s, "UPDATE tracks SET artist_id = 412 WHERE id = 9503");
    // Apparaît sur : contrebasse sur tout Fixture Jazz Session.
    album(
        &s,
        910,
        "Fixture Jazz Session",
        401,
        2018,
        "Jazz",
        7,
        "flac",
        44100,
        Some(16),
        false,
        None,
    );
    pistes(&s, 910, 7, Some(401));
    for p in 9101..=9107 {
        credit(&s, p, Some(412), "Ilse Brandt", "performer", Some("bass"));
        credit(&s, p, Some(402), "Nora Vidal", "performer", Some("piano"));
        credit(&s, p, None, "Sam Okafor", "performer", Some("saxophone"));
        credit(&s, p, None, "Tomas Reyes", "performer", Some("drums"));
    }
    // Second Session : contrebasse sur 3 pistes, productrice de tout.
    album(
        &s,
        911,
        "Second Session",
        401,
        2022,
        "Jazz",
        9,
        "flac",
        44100,
        Some(16),
        false,
        None,
    );
    pistes(&s, 911, 9, Some(401));
    for p in [9111, 9112, 9115] {
        credit(&s, p, Some(412), "Ilse Brandt", "performer", Some("bass"));
        // #7768 : Nora Vidal au piano à côté d'elle — 7 + 3 = 10 pistes
        // partagées, comme dans l'exemple § 6.1.
        credit(&s, p, Some(402), "Nora Vidal", "performer", Some("piano"));
    }
    for p in 9111..=9119 {
        credit(&s, p, Some(412), "Ilse Brandt", "producer", None);
        credit(&s, p, None, "Ruth Adler", "producer", None);
    }
    // Sam Okafor : contrebasse sur une piste.
    album(
        &s,
        920,
        "Okafor Plays Standards",
        403,
        2020,
        "Jazz",
        10,
        "mp3",
        44100,
        None,
        false,
        None,
    );
    pistes(&s, 920, 10, Some(403));
    credit(
        &s,
        9204,
        Some(412),
        "Ilse Brandt",
        "performer",
        Some("bass"),
    );
    // Lena Park Sings : deux compositions, elle n'y joue pas.
    album(
        &s,
        930,
        "Lena Park Sings",
        520,
        2023,
        "Jazz",
        11,
        "flac",
        44100,
        Some(16),
        false,
        None,
    );
    pistes(&s, 930, 11, Some(520));
    for p in [9302, 9309] {
        credit(&s, p, Some(412), "Ilse Brandt", "composer", None);
    }
    s
}

/// § 6.2 du contrat : Henrik Lund (fiche 433), chef d'orchestre.
fn bibliotheque_chef() -> AppState {
    let s = AppState::new(":memory:", 0, Default::default()).unwrap();
    artiste(&s, 433, "Henrik Lund");
    artiste(&s, 440, "Clara Moreau");
    artiste(&s, 441, "Orchestre Fixture de Lyon");
    artiste(&s, 450, "DJ Okapi");
    artiste(&s, 460, "Elise Fournier");

    album(
        &s,
        1001,
        "Sibelius: Symphonies 2 & 5",
        433,
        2017,
        "Classical",
        8,
        "flac",
        96000,
        Some(24),
        false,
        None,
    );
    pistes(&s, 1001, 8, Some(433));
    album(
        &s,
        1002,
        "Live at the Fixture Hall",
        433,
        2021,
        "Classical",
        6,
        "flac",
        44100,
        Some(16),
        false,
        Some("live"),
    );
    pistes(&s, 1002, 6, Some(433));
    // Apparitions : il est l'artiste de 3 pistes de Clara Moreau.
    album(
        &s,
        1010,
        "Fixture Concertos",
        440,
        2016,
        "Classical",
        6,
        "flac",
        44100,
        Some(16),
        false,
        None,
    );
    pistes(&s, 1010, 6, Some(440));
    exec(
        &s,
        "UPDATE tracks SET artist_id = 433 WHERE id IN (10101, 10102, 10103)",
    );
    for p in 10101..=10103 {
        credit(&s, p, Some(440), "Clara Moreau", "performer", Some("piano"));
    }
    // Il dirige tout le Requiem.
    album(
        &s,
        1020,
        "Brahms: Ein deutsches Requiem",
        441,
        2019,
        "Classical",
        7,
        "flac",
        44100,
        Some(16),
        false,
        None,
    );
    pistes(&s, 1020, 7, Some(441));
    for p in 10201..=10207 {
        credit(&s, p, Some(433), "Henrik Lund", "conductor", None);
    }
    // Remixeur d'une piste de DJ Okapi.
    album(
        &s,
        1030,
        "Okapi Reworks Brahms",
        450,
        2024,
        "Electronic",
        5,
        "aac",
        44100,
        None,
        false,
        None,
    );
    pistes(&s, 1030, 5, Some(450));
    credit(&s, 10303, Some(433), "Henrik Lund", "remixer", None);
    // Arrangeur de 4 pistes de Nordic Songs.
    album(
        &s,
        1040,
        "Nordic Songs",
        460,
        2022,
        "Classical",
        12,
        "flac",
        44100,
        Some(16),
        false,
        None,
    );
    pistes(&s, 1040, 12, Some(460));
    for p in 10401..=10404 {
        credit(&s, p, Some(433), "Henrik Lund", "arranger", None);
    }
    s
}

fn exemple(nom: &str) -> Value {
    let mut tout: Value = serde_json::from_str(CONTRAT).expect("fixture JSON");
    let mut ex = tout[nom].take();
    // #7768 (PR 5) : deux écarts de l'exemple § 6.1 avec la règle du contrat
    // § 3.3, dits ici plutôt que cachés. (1) « la fiche quand elle existe » :
    // Sam Okafor a une fiche (403, l'artiste d'album d'*Okafor Plays
    // Standards*), son `artist_id` est donc 403 et non `null`. (2) « ordre :
    // `shared_tracks` décroissant, puis nom sans casse » : à 10 pistes
    // chacun, « Nora Vidal » passe avant « Nora Vidal Quartet ». Le client
    // web n'ordonne pas la liste et ne lit `artist_id` que pour le lien.
    if let Some(liste) = ex.get_mut("played_with").and_then(Value::as_array_mut) {
        for j in liste.iter_mut() {
            if j["artist_name"] == "Sam Okafor" {
                j["artist_id"] = serde_json::json!(403);
            }
        }
        liste.sort_by(|a, b| {
            b["shared_tracks"]
                .as_i64()
                .cmp(&a["shared_tracks"].as_i64())
                .then_with(|| {
                    let n =
                        |v: &Value| v["artist_name"].as_str().unwrap_or_default().to_lowercase();
                    n(a).cmp(&n(b))
                })
        });
    }
    ex
}

fn cles(v: &Value) -> Vec<String> {
    let mut c: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    c.sort();
    c
}

/// Chaque champ de l'album de l'exemple est rendu à l'identique. `added_at`
/// vient de la date de première vue des fichiers et `community_listens` du
/// site : hors de ce que la base de banc reproduit.
fn comparer_album(attendu: &Value, rendu: &Value, ou: &str) {
    for (cle, valeur) in attendu.as_object().unwrap() {
        if cle == "added_at" || cle == "community_listens" {
            continue;
        }
        if cle == "artist_id" && valeur.is_null() {
            // Un album porte toujours une fiche d'artiste d'album (voir
            // l'en-tête) : l'exemple « sans fiche » rend la fiche.
            assert!(rendu["artist_id"].is_i64(), "{ou} : artist_id {rendu}");
            continue;
        }
        assert_eq!(
            &rendu[cle], valeur,
            "{ou} : champ `{cle}` ; attendu {valeur}, rendu {}",
            rendu[cle]
        );
    }
}

fn comparer_section(attendu: &Value, rendu: &Value, cle: &str) {
    let a = attendu[cle]
        .as_array()
        .unwrap_or_else(|| panic!("exemple : `{cle}` n'est pas un tableau"));
    let r = rendu[cle]
        .as_array()
        .unwrap_or_else(|| panic!("`{cle}` absente ou pas un tableau : {rendu}"));
    assert_eq!(
        r.len(),
        a.len(),
        "`{cle}` : {} albums attendus, {} rendus : {}",
        a.len(),
        r.len(),
        rendu[cle]
    );
    for (i, (att, ren)) in a.iter().zip(r).enumerate() {
        comparer_album(att, ren, &format!("{cle}[{i}]"));
    }
}

/// Les groupes de `collaborations`, comparés par nom (voir l'en-tête).
fn comparer_collaborations(attendu: &Value, rendu: &Value) {
    let par_nom = |v: &Value| -> Vec<(String, Value)> {
        let mut g: Vec<(String, Value)> = v["collaborations"]
            .as_array()
            .unwrap_or_else(|| panic!("`collaborations` absente : {v}"))
            .iter()
            .map(|g| {
                (
                    g["artist_name"].as_str().unwrap_or_default().to_string(),
                    g.clone(),
                )
            })
            .collect();
        g.sort_by(|a, b| a.0.cmp(&b.0));
        g
    };
    let a = par_nom(attendu);
    let r = par_nom(rendu);
    assert_eq!(
        r.iter().map(|g| g.0.as_str()).collect::<Vec<_>>(),
        a.iter().map(|g| g.0.as_str()).collect::<Vec<_>>(),
        "{}",
        rendu["collaborations"]
    );
    for ((nom, att), (_, ren)) in a.iter().zip(&r) {
        if !att["artist_id"].is_null() {
            assert_eq!(ren["artist_id"], att["artist_id"], "groupe {nom}");
        }
        comparer_section(att, ren, "albums");
    }
    // Et l'ordre rendu est celui des noms, sans casse (#4862).
    let noms: Vec<String> = rendu["collaborations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| g["artist_name"].as_str().unwrap_or_default().to_lowercase())
        .collect();
    let mut tries = noms.clone();
    tries.sort();
    assert_eq!(noms, tries);
}

fn comparer_reponse(attendu: &Value, rendu: &Value) {
    assert_eq!(cles(rendu), cles(attendu), "clés de la réponse : {rendu}");
    for cle in [
        "albums",
        "live",
        "compilations",
        "appearances",
        "covers",
        "writing",
    ] {
        if attendu.get(cle).is_some() {
            comparer_section(attendu, rendu, cle);
        }
    }
    if attendu.get("collaborations").is_some() {
        comparer_collaborations(attendu, rendu);
    }
    assert_eq!(
        rendu["credit_counts"], attendu["credit_counts"],
        "credit_counts : {}",
        rendu["credit_counts"]
    );
    assert_eq!(
        rendu["played_with"], attendu["played_with"],
        "played_with (#7768) : {}",
        rendu["played_with"]
    );
}

#[tokio::test]
async fn la_musicienne_de_jazz_rend_l_exemple_6_1_du_contrat_7767() {
    let app = tune_server::routes::router(bibliotheque_jazz());
    let (status, body) = get(&app, "/api/v1/library/artists/412/albums?sections=1").await;
    assert_eq!(status, StatusCode::OK);
    comparer_reponse(&exemple("jazz"), &body);

    // Les compteurs, en clair : interprète 5 albums (900, 910, 911, 920,
    // 950), 20 pistes (8 + 7 + 3 + 1 + 1) ; écriture 3 albums (900, 911,
    // 930), 15 pistes (4 + 9 + 2) — discographie comprise.
    assert_eq!(
        body["credit_counts"],
        serde_json::json!({
            "performing": { "albums": 5, "tracks": 20 },
            "writing": { "albums": 3, "tracks": 15 }
        })
    );
    // Second Session est à la fois dans « Apparaît sur » (contrebasse, focus
    // 3 pistes) et dans « Écriture et production » (productrice, focus 9).
    assert_eq!(body["writing"][0]["id"], 911);
    assert_eq!(
        body["writing"][0]["credit_roles"],
        serde_json::json!(["producer"])
    );
    assert_eq!(
        body["writing"][0]["focus_track_ids"]
            .as_array()
            .unwrap()
            .len(),
        9
    );
    // Lena Park Sings : dans « Reprises » ET dans « Écriture et production ».
    assert_eq!(body["covers"][0]["id"], 930);
    assert_eq!(body["writing"][1]["id"], 930);
}

#[tokio::test]
async fn le_chef_d_orchestre_rend_l_exemple_6_2_du_contrat_7767() {
    let app = tune_server::routes::router(bibliotheque_chef());
    let (status, body) = get(&app, "/api/v1/library/artists/433/albums?sections=1").await;
    assert_eq!(status, StatusCode::OK);
    comparer_reponse(&exemple("chef"), &body);

    assert_eq!(
        body["credit_counts"],
        serde_json::json!({
            "performing": { "albums": 5, "tracks": 25 },
            "writing": { "albums": 1, "tracks": 4 }
        })
    );
    // Le remixeur est AUDIBLE : Okapi Reworks Brahms est dans « Apparaît
    // sur », avec le rôle `remixer`, et nulle part ailleurs.
    let okapi = body["collaborations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["artist_name"] == "DJ Okapi")
        .unwrap_or_else(|| panic!("DJ Okapi absent de collaborations : {body}"));
    assert_eq!(
        okapi["albums"][0]["credit_roles"],
        serde_json::json!(["remixer"])
    );
    assert_eq!(
        okapi["albums"][0]["focus_track_ids"],
        serde_json::json!([10303])
    );
    // L'arrangeur seul : « Écriture et production », PAS « Apparaît sur »,
    // PAS « Reprises » (il n'est pas compositeur).
    assert_eq!(body["writing"][0]["id"], 1040);
    assert_eq!(
        body["writing"][0]["credit_roles"],
        serde_json::json!(["arranger"])
    );
    assert!(body.get("covers").is_none(), "{body}");
    let tout = serde_json::to_string(&body["collaborations"]).unwrap();
    assert!(!tout.contains("Nordic Songs"), "{tout}");
    // Le live est hors de « albums », dans « live ».
    assert_eq!(body["live"][0]["id"], 1002);
    assert_eq!(body["albums"].as_array().unwrap().len(), 1);
}

/// `writing` absente sans crédit d'écriture ; `credit_counts` TOUJOURS là,
/// zéros compris : c'est ce qui distingue « serveur ancien » de « aucun
/// crédit » pour le client. Et sans `sections=1`, le tableau nu, sans rien.
#[tokio::test]
async fn writing_absente_et_credit_counts_toujours_presents_7767() {
    let app = tune_server::routes::router(bibliotheque_chef());

    // Clara Moreau : artiste de 3 pistes, pianiste créditée sur 3 autres du
    // même disque → interprète 1 album, 6 pistes ; écriture 0.
    let (_, body) = get(&app, "/api/v1/library/artists/440/albums?sections=1").await;
    assert!(body.get("writing").is_none(), "{body}");
    assert_eq!(
        body["credit_counts"],
        serde_json::json!({
            "performing": { "albums": 1, "tracks": 6 },
            "writing": { "albums": 0, "tracks": 0 }
        }),
        "{body}"
    );

    // Elise Fournier : rien d'autre que son disque.
    let (_, body) = get(&app, "/api/v1/library/artists/460/albums?sections=1").await;
    assert!(
        body.get("writing").is_none() && body.get("covers").is_none(),
        "{body}"
    );
    assert_eq!(body["credit_counts"]["performing"]["tracks"], 12);
    assert_eq!(
        body["credit_counts"]["writing"],
        serde_json::json!({ "albums": 0, "tracks": 0 })
    );

    // Un artiste sans rien : les deux compteurs à zéro, présents.
    exec_sur(&app, 999).await;

    // Le tableau nu ne change pas : ni `credit_counts` ni `writing`.
    let (status, body) = get(&app, "/api/v1/library/artists/433/albums").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.is_array(), "{body}");
    assert_eq!(body.as_array().unwrap().len(), 2, "{body}");
}

async fn exec_sur(app: &axum::Router, id: i64) {
    let (status, body) = get(
        app,
        &format!("/api/v1/library/artists/{id}/albums?sections=1"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["albums"], serde_json::json!([]));
    assert_eq!(
        body["credit_counts"],
        serde_json::json!({
            "performing": { "albums": 0, "tracks": 0 },
            "writing": { "albums": 0, "tracks": 0 }
        }),
        "{body}"
    );
}

/// Un album masqué sort de tout : de `writing` comme des compteurs. Et une
/// ligne `balises` qui double la ligne MusicBrainz du même crédit (#7766) ne
/// compte qu'une fois : on compte des pistes, pas des lignes.
#[tokio::test]
async fn album_masque_exclu_et_doublon_de_source_compte_une_fois_7767() {
    let state = bibliotheque_chef();
    exec(
        &state,
        "INSERT INTO hidden_items (item_type, item_id) VALUES ('album', 1040)",
    );
    for p in 10201..=10207 {
        exec(
            &state,
            &format!(
                "INSERT INTO track_credits (track_id, artist_id, artist_name, role, instrument, position, source) \
                 VALUES ({p}, NULL, 'Henrik Lund', 'conductor', NULL, 0, 'balises')"
            ),
        );
    }
    let app = tune_server::routes::router(state);
    let (_, body) = get(&app, "/api/v1/library/artists/433/albums?sections=1").await;
    assert!(body.get("writing").is_none(), "{body}");
    assert_eq!(
        body["credit_counts"],
        serde_json::json!({
            "performing": { "albums": 5, "tracks": 25 },
            "writing": { "albums": 0, "tracks": 0 }
        }),
        "{body}"
    );
    let requiem = &body["collaborations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["artist_name"] == "Orchestre Fixture de Lyon")
        .unwrap()["albums"][0];
    assert_eq!(requiem["focus_track_ids"].as_array().unwrap().len(), 7);
    assert_eq!(requiem["credit_roles"], serde_json::json!(["conductor"]));
}

/// § 3.1 du contrat, le cas décisif de bout en bout : un disque d'autrui où
/// il est l'ARTISTE d'une piste (déjà dans « Apparitions ») et PRODUCTEUR
/// d'une autre reste hors de « Apparaît sur » (#4862, inchangé) mais entre
/// dans « Écriture et production » — y jouer n'efface pas l'avoir produit.
/// C'est la colonne `EXISTS` de `credits_hors_discographie` lue à VRAI.
#[tokio::test]
async fn artiste_d_une_piste_garde_le_disque_en_ecriture_et_production_7767() {
    let state = bibliotheque_chef();
    // Fixture Concertos (1010) : il signe 10101 à 10103, il produit 10104.
    credit(&state, 10104, Some(433), "Henrik Lund", "producer", None);
    let app = tune_server::routes::router(state);
    let (status, body) = get(&app, "/api/v1/library/artists/433/albums?sections=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["appearances"][0]["id"], 1010, "{body}");
    let collaborations = serde_json::to_string(&body["collaborations"]).unwrap();
    assert!(
        !collaborations.contains("Fixture Concertos"),
        "{collaborations}"
    );
    // Écriture : Fixture Concertos (2016) avant Nordic Songs (2022).
    let writing = body["writing"]
        .as_array()
        .unwrap_or_else(|| panic!("{body}"));
    assert_eq!(
        writing
            .iter()
            .map(|a| a["id"].as_i64().unwrap())
            .collect::<Vec<_>>(),
        [1010, 1040]
    );
    assert_eq!(writing[0]["focus_track_ids"], serde_json::json!([10104]));
    assert_eq!(writing[0]["credit_roles"], serde_json::json!(["producer"]));
    assert_eq!(
        body["credit_counts"],
        serde_json::json!({
            "performing": { "albums": 5, "tracks": 25 },
            "writing": { "albums": 2, "tracks": 5 }
        }),
        "{body}"
    );
}
