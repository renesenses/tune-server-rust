//! Témoins de #5593 — le périmètre des analyses de fond.
//!
//! Deux étages. Les clauses d'abord, sur leur texte et sur une vraie base :
//! préfixe borné par le séparateur, les deux séparateurs, l'apostrophe d'un
//! nom de dossier, la casse du genre. Puis les PASSES elles-mêmes : une piste
//! d'une racine exclue, posée sur un vrai fichier décodable, n'est ni
//! sélectionnée — donc jamais décodée — ni comptée, et la même piste redevient
//! du travail dès qu'on lève l'exclusion (contre-épreuve dans le même test).
use std::sync::Arc;

use super::*;
use crate::audio::embedding_store;
use crate::audio::replaygain;
use crate::db::backend::DbBackend;
use crate::db::sqlite::SqliteDb;
use crate::db::track_metadata_repo::TrackMetadataRepo;

fn base() -> (SqliteDb, Arc<dyn DbBackend>) {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    db.execute("INSERT INTO artists (id, name) VALUES (1, 'Tades')", &[])
        .unwrap();
    db.execute(
        "INSERT INTO albums (id, title, artist_id) VALUES (1, 'Disque', 1)",
        &[],
    )
    .unwrap();
    let backend: Arc<dyn DbBackend> = Arc::new(db.clone());
    (db, backend)
}

fn piste(db: &SqliteDb, id: i64, chemin: &str, genre: Option<&str>, genres: Option<&str>) {
    db.execute(
        "INSERT INTO tracks (id, title, album_id, artist_id, file_path, duration_ms, \
         sample_rate, channels, genre, genres) VALUES (?, 'T', 1, 1, ?, 1000, 44100, 2, ?, ?)",
        &[&id, &chemin, &genre, &genres],
    )
    .unwrap();
}

fn regler(backend: &Arc<dyn DbBackend>, cle: &str, valeur: serde_json::Value) {
    SettingsRepo::with_backend(backend.clone())
        .set(cle, &valeur.to_string())
        .unwrap();
}

fn ids(backend: &Arc<dyn DbBackend>, clause: &str) -> Vec<i64> {
    backend
        .query_many(
            &format!("SELECT t.id FROM tracks t WHERE 1 = 1{clause} ORDER BY t.id"),
            &[],
        )
        .unwrap()
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_i64()))
        .collect()
}

// ── Les clauses ─────────────────────────────────────────────────────────

#[test]
fn listes_vides_aucune_clause() {
    assert_eq!(clause_hors_racines("t.file_path", &[]), "");
    assert_eq!(clause_genres(&[]), "");
    let (_db, backend) = base();
    assert_eq!(clause_decodage(&backend), "", "rien de réglé : rien d'ajouté");
    assert_eq!(clause_clap(&backend), "");
}

/// Exclure `/music` n'exclut ni `/music2` ni `/musicale` : le préfixe porte le
/// séparateur. La racine elle-même, avec ou sans séparateur final, et les deux
/// séparateurs d'une base Windows sont exclus.
#[test]
fn la_racine_est_un_prefixe_borne_par_le_separateur() {
    let (db, backend) = base();
    piste(&db, 1, "/music/a.flac", None, None);
    piste(&db, 2, "/music2/b.flac", None, None);
    piste(&db, 3, "/musicale/c.flac", None, None);
    piste(&db, 4, "/music/sous/d.flac", None, None);
    piste(&db, 5, r"Z:\NAS\e.flac", None, None);
    piste(&db, 6, "Z:/NAS/f.flac", None, None);
    piste(&db, 7, r"Z:\NASbis\g.flac", None, None);
    let c = clause_hors_racines("t.file_path", &["/music/".into(), r"Z:\NAS".into()]);
    assert_eq!(ids(&backend, &c), vec![2, 3, 7], "{c}");
}

/// `/` comme racine : tout chemin absolu.
#[test]
fn la_racine_slash_exclut_tout_chemin_absolu() {
    let (db, backend) = base();
    piste(&db, 1, "/a.flac", None, None);
    piste(&db, 2, "relatif/b.flac", None, None);
    let c = clause_hors_racines("t.file_path", &["/".into()]);
    assert_eq!(ids(&backend, &c), vec![2], "{c}");
}

