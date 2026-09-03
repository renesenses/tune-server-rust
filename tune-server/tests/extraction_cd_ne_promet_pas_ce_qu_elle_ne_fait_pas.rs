//! L'extraction de CD ne promet plus ce qu'elle ne fait pas — #2466.
//!
//! `POST /cd-rip/rip` écrivait `cd_rip_current = {"status":"running"}` et
//! rendait `200 {"status":"started"}` sans lancer le moindre processus. Les
//! seuls `Command::new` de `routes/cd_rip.rs` sont ceux de la détection et de
//! la lecture de sommaire ; `cd_rip_current` n'est écrit et relu que par ce
//! fichier, aucune tâche de fond ne le consomme. `rip_status` rendait donc
//! `running` INDÉFINIMENT, et `cancel_rip` repassait un texte de succès sans
//! rien avoir à interrompre.
//!
//! L'arbitrage du 03/09 est de rendre la route honnête, PAS d'écrire
//! l'extracteur. Ce fichier mesure donc, sur le routeur réel, le CODE DE
//! STATUT et le CORPS JSON — jamais la condition du code, qu'un test ne ferait
//! que recopier.
//!
//! ⚠️ PRÉ-REQUIS DE MACHINE : la machine d'épreuve ne doit porter ni
//! `cdparanoia` ni `cdda2wav` (Shrek et les exécuteurs de la CI n'en portent
//! aucun). L'assertion de pré-requis est explicite et nommée : si elle tombe,
//! ce n'est pas la correction qui est en cause, c'est la machine. Les deux
//! branches de la décision, elles, sont éprouvées en fonctions pures dans
//! `src/routes/cd_rip.rs` — aucune machine ne porte les deux configurations à
//! la fois.
//!
//! ⚠️ `tune-server` porte `autotests = false` : ce fichier n'est compilé que
//! parce qu'il est déclaré dans l'agrégateur `server_contracts.rs`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;

/// Un serveur en mémoire et son dépôt de réglages, pour semer un résidu.
fn app() -> (axum::Router, SettingsRepo) {
    let state = tune_server::state::AppState::new(":memory:", 0, Default::default()).unwrap();
    let settings = SettingsRepo::with_backend(state.backend.clone());
    (tune_server::routes::router(state), settings)
}

async fn reponse(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(json!(null)),
    )
}

