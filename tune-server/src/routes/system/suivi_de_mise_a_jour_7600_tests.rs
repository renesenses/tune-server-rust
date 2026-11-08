//! Témoins de #7600 : chaque mise à jour qui n'aboutit pas laisse, sur
//! `GET /system/update/status`, un état d'échec explicite avec sa raison, et
//! jamais un « en cours » sans fin.

use axum::extract::State;
use serde_json::Value;

use super::{
    PREFIXE_DOCKER, bloque_une_nouvelle_tentative, noter_a_jour, noter_l_echec, poser_la_phase,
    raison, surveiller_la_tache,
};
use crate::routes::system::update::{UpdateBlame, update_status};
use crate::state::AppState;

fn etat() -> AppState {
    AppState::new(":memory:", 0, Default::default()).unwrap()
}

async fn suivi(state: &AppState) -> Value {
    update_status(State(state.clone())).await.0
}

fn assert_echec(corps: &Value, raison_attendue: &str) {
    assert_eq!(
        corps["update_in_progress"], false,
        "un échec ne doit pas rester « en cours » : {corps}"
    );
    assert_eq!(
        corps["status"], "failed",
        "état d'échec explicite : {corps}"
    );
    assert_eq!(
        corps["reason"], raison_attendue,
        "raison de l'échec : {corps}"
    );
    assert!(
        corps["phase"]
            .as_str()
            .is_some_and(|p| p.starts_with("failed")),
        "les clients actuels lisent la phase `failed…` : {corps}"
    );
    assert!(
        corps["message"].as_str().is_some_and(|m| !m.is_empty()),
        "le message dit la cause : {corps}"
    );
}

/// Avant : sans tentative, la route de suivi dit « au repos ».
#[tokio::test]
async fn sans_tentative_la_route_de_suivi_dit_au_repos_7600() {
    let state = etat();
    let corps = suivi(&state).await;
    assert_eq!(corps["status"], "idle", "{corps}");
    assert!(corps["reason"].is_null(), "{corps}");
    assert_eq!(corps["update_in_progress"], false);
}

/// Docker, paquet, `.no-auto-update`, droits, vérification, archive absente :
/// la raison du refus, jusque-là rendue dans la seule réponse au POST, se lit
/// sur la route de suivi.
#[tokio::test]
async fn chaque_refus_avant_telechargement_se_lit_sur_la_route_de_suivi_7600() {
    for raison_du_refus in [
        raison::DOCKER,
        raison::PACKAGE_MANAGED,
        raison::NOT_WRITABLE,
        raison::CHECK_FAILED,
        raison::NO_ASSET,
    ] {
        let state = etat();
        noter_l_echec(
            &state.update_phase,
            &state.update_issue,
            raison_du_refus,
            "refus de test",
        );
        let corps = suivi(&state).await;
        assert_echec(&corps, raison_du_refus);
        assert_eq!(corps["message"], "refus de test");
    }
}

