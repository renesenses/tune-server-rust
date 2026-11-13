//! #8160 — `GET /library/tracks/{id}/fiche-technique`, par le vrai routeur :
//! une piste locale réunit, datés, le format, le spectrogramme conservé, le DR
//! et sa source, le faux hi-res, l'intégrité, la provenance et la note du
//! profil ; une piste de service n'a que ce qui existe pour elle, sans section
//! vide ; une piste inconnue rend 404.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::state::AppState;

fn ecrire_wav(chemin: &std::path::Path) {
    let sr: u32 = 44_100;
    let n = sr as usize;
    let donnees = (n * 2) as u32;
    let mut o = Vec::with_capacity(44 + donnees as usize);
    o.extend_from_slice(b"RIFF");
    o.extend_from_slice(&(36 + donnees).to_le_bytes());
    o.extend_from_slice(b"WAVEfmt ");
    o.extend_from_slice(&16u32.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes());
    o.extend_from_slice(&1u16.to_le_bytes());
    o.extend_from_slice(&sr.to_le_bytes());
    o.extend_from_slice(&(sr * 2).to_le_bytes());
    o.extend_from_slice(&2u16.to_le_bytes());
    o.extend_from_slice(&16u16.to_le_bytes());
    o.extend_from_slice(b"data");
    o.extend_from_slice(&donnees.to_le_bytes());
    for i in 0..n {
        let s = 0.5 * (std::f64::consts::TAU * 440.0 * i as f64 / sr as f64).sin();
        o.extend_from_slice(&((s * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(chemin, o).unwrap();
}

fn executer(state: &AppState, sql: &str) {
    state
        .backend
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql} : {e}"));
}

async fn appel(
    app: &axum::Router,
    methode: &str,
    uri: &str,
    corps: Option<&str>,
) -> (StatusCode, Value) {
    let mut req = Request::builder().method(methode).uri(uri);
    if corps.is_some() {
        req = req.header("content-type", "application/json");
    }
    let r = app
        .clone()
        .oneshot(
            req.body(
                corps
                    .map(|c| Body::from(c.to_string()))
                    .unwrap_or_else(Body::empty),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    let statut = r.status();
    let octets = axum::body::to_bytes(r.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        statut,
        serde_json::from_slice(&octets).unwrap_or(Value::Null),
    )
}

/// Une piste locale 1 avec tout ce que la fiche réunit, et une piste de
/// service 2 qui n'a que ses identifiants.
fn semer(state: &AppState, chemin: &std::path::Path) {
    let chemin = chemin.to_string_lossy();
    executer(
        state,
        &format!(
            "INSERT INTO tracks (id, title, file_path, duration_ms, source, format, sample_rate, \
             bit_depth, channels, bitrate_kbps, isrc, musicbrainz_recording_id) VALUES \
             (1, 'locale', '{chemin}', 1000, 'local', 'wav', 44100, 16, 1, 706, 'FRXXX2600001', \
              '0f0e0d0c-0000-4000-8000-000000000001')"
        ),
    );
    executer(
        state,
        "INSERT INTO tracks (id, title, duration_ms, source, source_id, format, sample_rate, bit_depth, channels) \
         VALUES (2, 'service', 180000, 'qobuz', 'q-2', 'flac', 96000, 24, 2)",
    );
    executer(
        state,
        "INSERT INTO track_metadata (track_id, key, value) VALUES \
         (1, 'dr_track', '11'), (1, 'dr_source', 'analysis'), (1, 'dr_algo', 'tt-dr-v1'), \
         (1, 'rg_analyzed', '1762682400')",
    );
    executer(
        state,
        "INSERT INTO hires_verifications (track_id, verdict, raison, coupure_hz, bits_effectifs, \
         sample_rate, bits_declares, mesure_le) VALUES \
         (1, 'conforme', NULL, 21000.0, 16, 44100, 16, '2026-11-08T09:00:00Z')",
    );
    executer(
        state,
        "INSERT INTO track_credits (track_id, artist_name, role, source) VALUES \
         (1, 'Une', 'performer', 'tag'), (1, 'Deux', 'composer', 'tag'), \
         (1, 'Trois', 'producer', 'musicbrainz')",
    );
    let etat = r#"{"generation":"g-8160","status":"completed","total":1,"traites":1,"sains":1,"corrompus":0,"non_verifiables":0,"alertes_taille":0,"curseur":1,"dernier_id":1,"erreur":null}"#;
    let ligne = format!(
        r#"{{"id":1,"chemin":"{chemin}","taille":{},"taille_sans_nouvelle_date":false,"resultat":{{"verdict":"sain","code":"ok","erreur":null,"position_trames":null,"trames_decodees":44100}},"mode":"full"}}"#,
        std::fs::metadata(chemin.as_ref()).unwrap().len()
    );
    state
        .backend
        .execute(
            "INSERT INTO settings (key, value, updated_at) VALUES \
             ('audit_integrite_etat', ?, '2026-11-08T10:00:00Z'), \
             ('audit_integrite_ligne:g-8160:00000000000000000001', ?, '2026-11-08T10:00:05Z')",
            &[&etat, &ligne],
        )
        .unwrap();
    // La note d'un AUTRE profil ne doit jamais paraître.
    executer(
        state,
        "INSERT INTO notes_personnelles (profile_id, item_type, item_id, texte_md) \
         VALUES (99, 'track', 2, 'note d''un autre profil')",
    );
}

#[tokio::test]
async fn fiche_d_une_piste_locale_8160() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("locale.wav");
    ecrire_wav(&chemin);
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    semer(&state, &chemin);
    let app = crate::routes::router(state.clone());

    let (statut, _) = appel(
        &app,
        "PUT",
        "/api/v1/library/notes/track/1",
        Some(r#"{"texte_md":"**Pressage** japonais"}"#),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);

    let (statut, f) = appel(
        &app,
        "GET",
        "/api/v1/library/tracks/1/fiche-technique",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{f}");
    assert_eq!(f["track_id"], 1);
    assert_eq!(f["a_un_fichier"], true);
    assert!(f["genere_le"].as_str().is_some_and(|d| d.ends_with('Z')));

    let format = &f["format"];
    assert_eq!(format["codec"], "wav");
    assert_eq!(format["sample_rate"], 44100);
    assert_eq!(format["bit_depth"], 16);
    assert_eq!(format["channels"], 1);
    assert_eq!(format["bitrate_kbps"], 706);
    assert!(format["fichier_modifie_le"].as_str().is_some(), "{format}");

    // Jamais calculé : absent, avec l'URL à demander.
    assert_eq!(
        f["spectrogramme"]["etat"], "absent",
        "{}",
        f["spectrogramme"]
    );
    assert_eq!(
        f["spectrogramme"]["url"],
        "/api/v1/library/tracks/1/spectrogramme.png?largeur=1200&hauteur=400"
    );

    assert_eq!(f["dynamic_range"]["valeur"], 11);
    assert_eq!(f["dynamic_range"]["source"], "analysis");
    assert_eq!(f["dynamic_range"]["algo"], "tt-dr-v1");
    assert_eq!(f["dynamic_range"]["mesure_le"], "2025-11-09T10:00:00Z");

    assert_eq!(f["faux_hires"]["verdict"], "conforme");
    assert_eq!(f["faux_hires"]["bits_effectifs"], 16);
    assert_eq!(f["faux_hires"]["mesure_le"], "2026-11-08T09:00:00Z");

    assert_eq!(f["integrite"]["verdict"], "sain", "{}", f["integrite"]);
    assert_eq!(f["integrite"]["mode"], "full");
    assert_eq!(f["integrite"]["audite_le"], "2026-11-08T10:00:05Z");
    assert_eq!(f["integrite"]["fichier_change_depuis"], false);

    let p = &f["provenance"];
    assert_eq!(p["source"], "local");
    assert_eq!(p["isrc"], "FRXXX2600001");
    assert_eq!(p["balises_lues_du_fichier"], true);
    assert_eq!(
        p["credits"],
        serde_json::json!([
            {"source": "musicbrainz", "nombre": 1},
            {"source": "tag", "nombre": 2},
        ])
    );
    assert!(
        p.get("champs_tenus").is_none(),
        "aucun champ tenu : clé absente"
    );

    assert_eq!(f["note"]["texte_md"], "**Pressage** japonais");
    assert!(f["note"]["maj_le"].as_str().is_some());
    // Fil 2793 — la forme d'onde : son URL, calculée à la demande.
    assert_eq!(
        f["forme_d_onde"]["url"], "/api/v1/library/tracks/1/forme-onde?colonnes=1000",
        "{}",
        f["forme_d_onde"]
    );
    assert!(
        ["absent", "a_jour"].contains(&f["forme_d_onde"]["etat"].as_str().unwrap_or("")),
        "{}",
        f["forme_d_onde"]
    );

    // Recalcul, puis la fiche dit « à jour », datée.
    let (statut, s) = appel(&app, "POST", "/api/v1/library/tracks/1/spectrogramme", None).await;
    assert_eq!(statut, StatusCode::OK, "{s}");
    assert_eq!(s["etat"], "a_jour");
    let (_, f) = appel(
        &app,
        "GET",
        "/api/v1/library/tracks/1/fiche-technique",
        None,
    )
    .await;
    assert_eq!(f["spectrogramme"]["etat"], "a_jour");
    assert!(f["spectrogramme"]["calcule_le"].as_str().is_some());
    assert!(f["spectrogramme"]["octets"].as_i64().unwrap() > 0);

    // Le fichier change : intégrité et spectrogramme le disent.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut octets = std::fs::read(&chemin).unwrap();
    octets.extend_from_slice(&[0u8; 64]);
    std::fs::write(&chemin, octets).unwrap();
    let (_, f) = appel(
        &app,
        "GET",
        "/api/v1/library/tracks/1/fiche-technique",
        None,
    )
    .await;
    assert_eq!(f["spectrogramme"]["etat"], "perime");
    assert_eq!(f["integrite"]["fichier_change_depuis"], true);
}

#[tokio::test]
async fn fiche_d_une_piste_de_service_sans_section_vide_8160() {
    let dossier = tempfile::tempdir().unwrap();
    let chemin = dossier.path().join("locale.wav");
    ecrire_wav(&chemin);
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    semer(&state, &chemin);
    let app = crate::routes::router(state);

    let (statut, f) = appel(
        &app,
        "GET",
        "/api/v1/library/tracks/2/fiche-technique",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::OK, "{f}");
    assert_eq!(f["a_un_fichier"], false);
    assert_eq!(f["format"]["codec"], "flac");
    assert_eq!(f["format"]["sample_rate"], 96000);
    for vide in [
        "spectrogramme",
        "dynamic_range",
        "faux_hires",
        "integrite",
        "note",
        "forme_d_onde",
    ] {
        assert!(
            f[vide].is_null(),
            "« {vide} » doit valoir null pour une piste de service : {f}"
        );
    }
    assert_eq!(f["provenance"]["source"], "qobuz");
    assert_eq!(f["provenance"]["source_id"], "q-2");
    assert!(
        f["provenance"].get("balises_lues_du_fichier").is_none()
            && f["provenance"].get("credits").is_none(),
        "{}",
        f["provenance"]
    );

    let (statut, _) = appel(
        &app,
        "GET",
        "/api/v1/library/tracks/404/fiche-technique",
        None,
    )
    .await;
    assert_eq!(statut, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn douteux_masque_se_dit_non_analyse_8160() {
    let state = AppState::new(":memory:", 0, Default::default()).unwrap();
    executer(
        &state,
        "INSERT INTO tracks (id, title, source) VALUES (3, 'douteuse', 'local')",
    );
    executer(
        &state,
        "INSERT INTO hires_verifications (track_id, verdict, raison) VALUES (3, 'douteux', 'cd_gonfle')",
    );
    let app = crate::routes::router(state);
    let (_, f) = appel(
        &app,
        "GET",
        "/api/v1/library/tracks/3/fiche-technique",
        None,
    )
    .await;
    assert_eq!(f["faux_hires"]["verdict"], "non_analyse");
    assert!(f["faux_hires"]["raison"].is_null());
}
