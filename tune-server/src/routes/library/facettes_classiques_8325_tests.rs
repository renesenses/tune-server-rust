//! #8325 — chef d'orchestre, ensemble / orchestre et œuvre : trois facettes de
//! la Collection, leurs filtres et leurs colonnes, sur une vraie base.
//!
//! Les valeurs viennent des trois magasins ouverts : crédits
//! (`track_credits`, rôle `conductor`), balises du scan (`track_metadata`) et
//! fiche d'album (`album_metadata`). Le rail doit compter exactement ce que
//! la liste rend.

use super::*;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::track_repo::TrackRepo;

/// Quatre pistes, deux albums :
/// - A (album 1) : chef Karajan par un crédit MusicBrainz ;
/// - B (album 1) : chef Karajan par la balise CONDUCTOR, ensemble
///   « Rundfunkchor Berlin; Berliner Philharmoniker » (ENSEMBLE + ORCHESTRA) ;
/// - C et D (album 2) : chef Abbado et œuvre « Requiem » sur la fiche
///   d'album ; D porte aussi la balise WORK « Requiem » (même œuvre, une
///   seule fois comptée) et une ORCHESTRA en casse différente.
fn base() -> AppState {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let b = &state.backend;
    b.execute_batch("INSERT INTO albums (id, title) VALUES (1, 'Neuvième'), (2, 'Requiem');")
        .unwrap();
    let piste = |titre: &str, album: i64| {
        b.execute(
            "INSERT INTO tracks (title, file_path, album_id) VALUES (?1, ?2, ?3)",
            &[
                &titre as &dyn ToSqlValue,
                &format!("/m/{titre}.flac") as &dyn ToSqlValue,
                &album as &dyn ToSqlValue,
            ],
        )
        .unwrap();
        b.last_insert_rowid()
    };
    let a = piste("A", 1);
    let bb = piste("B", 1);
    let c = piste("C", 2);
    let d = piste("D", 2);
    b.execute(
        "INSERT INTO track_credits (track_id, artist_name, role, position, source) \
         VALUES (?1, 'Herbert von Karajan', 'conductor', 0, 'musicbrainz')",
        &[&a as &dyn ToSqlValue],
    )
    .unwrap();
    // Un soliste : rôle `performer`, jamais un chef ni un ensemble.
    b.execute(
        "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) \
         VALUES (?1, 'Anne-Sophie Mutter', 'performer', 'violin', 1)",
        &[&a as &dyn ToSqlValue],
    )
    .unwrap();
    // L'orchestre crédité par MusicBrainz (« performing orchestra ») :
    // `performer` sans instrument, reconnu à son nom. Un pianiste crédité de
    // la même façon (instrument non dit) n'est PAS un ensemble.
    for nom in ["Wiener Philharmoniker", "Maurizio Pollini"] {
        b.execute(
            "INSERT INTO track_credits (track_id, artist_name, role, position, source) \
             VALUES (?1, ?2, 'performer', 2, 'musicbrainz')",
            &[&c as &dyn ToSqlValue, &nom as &dyn ToSqlValue],
        )
        .unwrap();
    }
    let meta = |piste: i64, cle: &str, valeur: &str| {
        b.execute(
            "INSERT INTO track_metadata (track_id, key, value) VALUES (?1, ?2, ?3)",
            &[
                &piste as &dyn ToSqlValue,
                &cle as &dyn ToSqlValue,
                &valeur as &dyn ToSqlValue,
            ],
        )
        .unwrap();
    };
    meta(bb, "conductor", "Herbert von Karajan");
    meta(
        bb,
        "ensemble",
        "Rundfunkchor Berlin; Berliner Philharmoniker",
    );
    meta(d, "work", "Requiem");
    meta(d, "movement", "Dies irae");
    meta(d, "orchestra", "berliner philharmoniker");
    b.execute_batch(
        "INSERT INTO album_metadata (album_id, key, value) VALUES \
         (2, 'conductor', 'Claudio Abbado'), (2, 'work', 'Requiem');",
    )
    .unwrap();
    state
}