/// Les échecs de la tâche d'arrière-plan prennent la raison de l'étape où ils
/// surviennent : téléchargement, signature, extraction, droits d'écriture.
#[tokio::test]
async fn un_echec_en_arriere_plan_porte_la_raison_de_son_etape_7600() {
    let signature = format!("failed: {}", UpdateBlame::Untrusted.user_message());
    let cas: [(&str, &str, &str); 5] = [
        (
            "downloading",
            "failed: Download failed: HTTP 404",
            raison::DOWNLOAD_FAILED,
        ),
        (
            "verifying",
            signature.as_str(),
            raison::SIGNATURE_CHECK_FAILED,
        ),
        (
            "extracting",
            "failed: Extraction failed: invalid gzip header",
            raison::EXTRACT_FAILED,
        ),
        (
            "installing",
            "failed: Install failed: copy new binary: Permission denied (os error 13)",
            raison::INSTALL_FAILED,
        ),
        (
            "installing",
            "failed: Install crashed: index out of bounds",
            raison::INSTALL_CRASHED,
        ),
    ];
    let docker = format!("failed: {PREFIXE_DOCKER} — update by pulling the new image");
    let cas: Vec<(&str, &str, &str)> = cas
        .into_iter()
        .chain([("installing", docker.as_str(), raison::DOCKER)])
        .collect();
    for (etape, echec, attendue) in cas {
        let state = etat();
        poser_la_phase(&state.update_phase, &state.update_issue, "downloading");
        poser_la_phase(&state.update_phase, &state.update_issue, etape);
        let pendant = suivi(&state).await;
        assert_eq!(pendant["status"], "in_progress", "{pendant}");
        assert!(pendant["reason"].is_null(), "{pendant}");

        poser_la_phase(&state.update_phase, &state.update_issue, echec);
        let corps = suivi(&state).await;
        assert_echec(&corps, attendue);
        assert_eq!(
            corps["message"],
            echec.trim_start_matches("failed:").trim(),
            "le message est celui de l'étape"
        );
    }
}

/// La tâche d'arrière-plan qui panique pendant le téléchargement laissait la
/// phase figée sur `downloading` : « en cours » pour toujours, et chaque
/// nouveau clic refusé en `already_in_progress`.
#[tokio::test]
async fn une_tache_qui_panique_ne_laisse_plus_d_etat_en_cours_7600() {
    let state = etat();
    poser_la_phase(&state.update_phase, &state.update_issue, "downloading");
    let tache = tokio::spawn(async {
        panic!("panique de test pendant le téléchargement");
    });
    surveiller_la_tache(
        tache,
        state.update_phase.clone(),
        state.update_issue.clone(),
    )
    .await;
    let corps = suivi(&state).await;
    assert_echec(&corps, raison::TASK_ABORTED);
    assert!(
        corps["message"]
            .as_str()
            .is_some_and(|m| m.contains("downloading")),
        "le message nomme l'étape interrompue : {corps}"
    );
    assert!(
        !bloque_une_nouvelle_tentative(corps["phase"].as_str()),
        "après `task_aborted`, un nouveau clic ne doit plus être refusé en `already_in_progress` : {corps}"
    );
}

/// Une tâche annulée, ou rendue sans résultat, ne fige pas non plus l'état.
#[tokio::test]
async fn une_tache_annulee_ne_laisse_plus_d_etat_en_cours_7600() {
    let state = etat();
    poser_la_phase(&state.update_phase, &state.update_issue, "extracting");
    let tache = tokio::spawn(async {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    });
    tache.abort();
    surveiller_la_tache(
        tache,
        state.update_phase.clone(),
        state.update_issue.clone(),
    )
    .await;
    assert_echec(&suivi(&state).await, raison::TASK_ABORTED);
}

/// La surveillance ne réécrit ni un échec déjà posé, ni une relance.
#[tokio::test]
async fn la_surveillance_garde_l_echec_ou_la_relance_deja_poses_7600() {
    let state = etat();
    let (phase, issue) = (state.update_phase.clone(), state.update_issue.clone());
    let tache = tokio::spawn(async move {
        poser_la_phase(&phase, &issue, "downloading");
        poser_la_phase(&phase, &issue, "failed: Download failed: timeout");
    });
    surveiller_la_tache(
        tache,
        state.update_phase.clone(),
        state.update_issue.clone(),
    )
    .await;
    let corps = suivi(&state).await;
    assert_echec(&corps, raison::DOWNLOAD_FAILED);

    let state = etat();
    poser_la_phase(&state.update_phase, &state.update_issue, "restarting");
    let tache = tokio::spawn(async {});
    surveiller_la_tache(
        tache,
        state.update_phase.clone(),
        state.update_issue.clone(),
    )
    .await;
    let corps = suivi(&state).await;
    assert_eq!(corps["phase"], "restarting", "{corps}");
    assert_eq!(corps["status"], "in_progress", "{corps}");
}

