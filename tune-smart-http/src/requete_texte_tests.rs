//! #6287 — témoins du langage de requête : la grammaire, la traduction vers
//! les règles de l'éditeur, et le moteur sur une vraie base SQLite.

use super::*;
use crate::criteres::tests::bibliotheque;
use crate::smart_refs::EmptyResolver;
use tune_core::db::backend::DbBackend;
use tune_core::db::sqlite::SqliteDb;

fn regle_seule(texte: &str) -> Value {
    match analyser(texte).unwrap_or_else(|e| panic!("{texte} : {e}")) {
        Noeud::Regle(r) => r,
        autre => panic!("{texte} : une seule règle attendue, lu {autre:?}"),
    }
}

/// La bibliothèque partagée des critères (#5547), plus des crédits par
/// INSTRUMENT, qui sont l'objet de la demande :
///
/// | piste | crédits |
/// |---|---|
/// | 1 `Blue in Green` | Teo Macero (producer) ; Bill Evans (performer, piano) |
/// | 2 `So What` | Bill Evans (performer, piano) ; Paul Chambers (performer, double bass) |
/// | 3 `Giant Steps` | Tommy Flanagan (performer, piano) ; Paul Chambers (performer, double bass) |
/// | 4 `Solo` | aucun |
///
/// Écoutes : piste 1 (3, dernière 2024-06), piste 2 (1, 2019-03). Les
/// pistes 3 et 4 n'ont jamais été écoutées. Favoris : pistes 1 et 3.
fn base() -> SqliteDb {
    let db = bibliotheque();
    db.execute_batch(
        "INSERT INTO track_credits (track_id, artist_name, role, instrument) VALUES \
           (1,'Bill Evans','performer','piano'), \
           (2,'Bill Evans','performer','piano'), \
           (2,'Paul Chambers','performer','double bass'), \
           (3,'Tommy Flanagan','performer','piano'), \
           (3,'Paul Chambers','performer','double bass'); \
         UPDATE tracks SET duration_ms = 337000 WHERE id = 1; \
         UPDATE tracks SET duration_ms = 562000 WHERE id = 2; \
         UPDATE tracks SET duration_ms = 286000 WHERE id = 3; \
         UPDATE tracks SET duration_ms = 120000 WHERE id = 4;",
    )
    .unwrap();
    // Favoris de piste du profil 1 : la table que lit la règle « favori »
    // (`smart_refs::favorites_sub`).
    db.execute_batch(
        "INSERT INTO favorites (profile_id, item_type, item_id) VALUES (1,'track',1),(1,'track',3);",
    )
    .unwrap();
    db
}

/// Les pistes que retient une requête, par le moteur réel.
fn pistes(db: &SqliteDb, texte: &str) -> Vec<i64> {
    let arbre = analyser(texte).unwrap_or_else(|e| panic!("{texte} : {e}"));
    let ctx = RefCtx::root(&EmptyResolver, Some(1));
    let c = compiler(&arbre, &ctx);
    assert!(c.refusees.is_empty(), "{texte} : refusées {:?}", c.refusees);
    let sql = format!(
        "SELECT t.id FROM tracks t \
         LEFT JOIN albums al ON t.album_id = al.id \
         LEFT JOIN artists ar ON t.artist_id = ar.id \
         WHERE {} ORDER BY t.id",
        c.condition
    );
    db.query_many(&sql, &[])
        .unwrap_or_else(|e| panic!("{texte} : {e}\n{sql}"))
        .iter()
        .map(|r| r[0].as_i64().unwrap())
        .collect()
}

// ── La demande ─────────────────────────────────────────────────────────────

