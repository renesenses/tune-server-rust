//! #2108 — l'opérateur « Et » entre les valeurs d'une même facette.
//!
//! Bertrand, fil 1963 : « Réfléchir à la possibilité d'opérateurs logiques
//! "Et" et "Ou" (utile quand on cherche un compositeur ou un interprète en
//! musique classique) ». Le « Ou » de #2168 reste le défaut ; `?all_of=<facette>`
//! fait combiner les valeurs cochées de cette facette en ET — la piste doit
//! les porter TOUTES. La liste (`TrackRepo::list_filtered`) et le rail
//! (`build_facet_conditions`) passent par le même `facet_filter::combiner`.

use super::*;
use tune_core::db::backend::ToSqlValue;
use tune_core::db::track_repo::TrackRepo;

fn depuis(raw: &str) -> FacetQuery {
    match FacetQuery::default().hydrate(Some(raw)) {
        Ok(q) => q,
        Err(_) => panic!("requête acceptable : {raw}"),
    }
}

fn texte(v: &SqlValue) -> String {
    match v {
        SqlValue::Text(s) => s.clone(),
        SqlValue::Int(i) => i.to_string(),
        autre => format!("{autre:?}"),
    }
}

/// En « Et », un prédicat PAR valeur, joints par `AND`, marqueurs dans
/// l'ordre ; sans `all_of`, le SQL de #2168, mot pour mot.
#[test]
fn le_compositeur_en_et_donne_un_like_par_valeur() {
    let (conds, params) = build_facet_conditions(
        &depuis("composer=Bach&composer=Vivaldi&all_of=composer"),
        Engine::Postgres,
        "",
        None,
    );
    assert_eq!(
        conds,
        vec!["(LOWER(t.composer) LIKE LOWER($1) AND LOWER(t.composer) LIKE LOWER($2))"]
    );
    assert_eq!(
        params.iter().map(texte).collect::<Vec<_>>(),
        vec!["%Bach%".to_string(), "%Vivaldi%".to_string()]
    );

    let (conds, _) = build_facet_conditions(
        &depuis("composer=Bach&composer=Vivaldi"),
        Engine::Postgres,
        "",
        None,
    );
    assert_eq!(
        conds,
        vec!["(LOWER(t.composer) LIKE LOWER($1) OR LOWER(t.composer) LIKE LOWER($2))"],
        "défaut inchangé : le « Ou » de #2168"
    );
}

/// Une facette de jointure en « Et » : un `EXISTS` par valeur. Un seul
/// `EXISTS (… = a AND … = b)` chercherait une ligne qui vaille les deux.
#[test]
fn une_facette_de_jointure_en_et_pose_un_exists_par_valeur() {
    let (conds, params) = build_facet_conditions(
        &depuis("playlist=Matin&playlist=Soir&all_of=playlist"),
        Engine::Postgres,
        "",
        None,
    );
    assert_eq!(conds.len(), 1, "{conds:?}");
    assert_eq!(conds[0].matches("EXISTS (").count(), 2, "{}", conds[0]);
    assert!(conds[0].contains(") AND EXISTS ("), "{}", conds[0]);
    assert!(conds[0].contains("LOWER($1)") && conds[0].contains("LOWER($2)"));
    assert_eq!(
        params.iter().map(texte).collect::<Vec<_>>(),
        vec!["Matin".to_string(), "Soir".to_string()]
    );
}

/// Une facette à UNE valeur par piste (l'année) ignore `all_of` : deux années
/// en ET ne rendraient jamais rien.
#[test]
fn une_facette_monovaluee_ignore_le_et() {
    let q = depuis("year=1971&year=1972&all_of=year&all_of=inconnue");
    assert!(q.sel.en_et.is_empty(), "{:?}", q.sel.en_et);
    let (conds, _) = build_facet_conditions(&q, Engine::Postgres, "", None);
    assert_eq!(conds, vec!["t.year IN ($1, $2)"]);
}

/// De bout en bout, sur une vraie base : la liste ET le rail.
#[tokio::test]
async fn les_instruments_en_et_ne_gardent_que_la_piste_qui_les_porte_tous() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    let b = &state.backend;
    let piste = |titre: &str, annee: i64| {
        b.execute(
            "INSERT INTO tracks (title, file_path, year) VALUES (?1, ?2, ?3)",
            &[
                &titre as &dyn ToSqlValue,
                &format!("/m/{titre}.flac") as &dyn ToSqlValue,
                &annee as &dyn ToSqlValue,
            ],
        )
        .unwrap();
        b.last_insert_rowid()
    };
    let credit = |track: i64, instrument: &str| {
        b.execute(
            "INSERT INTO track_credits (track_id, artist_name, role, instrument, position) VALUES (?1, ?2, 'performer', ?3, 0)",
            &[&track.to_string() as &dyn ToSqlValue, &"Musicien" as &dyn ToSqlValue, &instrument as &dyn ToSqlValue],
        )
        .unwrap();
    };
    // A : violon seul (1998) ; B : violon ET violoncelle (2004) ; C :
    // violoncelle seul (2010).
    let a = piste("A", 1998);
    let bb = piste("B", 2004);
    let c = piste("C", 2010);
    credit(a, "violon");
    credit(bb, "violon");
    credit(bb, "violoncelle");
    credit(c, "violoncelle");

    let titres = |raw: &str| -> Vec<String> {
        let Ok(f) = track_filter_from_raw(Some(raw)) else {
            panic!("requête acceptable : {raw}");
        };
        let (pistes, _) = TrackRepo::with_backend(state.backend.clone())
            .list_filtered(&f, 50, 0)
            .unwrap();
        let mut t: Vec<String> = pistes.into_iter().map(|p| p.title).collect();
        t.sort();
        t
    };
    assert_eq!(
        titres("instrument=violon&instrument=violoncelle"),
        ["A", "B", "C"],
        "« Ou » : l'un OU l'autre"
    );
    assert_eq!(
        titres("instrument=violon&instrument=violoncelle&all_of=instrument"),
        ["B"],
        "« Et » : les deux à la fois"
    );

    // Le rail compte ce que la liste rend.
    async fn annees(state: &AppState, raw: &str) -> Vec<String> {
        let Json(v) = library_facets(
            crate::routes::active_profile::ActiveProfile(1),
            Query(FacetQuery {
                fields: Some("year".into()),
                ..Default::default()
            }),
            RawQuery(Some(raw.to_string())),
            State(state.clone()),
        )
        .await
        .ok()
        .expect("la route répond");
        let mut out: Vec<String> = v["year"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["value"].as_str().unwrap_or_default().to_string())
            .collect();
        out.sort();
        out
    }
    assert_eq!(
        annees(
            &state,
            "fields=year&instrument=violon&instrument=violoncelle"
        )
        .await,
        ["1998", "2004", "2010"]
    );
    assert_eq!(
        annees(
            &state,
            "fields=year&instrument=violon&instrument=violoncelle&all_of=instrument"
        )
        .await,
        ["2004"]
    );
}