async fn get(app: &axum::Router, chemin: &str) -> (StatusCode, Value) {
    reponse(
        app,
        Request::get(format!("/api/v1{chemin}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await
}

async fn post(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value) {
    reponse(
        app,
        Request::post(format!("/api/v1{chemin}"))
            .header("Content-Type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

/// Secondes UNIX — la forme qu'écrit `cd_rip.rs` dans `started_at`.
fn maintenant_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// La machine d'épreuve ne porte aucun extracteur. Mesuré par la route de
/// détection elle-même, pas déduit d'un `#[cfg]`.
async fn exiger_machine_nue(app: &axum::Router) {
    let (status, corps) = get(app, "/cd-rip/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        corps["available"].as_bool(),
        Some(false),
        "PRÉ-REQUIS : cette épreuve exige une machine SANS cdparanoia ni \
         cdda2wav. Ce n'est pas la correction de #2466 qui est en cause, \
         c'est la machine — {corps}"
    );
}

// ---------------------------------------------------------------------------
// 1. LE DÉFAUT — « started » sur une machine qui ne peut rien extraire.
// ---------------------------------------------------------------------------

/// Le cœur de #2466 : sans extracteur, `POST /cd-rip/rip` ne rend plus
/// `{"status":"started"}`. Il rend un document de refus qui NOMME les outils
/// absents, avec un code stable pour le client.
#[tokio::test]
async fn sans_extracteur_le_depart_d_extraction_est_refuse_et_le_dit() {
    let (app, _) = app();
    exiger_machine_nue(&app).await;

    let (status, corps) = post(&app, "/cd-rip/rip", json!({})).await;

    assert_eq!(status, StatusCode::OK, "{corps}");
    assert_ne!(
        corps["status"].as_str(),
        Some("started"),
        "aucun processus n'a été lancé : annoncer « started » est le défaut — {corps}"
    );
    assert_eq!(
        corps["status"].as_str(),
        Some("not_available"),
        "même vocabulaire que sacd_rip.rs — {corps}"
    );
    assert_eq!(
        corps["reason"].as_str(),
        Some("no_cd_extractor"),
        "le client lit ce code pour choisir sa traduction — {corps}"
    );

    let message = corps["message"].as_str().unwrap_or_default();
    for outil in ["cdparanoia", "cdda2wav"] {
        assert!(
            message.contains(outil),
            "le refus doit dire CE QUI MANQUE, pas « indisponible » en bloc : {message}"
        );
        assert!(
            corps["missing"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(outil))),
            "{corps}"
        );
    }
    assert!(
        !message.contains("start_rip") && !message.contains("cd_rip_current"),
        "aucun nom de fonction ni de clef interne dans une phrase lue par un \
         humain : {message}"
    );
}

/// Et la relecture dit la même chose : `GET /cd-rip/rip/status` ne rend pas
/// `running` après un départ refusé. C'est l'autre moitié du ticket — avant,
/// un seul appel suffisait à figer l'écran sur « extraction en cours ».
#[tokio::test]
async fn apres_un_depart_refuse_la_relecture_ne_dit_pas_running() {
    let (app, _) = app();
    exiger_machine_nue(&app).await;

    let (_, depart) = post(&app, "/cd-rip/rip", json!({ "format": "flac" })).await;

    let (status, corps) = get(&app, "/cd-rip/rip/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(
        corps["status"].as_str(),
        Some("running"),
        "rien ne tourne : le statut ne peut pas dire le contraire — {corps}"
    );
    assert_eq!(corps["status"].as_str(), Some("not_available"), "{corps}");
    assert_eq!(
        corps["id"], depart["id"],
        "la relecture doit porter sur le même départ que la réponse au POST"
    );
}

/// La contre-épreuve : dire la vérité n'a rien retiré au contrat. Les champs
/// que lisait un client avant la correction sont tous encore là.
#[tokio::test]
async fn le_refus_garde_les_champs_que_lisait_deja_le_client() {
    let (app, _) = app();
    exiger_machine_nue(&app).await;

    let (_, corps) = post(
        &app,
        "/cd-rip/rip",
        json!({ "output_dir": "/tmp/x", "format": "aiff", "device": "/dev/sr0" }),
    )
    .await;

    for champ in ["id", "status", "output_dir", "format", "message"] {
        assert!(
            corps.get(champ).is_some(),
            "champ historique disparu de la réponse : {champ} — {corps}"
        );
    }
    assert_eq!(corps["output_dir"].as_str(), Some("/tmp/x"));
    assert_eq!(corps["format"].as_str(), Some("aiff"));
    assert_eq!(corps["device"].as_str(), Some("/dev/sr0"));
}

// ---------------------------------------------------------------------------
// 2. LE RÉSIDU — le `running` que l'ancien code a laissé en base.
// ---------------------------------------------------------------------------

/// Une installation qui porte déjà `cd_rip_current = {"status":"running"}`,
/// écrit par l'ancien code et jamais effaçable : au redémarrage, l'écran
/// affichait une extraction en cours À VIE. Ce résidu survit au correctif s'il
/// n'est pas traité — il est traité à la relecture.
#[tokio::test]
async fn le_residu_running_d_une_ancienne_installation_ne_dit_plus_running() {
    let (app, settings) = app();
    settings
        .set(
            "cd_rip_current",
            &json!({
                "id": "vieux", "status": "running", "output_dir": "/music",
                "format": "wav", "progress": 0, "started_at": "1000",
            })
            .to_string(),
        )
        .unwrap();

    let (status, corps) = get(&app, "/cd-rip/rip/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(
        corps["status"].as_str(),
        Some("running"),
        "un enregistrement antérieur au démarrage du processus ne peut pas \
         courir — {corps}"
    );
    assert_eq!(corps["status"].as_str(), Some("interrupted"), "{corps}");
    assert_eq!(
        corps["id"].as_str(),
        Some("vieux"),
        "le document est corrigé, pas jeté"
    );
    assert!(
        !corps["message"].as_str().unwrap_or_default().is_empty(),
        "un état corrigé sans un mot ne vaut pas mieux — {corps}"
    );
}

/// Et le résidu est NETTOYÉ en base, pas seulement dans la réponse : la clef
/// relue directement ne dit plus `running`. Sans cela, chaque lecture
/// repartirait du résidu, et un client qui lit le réglage par une autre voie
/// verrait encore une extraction en cours.
#[tokio::test]
async fn le_residu_est_efface_de_la_base_pas_seulement_de_la_reponse() {
    let (app, settings) = app();
    settings
        .set(
            "cd_rip_current",
            &json!({ "id": "vieux", "status": "running", "started_at": "1000" }).to_string(),
        )
        .unwrap();

    let (_, premiere) = get(&app, "/cd-rip/rip/status").await;
    assert_eq!(premiere["status"].as_str(), Some("interrupted"));

    let en_base: Value =
        serde_json::from_str(&settings.get("cd_rip_current").unwrap().unwrap()).unwrap();
    assert_ne!(
        en_base["status"].as_str(),
        Some("running"),
        "le résidu est resté en base : il ressortira au prochain démarrage — {en_base}"
    );

    let (_, seconde) = get(&app, "/cd-rip/rip/status").await;
    assert_eq!(seconde, premiere, "la correction doit être stable");
}

// ---------------------------------------------------------------------------
// 3. LE TÉMOIN — ce que la correction n'a PAS le droit de changer.
// ---------------------------------------------------------------------------

/// LE TÉMOIN. Une extraction enregistrée par CE processus — le comportement
/// d'une machine correctement équipée — est rendue telle quelle. La garde de
/// #2466 ne doit refuser ni réécrire une installation qui, elle, a de quoi
/// extraire.
///
/// Ce test doit rester VERT quand on retire la garde de disponibilité du code
/// de production : il ne mesure pas la garde, il mesure ce qu'elle épargne.
#[tokio::test]
async fn le_temoin_une_extraction_de_ce_processus_est_rendue_intacte() {
    let (app, settings) = app();
    let en_cours = json!({
        "id": "frais", "status": "running", "output_dir": "/music",
        "format": "wav", "progress": 12, "started_at": maintenant_unix().to_string(),
    });
    settings
        .set("cd_rip_current", &en_cours.to_string())
        .unwrap();

    let (status, corps) = get(&app, "/cd-rip/rip/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        corps["status"].as_str(),
        Some("running"),
        "une extraction que ce processus vient d'inscrire n'est pas un résidu — {corps}"
    );
    assert_eq!(corps["progress"].as_i64(), Some(12));
    assert_eq!(corps, en_cours, "le document ne doit pas être retouché");
}

/// Sans aucun enregistrement, la route reste `idle` — inchangé.
#[tokio::test]
async fn le_temoin_sans_enregistrement_le_statut_reste_idle() {
    let (app, _) = app();
    let (status, corps) = get(&app, "/cd-rip/rip/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(corps["status"].as_str(), Some("idle"), "{corps}");
}

// ---------------------------------------------------------------------------
// 4. L'ANNULATION — elle n'a jamais rien tué, elle ne le prétend plus.
// ---------------------------------------------------------------------------

/// `cancel_rip` repassait « Rip task cancelled » sans rien à interrompre. Il
/// efface un état enregistré : c'est tout ce qu'il a le droit d'annoncer, et
/// après lui la relecture ne dit plus `running`.
#[tokio::test]
async fn l_annulation_efface_l_etat_sans_pretendre_avoir_tue_un_processus() {
    let (app, settings) = app();
    settings
        .set(
            "cd_rip_current",
            &json!({ "id": "vieux", "status": "running", "started_at": "1000" }).to_string(),
        )
        .unwrap();

    let (status, corps) = post(&app, "/cd-rip/rip/cancel", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(corps["status"].as_str(), Some("cancelled"), "{corps}");
    let message = corps["message"].as_str().unwrap_or_default();
    assert_ne!(
        message, "Rip task cancelled",
        "cette phrase-là annonçait l'interruption d'un processus qui n'a \
         jamais existé"
    );
    assert!(
        !message.is_empty(),
        "l'annulation doit dire ce qu'elle a fait — {corps}"
    );

    let (_, relu) = get(&app, "/cd-rip/rip/status").await;
    assert_ne!(relu["status"].as_str(), Some("running"), "{relu}");
    assert_eq!(relu["status"].as_str(), Some("cancelled"));
}