async fn rail(state: &AppState, raw: &str) -> Value {
    let Json(v) = library_facets(
        crate::routes::active_profile::ActiveProfile(1),
        Query(FacetQuery {
            fields: values_of_fields(raw),
            ..Default::default()
        }),
        RawQuery(Some(raw.to_string())),
        State(state.clone()),
    )
    .await
    .ok()
    .expect("la route répond");
    v
}

fn values_of_fields(raw: &str) -> Option<String> {
    super::super::query_multi::values_of(Some(raw), "fields")
        .into_iter()
        .next()
}

/// `[(valeur, effectif)]` d'une facette, dans l'ordre rendu.
fn effectifs(v: &Value, facette: &str) -> Vec<(String, i64)> {
    v[facette]
        .as_array()
        .unwrap_or_else(|| panic!("facette `{facette}` absente : {v}"))
        .iter()
        .map(|e| {
            (
                e["value"].as_str().unwrap_or_default().to_string(),
                e["count"].as_i64().unwrap_or(-1),
            )
        })
        .collect()
}

fn titres(state: &AppState, raw: &str) -> Vec<String> {
    let Ok(f) = track_filter_from_raw(Some(raw)) else {
        panic!("requête acceptable : {raw}");
    };
    let (pistes, _) = TrackRepo::with_backend(state.backend.clone())
        .list_filtered(&f, 50, 0)
        .unwrap();
    let mut t: Vec<String> = pistes.into_iter().map(|p| p.title).collect();
    t.sort();
    t
}

fn paire(v: &str, n: i64) -> (String, i64) {
    (v.to_string(), n)
}

#[tokio::test]
async fn le_rail_compte_chefs_ensembles_et_oeuvres_de_tous_les_magasins_8325() {
    let state = base();
    let v = rail(&state, "fields=conductor,ensemble,work").await;
    assert_eq!(
        effectifs(&v, "conductor"),
        vec![paire("Claudio Abbado", 2), paire("Herbert von Karajan", 2)],
        "#8325 — chefs : crédit (A), balise (B), fiche d'album (C, D)"
    );
    assert_eq!(
        effectifs(&v, "ensemble"),
        vec![
            paire("Berliner Philharmoniker", 2),
            paire("Rundfunkchor Berlin", 1),
            paire("Wiener Philharmoniker", 1)
        ],
        "#8325 — ensembles : une entrée par personne, casses réunies, \
         l'orchestre crédité par MusicBrainz compris, pas le pianiste"
    );
    assert_eq!(
        effectifs(&v, "work"),
        vec![paire("Requiem", 2)],
        "#8325 — œuvre : D la porte deux fois, comptée une fois"
    );
}

#[tokio::test]
async fn la_liste_filtre_par_chef_ensemble_et_oeuvre_8325() {
    let state = base();
    assert_eq!(
        titres(&state, "conductor=herbert%20von%20karajan"),
        ["A", "B"]
    );
    assert_eq!(titres(&state, "conductor=Claudio%20Abbado"), ["C", "D"]);
    assert_eq!(
        titres(&state, "ensemble=Berliner%20Philharmoniker"),
        ["B", "D"],
        "une entrée au milieu d'une liste « ; » est trouvée"
    );
    assert_eq!(titres(&state, "ensemble=Rundfunkchor%20Berlin"), ["B"]);
    assert_eq!(
        titres(&state, "ensemble=Wiener%20Philharmoniker"),
        ["C"],
        "l'orchestre d'un crédit MusicBrainz"
    );
    assert!(
        titres(&state, "ensemble=Maurizio%20Pollini").is_empty(),
        "un soliste sans instrument n'est pas un ensemble"
    );
    assert!(
        titres(&state, "ensemble=Berliner").is_empty(),
        "une entrée se compare entière, pas en sous-chaîne"
    );
    assert_eq!(titres(&state, "work=Requiem"), ["C", "D"]);
    assert_eq!(
        titres(
            &state,
            "ensemble=Rundfunkchor%20Berlin&ensemble=Berliner%20Philharmoniker&all_of=ensemble"
        ),
        ["B"],
        "« Et » : la piste porte les deux"
    );
    assert!(
        titres(&state, "conductor=Anne-Sophie%20Mutter").is_empty(),
        "un soliste n'est pas un chef"
    );
}