#[test]
fn les_pistes_ou_un_musicien_joue_d_un_instrument_6287() {
    let db = base();
    assert_eq!(
        pistes(&db, r#"credit(artiste: "Bill Evans", instrument: piano)"#),
        vec![1, 2]
    );
    // Le MÊME crédit porte l'artiste et l'instrument : Paul Chambers joue
    // sur la piste 3, et quelqu'un y joue du piano, mais pas lui.
    assert_eq!(
        pistes(
            &db,
            r#"credit(artiste: "Paul Chambers", instrument: piano)"#
        ),
        Vec::<i64>::new()
    );
    // Deux crédits distincts, chacun sur sa ligne : les deux musiciens.
    assert_eq!(
        pistes(
            &db,
            r#"credit(artiste: "Paul Chambers") credit(instrument: piano, artiste: flanagan)"#
        ),
        vec![3]
    );
}

#[test]
fn les_favoris_jamais_ecoutes_6287() {
    let db = base();
    assert_eq!(pistes(&db, "favori"), vec![1, 3]);
    assert_eq!(pistes(&db, "jamais_ecoute"), vec![3, 4]);
    assert_eq!(pistes(&db, "favori jamais_ecoute"), vec![3]);
    assert_eq!(pistes(&db, "favori et jamais-écouté"), vec![3]);
}

#[test]
fn les_plages_de_valeurs_6287() {
    let db = base();
    assert_eq!(pistes(&db, "annee:1959..1960"), vec![1, 2, 3]);
    assert_eq!(
        pistes(&db, "année:1960..1959"),
        vec![1, 2, 3],
        "bornes dans le désordre : remises dans l'ordre"
    );
    assert_eq!(pistes(&db, "annee:1960..2001"), vec![3, 4]);
    assert_eq!(pistes(&db, "duree>5min"), vec![1, 2]);
    assert_eq!(pistes(&db, "duree:4:00..6:00"), vec![1, 3]);
    assert_eq!(pistes(&db, "ecoutes>=1"), vec![1, 2]);
    assert_eq!(pistes(&db, "ecoutes<=1"), vec![2, 3, 4]);
    assert_eq!(
        pistes(&db, "ecoutes<1"),
        vec![3, 4],
        "jamais écoutées : zéro écoute"
    );
    assert_eq!(pistes(&db, "ecoutes<0"), Vec::<i64>::new());
    assert_eq!(pistes(&db, "derniere_ecoute<2020-01-01"), vec![2]);
    assert_eq!(pistes(&db, "derniere_ecoute:jamais"), vec![3, 4]);
}

#[test]
fn les_combinaisons_booleennes_6287() {
    let db = base();
    // ET lie plus fort que OU : (Rock ET 2001) OU producteur.
    assert_eq!(
        pistes(&db, "genre:rock annee=2001 ou credit(role: producer)"),
        vec![1, 4]
    );
    assert_eq!(
        pistes(&db, "(genre:rock ou credit(role: producer)) annee=2001"),
        vec![4]
    );
    assert_eq!(pistes(&db, "titre:blue | titre:solo"), vec![1, 4]);
    assert_eq!(
        pistes(&db, "genre:jazz & non credit(instrument: piano)"),
        Vec::<i64>::new()
    );
}

/// `non a` est le complément EXACT de `a`, valeur absente comprise.
#[test]
fn la_negation_est_le_complement_exact_6287() {
    let db = base();
    db.execute_batch("UPDATE tracks SET genre = NULL WHERE id = 3;")
        .unwrap();
    for a in [
        "genre:rock",
        "genre=Jazz",
        "credit(instrument: piano)",
        "annee>1959",
        "favori",
        "label:col",
        "duree<5min",
    ] {
        let oui = pistes(&db, a);
        for non in [
            format!("non {a}"),
            format!("!{a}"),
            format!("-{a}"),
            format!("not ({a})"),
        ] {
            let complement = pistes(&db, &non);
            let mut union: Vec<i64> = oui.iter().chain(&complement).copied().collect();
            union.sort();
            assert_eq!(
                union,
                vec![1, 2, 3, 4],
                "{a} / {non} : {oui:?} + {complement:?}"
            );
            assert!(
                oui.iter().all(|i| !complement.contains(i)),
                "{a} / {non} se recouvrent : {oui:?} / {complement:?}"
            );
        }
    }
}

// ── La traduction vers l'éditeur ───────────────────────────────────────────

#[test]
fn chaque_critere_devient_la_regle_de_l_editeur_6287() {
    for (texte, attendu) in [
        (
            "genre:jazz",
            json!({"field": "genre", "op": "contains", "value": "jazz"}),
        ),
        (
            "genre=Jazz",
            json!({"field": "genre", "op": "=", "value": "Jazz"}),
        ),
        (
            "genre!=vide",
            json!({"field": "genre", "op": "is_not_null", "value": null}),
        ),
        (
            "compositeur:vide",
            json!({"field": "composer", "op": "is_null", "value": null}),
        ),
        (
            "annee:1959",
            json!({"field": "year", "op": "=", "value": 1959}),
        ),
        (
            "annee>=1959",
            json!({"field": "year", "op": ">=", "value": 1959}),
        ),
        (
            "annee:1950..1959",
            json!({"field": "year", "op": "between", "value": [1950, 1959]}),
        ),
        (
            "duree>90s",
            json!({"field": "duration", "op": ">", "value": 90000}),
        ),
        (
            "ecoutes<=2",
            json!({"field": "play_count", "op": "between", "value": [0, 2]}),
        ),
        (
            "ecoutes<2",
            json!({"field": "play_count", "op": "between", "value": [0, 1]}),
        ),
        (
            "ajout:30j",
            json!({"field": "added_at", "op": "in_last", "value": 30}),
        ),
        (
            "derniere_ecoute:jamais",
            json!({"field": "last_played_at", "op": "is_null", "value": null}),
        ),
        (
            "derniere_ecoute:2024-01-01..2024-12-31",
            json!({"field": "last_played_at", "op": "between", "value": ["2024-01-01", "2024-12-31"]}),
        ),
        (
            "source:Qobuz",
            json!({"field": "source", "op": "=", "value": "qobuz"}),
        ),
        (
            "dossier=/m/jazz",
            json!({"field": "folder", "op": "starts_with", "value": "/m/jazz"}),
        ),
        (
            "etiquette:7",
            json!({"field": "tag", "op": "is", "value": "7"}),
        ),
        (
            "favori:album",
            json!({"field": "favorite", "op": "is", "value": "album"}),
        ),
        (
            "favori",
            json!({"field": "favorite", "op": "is", "value": "track"}),
        ),
        (
            "ia:non",
            json!({"field": "ai_generated", "op": "is_false", "value": null}),
        ),
        (
            r#"credit(role: producer, artiste: "Teo Macero")"#,
            json!({"field": "credit", "op": "has", "value": {"role": "producer", "artist_name": "Teo Macero"}}),
        ),
        // Les noms de règle se lisent tels quels.
        (
            "track_artist:Evans",
            json!({"field": "track_artist", "op": "contains", "value": "Evans"}),
        ),
    ] {
        assert_eq!(regle_seule(texte), attendu, "{texte}");
    }
}

#[test]
fn une_requete_plate_a_sa_forme_d_editeur_6287() {
    let (r, mode) = regles_editeur(&analyser("genre:jazz annee>1959").unwrap()).unwrap();
    assert_eq!(mode, "all");
    assert_eq!(r.len(), 2);
    let (r, mode) = regles_editeur(&analyser("genre:jazz ou genre:rock").unwrap()).unwrap();
    assert_eq!(mode, "any");
    assert_eq!(r.len(), 2);
    // Imbriquée ou niée : l'éditeur n'a qu'un niveau.
    assert!(regles_editeur(&analyser("(genre:a ou genre:b) annee:1").unwrap()).is_none());
    assert!(regles_editeur(&analyser("non genre:a").unwrap()).is_none());
}

/// Les règles rendues sont celles que l'éditeur enregistre : le moteur des
/// playlists, nourri de `rules` + `match_mode`, rend les MÊMES pistes que la
/// requête.
#[test]
fn la_forme_d_editeur_rend_les_memes_pistes_6287() {
    let db = base();
    let ctx = RefCtx::root(&EmptyResolver, Some(1));
    for texte in [
        "favori jamais_ecoute",
        "genre:jazz ou genre:rock",
        r#"credit(artiste: "Bill Evans", instrument: piano) annee=1959"#,
    ] {
        let (regles, mode) = regles_editeur(&analyser(texte).unwrap()).unwrap();
        let (w, _o, _l, refus) = build_smart_query_rapport(
            &Value::Array(regles).to_string(),
            mode,
            "title",
            "asc",
            None,
            &ctx,
        );
        assert!(refus.is_empty(), "{refus:?}");
        let ids: Vec<i64> = db
            .query_many(
                &format!(
                    "SELECT t.id FROM tracks t LEFT JOIN albums al ON t.album_id = al.id \
                     LEFT JOIN artists ar ON t.artist_id = ar.id {w} ORDER BY t.id"
                ),
                &[],
            )
            .unwrap()
            .iter()
            .map(|r| r[0].as_i64().unwrap())
            .collect();
        assert_eq!(ids, pistes(&db, texte), "{texte}");
    }
}

// ── Les erreurs disent où ──────────────────────────────────────────────────

#[test]
fn une_requete_fautive_dit_quoi_et_ou_6287() {
    for (texte, position, morceau) in [
        ("", 0, "vide"),
        ("   ", 0, "vide"),
        ("genre", 0, "champ:valeur"),
        ("couleur:bleu", 0, "champ inconnu"),
        ("genre:jazz (annee:1959", 11, "jamais fermée"),
        ("genre:", 6, "valeur attendue"),
        ("annee:mille", 6, "nombre attendu"),
        ("genre>jazz", 6, "texte"),
        ("credit(couleur: bleu)", 7, "artiste, role ou instrument"),
        ("credit()", 7, "vide"),
        (r#"titre:"sans fin"#, 6, "guillemet"),
        ("genre:jazz )", 11, "inattendu"),
        ("etiquette:favoris", 10, "identifiant"),
        ("\"Bill Evans\"", 0, "titre:"),
        ("ecoutes!=3", 9, "différent"),
    ] {
        let e = analyser(texte).expect_err(texte);
        assert_eq!(e.position, position, "{texte} : {e}");
        assert!(
            e.message.contains(morceau),
            "{texte} : « {} » sans « {morceau} »",
            e.message
        );
    }
}

/// Une valeur porte ce qu'elle veut — apostrophes, guillemets échappés,
/// opérateurs : c'est le moteur des règles qui l'échappe, jamais ce module.
#[test]
fn une_valeur_ne_sort_jamais_de_sa_regle_6287() {
    let db = base();
    assert_eq!(
        regle_seule(r#"titre:"l'été \"indien\" ou genre:rock""#),
        json!({"field": "title", "op": "contains", "value": "l'été \"indien\" ou genre:rock"})
    );
    assert_eq!(pistes(&db, r#"titre:"x' OR 1=1 --""#), Vec::<i64>::new());
    assert_eq!(pistes(&db, r#"genre:"jazz-rock""#), Vec::<i64>::new());
    assert_eq!(
        pistes(&db, "genre:jazz-rock"),
        Vec::<i64>::new(),
        "le tiret interne reste dans le mot"
    );
}

// ── Relecture rc7 : bornes, négation d'un refus, dates, débordements ───────

/// Une requête trop longue ou trop imbriquée est refusée à l'analyse : la
/// descente récursive (analyse puis compilation) ne doit pas dépendre de ce
/// qu'envoie le client.
#[test]
fn une_requete_est_bornee_en_longueur_et_en_profondeur_6287() {
    let longue = format!("titre:\"{}\"", "a".repeat(LONGUEUR_MAX));
    let e = analyser(&longue).expect_err("requête trop longue acceptée");
    assert!(e.message.contains("trop longue"), "{e}");
    let juste = format!("titre:{}", "a".repeat(LONGUEUR_MAX - 6));
    assert!(
        analyser(&juste).is_ok(),
        "{LONGUEUR_MAX} caractères passent"
    );

    let parentheses = |n: usize| format!("{}genre:jazz{}", "(".repeat(n), ")".repeat(n));
    assert!(analyser(&parentheses(PROFONDEUR_MAX)).is_ok());
    let e = analyser(&parentheses(PROFONDEUR_MAX + 1)).expect_err("imbrication sans borne");
    assert!(e.message.contains("imbriquées"), "{e}");

    let negations = |n: usize| format!("{}genre:jazz", "non ".repeat(n));
    assert!(analyser(&negations(PROFONDEUR_MAX)).is_ok());
    let e = analyser(&negations(PROFONDEUR_MAX + 1)).expect_err("négations sans borne");
    assert!(e.message.contains("imbriquées"), "{e}");
    // 1 000 parenthèses : une erreur, pas un débordement de pile.
    assert!(analyser(&parentheses(1000)).is_err());
}

/// Sous NON, un critère que le moteur refuse (FAUX) deviendrait VRAI : la
/// compilation le signale à part, pour que la route refuse la requête.
#[test]
fn un_refus_sous_une_negation_est_signale_a_part_6287() {
    let ctx = RefCtx::root(&EmptyResolver, Some(1));
    let c = compiler(&analyser("non ajout:foo..bar").unwrap(), &ctx);
    assert!(!c.refusees.is_empty(), "le moteur devait refuser la plage");
    assert_eq!(c.refusees_sous_non, c.refusees);
    let c = compiler(&analyser("genre:jazz ajout:foo..bar").unwrap(), &ctx);
    assert!(!c.refusees.is_empty());
    assert!(c.refusees_sous_non.is_empty(), "sans NON, FAUX reste FAUX");
}

/// Le moteur ne connaît que `>` et `<` sur les dates : `>=` et `<=` sont
/// refusés plutôt que rabattus sans le dire.
#[test]
fn une_date_refuse_les_comparaisons_larges_6287() {
    for texte in ["ajout>=2024-01-01", "derniere_ecoute<=2024-01-01"] {
        let e = analyser(texte).expect_err(texte);
        assert!(e.message.contains("strictes"), "{texte} : {e}");
    }
    assert_eq!(
        regle_seule("ajout>2024-01-01"),
        json!({"field": "added_at", "op": ">", "value": "2024-01-01"})
    );
}

/// Une durée démesurée est refusée, sans débordement arithmétique.
#[test]
fn une_duree_demesuree_est_refusee_6287() {
    assert_eq!(duree_ms("9223372036854775807h"), None);
    assert_eq!(duree_ms("9223372036854775807:00"), None);
    assert_eq!(duree_ms("4:30"), Some(270_000));
    assert!(analyser("duree>9223372036854775807h").is_err());
}