/// Une apostrophe dans un nom de dossier ne casse pas la requête, et un `%` ou
/// un `_` n'y est pas un joker.
#[test]
fn apostrophe_et_jokers_sont_des_caracteres_ordinaires() {
    let (db, backend) = base();
    piste(&db, 1, "/mnt/l'été/a.flac", None, None);
    piste(&db, 2, "/mnt/100%_x/b.flac", None, None);
    piste(&db, 3, "/mnt/100ab_x/c.flac", None, None);
    let c = clause_hors_racines("t.file_path", &["/mnt/l'été".into(), "/mnt/100%_x".into()]);
    assert_eq!(ids(&backend, &c), vec![3], "{c}");
}

/// Le genre se cherche dans la colonne `t.genre` OU dans le tableau
/// `t.genres`, sans la casse. Une piste sans genre n'est pas retenue.
#[test]
fn le_genre_se_cherche_dans_la_colonne_et_dans_le_tableau() {
    let (db, backend) = base();
    piste(&db, 1, "/m/1.flac", Some("Jazz"), None);
    piste(&db, 2, "/m/2.flac", Some("Rock"), Some(r#"["Rock","POP"]"#));
    piste(&db, 3, "/m/3.flac", Some("Rock"), Some(r#"["Rock"]"#));
    piste(&db, 4, "/m/4.flac", None, None);
    piste(&db, 5, "/m/5.flac", Some("Acid Jazz"), None);
    piste(&db, 6, "/m/6.flac", Some("Rock'n'Roll"), None);
    let c = clause_genres(&["jazz".into(), "Pop".into(), "Rock'n'Roll".into()]);
    assert_eq!(ids(&backend, &c), vec![1, 2, 6], "{c}");
}

#[test]
fn normaliser_rogne_dedoublonne_et_refuse_le_reste() {
    use serde_json::json;
    assert_eq!(
        normaliser(&json!([" a ", "", "a", "b"])).unwrap(),
        vec!["a".to_string(), "b".to_string()]
    );
    assert_eq!(normaliser(&json!(null)).unwrap(), Vec::<String>::new());
    assert!(normaliser(&json!("a")).is_err());
    assert!(normaliser(&json!([1])).is_err());
    let trop: Vec<String> = (0..=ENTREES_MAX).map(|i| i.to_string()).collect();
    assert!(normaliser(&json!(trop)).is_err());
}

/// Une valeur illisible en base se relit « rien d'exclu », jamais une panne.
#[test]
fn une_valeur_illisible_en_base_vaut_rien_d_exclu() {
    let (_db, backend) = base();
    SettingsRepo::with_backend(backend.clone())
        .set(CLE_RACINES_EXCLUES, "pas du json")
        .unwrap();
    assert!(racines_exclues(&backend).is_empty());
    assert_eq!(clause_decodage(&backend), "");
}

// ── Les compteurs ───────────────────────────────────────────────────────

/// Les compteurs des quatre passes ne comptent plus une piste exclue — et la
/// comptent de nouveau dès que l'exclusion est levée.
#[test]
fn les_compteurs_excluent_la_racine_reglee() {
    let (db, backend) = base();
    SettingsRepo::with_backend(backend.clone())
        .set(replaygain::MODE_KEY, "track")
        .unwrap();
    piste(&db, 1, "/local/a.flac", Some("Jazz"), None);
    piste(&db, 2, "/nas/musique/b.flac", Some("Jazz"), None);
    piste(&db, 3, "/nas/musique/c.flac", Some("Rock"), None);
    let meta = TrackMetadataRepo::new(db.clone());
    // Le témoin que le rattrapage des empreintes et de la plage dynamique
    // exigent quand le ReplayGain est armé : posé AVANT l'exclusion, il ne
    // doit pas suffire à garder la piste dans leur périmètre.
    for id in [1, 2, 3] {
        meta.set(id, "rg_analyzed", "1").unwrap();
    }
    piste(&db, 4, "/local/d.flac", Some("Rock"), None);
    piste(&db, 5, "/nas/musique/e.flac", Some("Jazz"), None);

    let avant = (
        replaygain::compter_les_candidats_replaygain(&backend),
        replaygain::compter_les_candidats_dr(&backend),
        replaygain::compter_les_candidats_a_empreinter(&backend),
        embedding_store::eligible_count(&backend),
    );
    assert_eq!(avant, (2, 3, Some(3), 5), "sans périmètre");

    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!(["/nas/musique"]));
    assert_eq!(replaygain::compter_les_candidats_replaygain(&backend), 1);
    assert_eq!(replaygain::compter_les_candidats_dr(&backend), 1);
    assert_eq!(replaygain::compter_les_candidats_a_empreinter(&backend), Some(1));
    assert_eq!(embedding_store::eligible_count(&backend), 2);

    // Genres du CLAP par-dessus : Jazz seul, sur ce qui reste (1 et 4).
    regler(&backend, CLE_GENRES_CLAP, serde_json::json!(["jazz"]));
    assert_eq!(embedding_store::eligible_count(&backend), 1);
    // Le genre ne touche QUE le CLAP.
    assert_eq!(replaygain::compter_les_candidats_replaygain(&backend), 1);

    // Contre-épreuve : on lève tout, les chiffres d'avant reviennent.
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!([]));
    regler(&backend, CLE_GENRES_CLAP, serde_json::json!([]));
    let apres = (
        replaygain::compter_les_candidats_replaygain(&backend),
        replaygain::compter_les_candidats_dr(&backend),
        replaygain::compter_les_candidats_a_empreinter(&backend),
        embedding_store::eligible_count(&backend),
    );
    assert_eq!(apres, avant);
}

/// Une piste d'une racine exclue dont le disque ne répond pas n'est plus
/// « en attente d'un disque » : elle n'est plus du travail du tout.
#[test]
fn une_reportee_d_une_racine_exclue_n_attend_plus_son_disque() {
    let (db, backend) = base();
    piste(&db, 1, "/local/a.flac", None, None);
    piste(&db, 2, "/nas/b.flac", None, None);
    let meta = TrackMetadataRepo::new(db.clone());
    // Un report très frais : au-dessus de tout seuil.
    meta.set(1, "rg_path_unresolved", "99999999999").unwrap();
    meta.set(2, "rg_path_unresolved", "99999999999").unwrap();
    assert_eq!(replaygain::compter_les_reportees_par_chemin(&backend), 2);
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!(["/nas"]));
    assert_eq!(replaygain::compter_les_reportees_par_chemin(&backend), 1);
}

/// Le numérateur de la jauge CLAP suit le dénominateur : une piste traitée
/// AVANT son exclusion sort des deux à la fois.
#[test]
fn le_numerateur_clap_suit_le_perimetre() {
    let (db, backend) = base();
    piste(&db, 1, "/local/a.flac", None, None);
    piste(&db, 2, "/nas/b.flac", None, None);
    let meta = TrackMetadataRepo::new(db.clone());
    meta.set(1, "audio_embed_analyzed", embedding_store::MODEL_ID)
        .unwrap();
    meta.set(2, "audio_embed_analyzed", embedding_store::MODEL_ID)
        .unwrap();
    assert_eq!(embedding_store::processed_count_in_scope(&backend), 2);
    assert_eq!(embedding_store::eligible_count(&backend), 2);
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!(["/nas"]));
    assert_eq!(embedding_store::eligible_count(&backend), 1);
    assert_eq!(
        embedding_store::processed_count_in_scope(&backend),
        1,
        "la piste du NAS sort du numérateur comme du dénominateur"
    );
    // Le compteur global, lui, ne bouge pas : la recherche par ambiance garde
    // les empreintes déjà calculées.
    assert_eq!(embedding_store::processed_count(&backend), 2);
}