/// « Déjà à jour » n'est pas un échec, mais une fin lisible.
#[tokio::test]
async fn deja_a_jour_se_lit_sur_la_route_de_suivi_7600() {
    let state = etat();
    noter_l_echec(
        &state.update_phase,
        &state.update_issue,
        raison::CHECK_FAILED,
        "ancien échec",
    );
    noter_a_jour(
        &state.update_phase,
        &state.update_issue,
        "Already running the latest version",
    );
    let corps = suivi(&state).await;
    assert_eq!(corps["status"], "up_to_date", "{corps}");
    assert_eq!(corps["reason"], raison::UP_TO_DATE, "{corps}");
    assert!(corps["phase"].is_null(), "{corps}");
    assert_eq!(corps["update_in_progress"], false);
}

/// Une nouvelle tentative efface la raison de la précédente.
#[tokio::test]
async fn une_nouvelle_tentative_efface_la_raison_precedente_7600() {
    let state = etat();
    noter_l_echec(
        &state.update_phase,
        &state.update_issue,
        raison::NOT_WRITABLE,
        "dossier non inscriptible",
    );
    poser_la_phase(&state.update_phase, &state.update_issue, "downloading");
    let corps = suivi(&state).await;
    assert_eq!(corps["status"], "in_progress", "{corps}");
    assert!(corps["reason"].is_null(), "{corps}");
    assert_eq!(corps["update_in_progress"], true);
}

/// Le corps de `update_install`, sans sa définition.
fn corps_de_update_install() -> &'static str {
    let src = include_str!("update.rs");
    let debut = src
        .find("pub(super) async fn update_install(")
        .expect("update_install");
    let corps = &src[debut..];
    let fin = corps.find("\n}\n").expect("fin de update_install");
    &corps[..fin]
}

/// Garde de branchement : chaque refus définitif de `update_install` pose
/// sa raison, la tâche d'arrière-plan passe par `poser_la_phase`, et elle est
/// surveillée.
#[test]
fn update_install_branche_chaque_refus_sur_la_route_de_suivi_7600() {
    let corps = corps_de_update_install();
    for (appel, attendu) in [
        ("noter_l_echec(&state, raison::DOCKER,", 1),
        ("raison::PACKAGE_MANAGED,", 2),
        ("noter_l_echec(&state, raison::NOT_WRITABLE,", 1),
        ("noter_l_echec(&state, raison::CHECK_FAILED,", 1),
        ("noter_l_echec(&state, raison::NO_ASSET,", 1),
        ("suivi::noter_a_jour(", 1),
        ("suivi::poser_la_phase(&update_phase, &update_issue, p)", 1),
        ("suivi::surveiller_la_tache(", 1),
    ] {
        assert_eq!(
            corps.matches(appel).count(),
            attendu,
            "update_install doit appeler `{appel}` {attendu} fois"
        );
    }
    assert_eq!(
        corps
            .matches("suivi::bloque_une_nouvelle_tentative(Some(p))")
            .count(),
        1,
        "la garde `already_in_progress` doit passer par `bloque_une_nouvelle_tentative`"
    );
    assert!(
        !corps.contains("*update_phase.lock().unwrap() = Some("),
        "la tâche ne doit plus écrire la phase sans sa raison"
    );
}

/// La garde Docker de secours de la tâche n'est reconnue que par le début de
/// sa phrase : si `update.rs` la réécrit, sa raison retomberait sans bruit
/// sur `install_failed`.
#[test]
fn la_phrase_docker_de_la_tache_est_celle_que_le_suivi_reconnait_7600() {
    let attendu = format!("\"failed: {PREFIXE_DOCKER}");
    assert_eq!(
        corps_de_update_install().matches(attendu.as_str()).count(),
        1,
        "la garde Docker de la tâche doit écrire une phase commençant par `failed: {PREFIXE_DOCKER}`"
    );
}