#[tokio::test]
async fn les_facettes_se_restreignent_entre_elles_8325() {
    let state = base();
    let v = rail(&state, "fields=conductor,work&work=Requiem").await;
    assert_eq!(
        effectifs(&v, "conductor"),
        vec![paire("Claudio Abbado", 2)],
        "le rail compte ce que la liste rend"
    );
    assert_eq!(
        effectifs(&v, "work"),
        vec![paire("Requiem", 2)],
        "une facette ne se filtre pas elle-même"
    );
    let v = rail(&state, "fields=work&conductor=Herbert%20von%20Karajan").await;
    assert!(effectifs(&v, "work").is_empty(), "{v}");
}

#[tokio::test]
async fn les_pistes_portent_les_colonnes_classiques_8325() {
    let state = base();
    let Ok(f) = track_filter_from_raw(Some(
        "conductor=Claudio%20Abbado&conductor=Herbert%20von%20Karajan",
    )) else {
        panic!("requête acceptable");
    };
    let (pistes, _) = TrackRepo::with_backend(state.backend.clone())
        .list_filtered(&f, 50, 0)
        .unwrap();
    let items = super::super::tracks::joindre_dr_par_piste(&state, 1, pistes);
    let de = |titre: &str| -> &Value {
        items
            .iter()
            .find(|i| i["title"] == titre)
            .unwrap_or_else(|| panic!("piste {titre} absente"))
    };
    assert_eq!(de("A")["conductor"], "Herbert von Karajan");
    assert!(de("A").get("ensemble").is_none(), "clé ABSENTE sans valeur");
    assert_eq!(
        de("B")["ensemble"],
        "Berliner Philharmoniker; Rundfunkchor Berlin"
    );
    assert_eq!(de("D")["conductor"], "Claudio Abbado");
    assert_eq!(de("D")["work"], "Requiem");
    assert_eq!(de("D")["movement"], "Dies irae");
    assert_eq!(de("C")["work"], "Requiem", "l'œuvre de la fiche d'album");
    assert!(de("C").get("movement").is_none());
}

/// Le SQL de la facette tient sur PostgreSQL : identifiants projetés en texte
/// (miroir copié depuis SQLite), marqueurs numérotés dans l'ordre.
#[test]
fn le_predicat_postgres_projette_les_identifiants_en_texte_8325() {
    let q = FacetQuery::default()
        .hydrate(Some("conductor=Karajan&work=Requiem"))
        .ok()
        .expect("requête acceptable");
    let (conds, params) = build_facet_conditions(&q, Engine::Postgres, "", None);
    assert_eq!(conds.len(), 2, "{conds:?}");
    assert!(
        conds[0].starts_with("CAST(t.id AS TEXT) IN (SELECT s.piste FROM ("),
        "{}",
        conds[0]
    );
    assert!(
        conds[0].contains("CAST(tc.track_id AS TEXT)"),
        "{}",
        conds[0]
    );
    assert!(conds[0].contains("LIKE LOWER($1)"), "{}", conds[0]);
    assert!(conds[1].contains("LIKE LOWER($2)"), "{}", conds[1]);
    assert!(
        !conds[1].contains("track_credits"),
        "l'œuvre n'est pas un crédit"
    );
    let liees: Vec<String> = params
        .iter()
        .map(|p| match p {
            SqlValue::Text(s) => s.clone(),
            autre => format!("{autre:?}"),
        })
        .collect();
    assert_eq!(liees, vec!["%;Karajan;%", "%;Requiem;%"]);
}