// ── Les passes : jamais décodée ─────────────────────────────────────────

fn fixture() -> &'static str {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/ape/sine_16s_c3000.wav"
    )
}

fn racine_de_la_fixture() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures").to_string()
}

/// 🔴 Le cœur de la demande : une piste d'une racine exclue, sur un fichier
/// parfaitement décodable, n'est PAS décodée par la passe ReplayGain — elle
/// ne reçoit ni témoin, ni gain, ni empreinte. Contre-épreuve dans le même
/// test : l'exclusion levée, la même passe la décode.
#[tokio::test]
async fn la_passe_replaygain_ne_decode_jamais_une_racine_exclue() {
    let (db, backend) = base();
    SettingsRepo::with_backend(backend.clone())
        .set(replaygain::MODE_KEY, "track")
        .unwrap();
    piste(&db, 42, fixture(), None, None);
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!([racine_de_la_fixture()]));

    assert_eq!(replaygain::analyze_track_batch(&backend).await, 0);
    let m = TrackMetadataRepo::new(db.clone()).get_all(42).unwrap();
    assert!(
        !m.contains_key("rg_analyzed") && !m.contains_key("rg_track_gain"),
        "exclue : jamais décodée, aucun témoin : {m:?}"
    );
    // Les rattrapages ne la prennent pas non plus (ReplayGain armé : ils
    // exigent son témoin, absent — on vérifie qu'ils restent muets).
    assert_eq!(replaygain::empreinter_un_lot(&backend).await, 0);
    assert_eq!(replaygain::rattraper_un_lot_de_dr(&backend).await, 0);

    // Contre-épreuve.
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!([]));
    assert_eq!(replaygain::analyze_track_batch(&backend).await, 1);
    assert!(
        TrackMetadataRepo::new(db.clone())
            .get_all(42)
            .unwrap()
            .contains_key("rg_analyzed")
    );
}

