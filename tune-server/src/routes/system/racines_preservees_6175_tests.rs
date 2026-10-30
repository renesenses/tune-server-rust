use super::*;
use crate::auth::RequireAdmin;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

fn reglages(state: &AppState) -> SettingsRepo {
    SettingsRepo::with_backend(state.backend.clone())
}

async fn ajout(state: &AppState, path: &str) -> Result<axum::response::Response, AppError> {
    add_music_dir(
        RequireAdmin,
        State(state.clone()),
        Json(AddMusicDir { path: path.into() }),
    )
    .await
    .map(IntoResponse::into_response)
}

fn erreur(result: Result<impl IntoResponse, AppError>) {
    match result {
        Err(e) => assert_eq!(e.status, StatusCode::INTERNAL_SERVER_ERROR),
        Ok(_) => {
            panic!("#6175 : une erreur de réglage ne doit pas annoncer un ajout/retrait réussi")
        }
    }
}

#[tokio::test]
async fn une_valeur_illisible_reste_intacte_apres_ajout_et_retrait_6175() {
    let state = etat();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    for invalide in ["[\"racine-privee\",", "null", "{}", "[42]", "\"/musique\""] {
        reglages(&state).set("music_dirs", invalide).unwrap();
        erreur(ajout(&state, path).await);
        assert_eq!(
            reglages(&state).get("music_dirs").unwrap().as_deref(),
            Some(invalide)
        );
        erreur(retirer_un_dossier(&state, path, None).map(Json));
        assert_eq!(
            reglages(&state).get("music_dirs").unwrap().as_deref(),
            Some(invalide)
        );
    }
}

#[tokio::test]
async fn une_erreur_sql_de_lecture_refuse_les_deux_modifications_6175() {
    let state = etat();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    reglages(&state)
        .set("music_dirs", "[\"/musique\"]")
        .unwrap();
    state
        .backend
        .execute(
            "ALTER TABLE settings RENAME TO settings_conservees_6175",
            &[],
        )
        .unwrap();
    erreur(ajout(&state, path).await);
    erreur(retirer_un_dossier(&state, path, None).map(Json));
    state
        .backend
        .execute(
            "ALTER TABLE settings_conservees_6175 RENAME TO settings",
            &[],
        )
        .unwrap();
    assert_eq!(
        reglages(&state).get("music_dirs").unwrap().as_deref(),
        Some("[\"/musique\"]")
    );
}

#[tokio::test]
async fn une_erreur_sql_d_ecriture_ne_repond_pas_succes_6175() {
    let state = etat();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let valeur = "[\"/musique\",\"/autre\"]";
    reglages(&state).set("music_dirs", valeur).unwrap();
    state.backend.execute(
        "CREATE TRIGGER refus_ecriture_6175 BEFORE INSERT ON settings WHEN NEW.key = 'music_dirs' BEGIN SELECT RAISE(ABORT, 'ecriture refusee pour le temoin'); END", &[]
    ).unwrap();
    erreur(ajout(&state, path).await);
    erreur(retirer_un_dossier(&state, "/musique", None).map(Json));
    assert_eq!(
        reglages(&state).get("music_dirs").unwrap().as_deref(),
        Some(valeur)
    );
}

#[tokio::test]
async fn un_ajout_deja_present_et_un_retrait_gardent_les_autres_racines_6175() {
    let state = etat();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().to_str().unwrap();
    let normalise = tune_core::scanner::walker::normalize_path(path);
    let autre = tune_core::scanner::walker::normalize_path("/autre-musique");
    let dirs = vec![normalise.clone(), autre.clone()];
    reglages(&state)
        .set("music_dirs", &serde_json::to_string(&dirs).unwrap())
        .unwrap();
    assert_eq!(
        ajout(&state, path).await.ok().unwrap().status(),
        StatusCode::OK
    );
    let rep = retirer_un_dossier(&state, path, None).ok().unwrap();
    assert_eq!(rep["dirs"], json!([autre]));
    let conservees: Vec<String> =
        serde_json::from_str(&reglages(&state).get("music_dirs").unwrap().unwrap()).unwrap();
    assert_eq!(conservees, vec![autre]);
}

#[tokio::test]
async fn un_reglage_absent_ou_vide_reste_un_cas_initial_valide_6175() {
    let state = etat();
    assert!(
        lire_racines_avant_modification(&reglages(&state))
            .ok()
            .unwrap()
            .is_empty()
    );
    reglages(&state).set("music_dirs", "[]").unwrap();
    assert!(
        lire_racines_avant_modification(&reglages(&state))
            .ok()
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        retirer_un_dossier(&state, "/inexistant", None)
            .ok()
            .unwrap()["dirs"],
        json!([])
    );
}