/// Même chose pour les deux rattrapages, ReplayGain COUPÉ (#5246 : ils
/// travaillent alors sans témoin) — c'est le cas où ils décoderaient seuls.
#[tokio::test]
async fn les_rattrapages_ne_decodent_jamais_une_racine_exclue() {
    let (db, backend) = base();
    piste(&db, 42, fixture(), None, None);
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!([racine_de_la_fixture()]));
    assert!(!replaygain::analysis_enabled(&backend), "ReplayGain coupé");

    assert_eq!(replaygain::empreinter_un_lot(&backend).await, 0);
    assert_eq!(replaygain::rattraper_un_lot_de_dr(&backend).await, 0);
    let fp: Option<String> = db
        .query_one("SELECT audio_fingerprint FROM tracks WHERE id = 42", &[])
        .unwrap()
        .and_then(|r| r.first().and_then(|v| v.as_string()));
    assert_eq!(fp, None, "exclue : aucune empreinte");
    let m = TrackMetadataRepo::new(db.clone()).get_all(42).unwrap();
    assert!(
        !m.contains_key("dr_track") && !m.contains_key("dr_indisponible"),
        "exclue : aucune mesure ni marque de plage : {m:?}"
    );

    // Contre-épreuve : levée, l'empreinte est posée et la plage tentée.
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!([]));
    assert_eq!(replaygain::empreinter_un_lot(&backend).await, 1);
    assert_eq!(replaygain::rattraper_un_lot_de_dr(&backend).await, 1);
}

/// La sélection du CLAP — la seule entrée de sa passe — ne rend ni une piste
/// d'une racine exclue, ni une piste hors des genres retenus.
#[test]
fn la_selection_clap_respecte_racines_et_genres() {
    let (db, backend) = base();
    piste(&db, 1, "/local/a.flac", Some("Jazz"), None);
    piste(&db, 2, "/local/b.flac", Some("Rock"), None);
    piste(&db, 3, "/nas/c.flac", Some("Jazz"), None);
    let rendus = |backend: &Arc<dyn DbBackend>| -> Vec<i64> {
        let mut v: Vec<i64> = embedding_store::candidats_acoustiques(backend, "00000000000", 25)
            .unwrap()
            .into_iter()
            .filter_map(|c| match c {
                embedding_store::CandidatAcoustique::Pret { track_id, .. } => Some(track_id),
                _ => None,
            })
            .collect();
        v.sort();
        v
    };
    assert_eq!(rendus(&backend), vec![1, 2, 3]);
    regler(&backend, CLE_RACINES_EXCLUES, serde_json::json!(["/nas"]));
    regler(&backend, CLE_GENRES_CLAP, serde_json::json!(["Jazz"]));
    assert_eq!(rendus(&backend), vec![1]);
}
