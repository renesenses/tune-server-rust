//! Une clef d'API annoncée est une clef écrite (#2795).
//!
//! ## Le défaut
//!
//! `load_api_keys` et `load_webhooks` écrivaient
//! `.ok().flatten().and_then(…).unwrap_or_default()` : une panne de base et un
//! JSON corrompu rendaient exactement la même chose qu'une liste vide. La
//! liste vide servait ensuite de base au read-modify-write, et
//! `save_api_keys` jetait le `Result` de `settings.set`. Trois conséquences,
//! toutes silencieuses :
//!
//! 1. une clef affichée **une seule fois** au client, jamais persistée — et
//!    donc irrécupérable ;
//! 2. une révocation annoncée `{"ok":true}` qui laissait la clef valable ;
//! 3. une lecture en échec suivie d'une écriture qui remplaçait **toutes** les
//!    clefs par `[]`.
//!
//! ## Ce que ce fichier verrouille
//!
//! Un essai par critère d'acceptation de la #2795, plus les témoins
//! anti-régression : le chemin nominal doit rester nominal, et aucune clef ne
//! doit apparaître là où elle n'a rien à faire.
//!
//! Le journal, lui, est éprouvé à part : `cles_developpeur_hors_journal.rs`,
//! qui a besoin d'un abonné `tracing` **global** et donc d'un binaire à lui.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Tier;

const CLES: &str = "/api/v1/developer/api-keys";
const WEBHOOKS: &str = "/api/v1/developer/webhooks";
const INSTALLES: &str = "/api/v1/marketplace/plugins/installed";

/// Un serveur en mémoire, **premium** : sans quoi toutes les routes du
/// Developer API répondent 402 et l'essai n'observerait rien.
async fn serveur() -> (axum::Router, tune_server::state::AppState) {
    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("base en memoire");
    etat.license.update_from_server(Tier::Premium, None).await;
    let app = tune_server::routes::router(etat.clone());
    (app, etat)
}

async fn envoyer(app: &axum::Router, requete: Request<Body>) -> (StatusCode, Value, String) {
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let texte = String::from_utf8_lossy(&octets).into_owned();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(json!(null));
    (statut, corps, texte)
}

async fn poster(app: &axum::Router, chemin: &str, corps: Value) -> (StatusCode, Value, String) {
    envoyer(
        app,
        Request::post(chemin)
            .header("content-type", "application/json")
            .body(Body::from(corps.to_string()))
            .unwrap(),
    )
    .await
}

async fn lire(app: &axum::Router, chemin: &str) -> (StatusCode, Value, String) {
    envoyer(app, Request::get(chemin).body(Body::empty()).unwrap()).await
}

async fn supprimer(app: &axum::Router, chemin: &str) -> (StatusCode, Value, String) {
    envoyer(app, Request::delete(chemin).body(Body::empty()).unwrap()).await
}

fn demande_de_cle(nom: &str) -> Value {
    json!({ "name": nom, "scopes": ["read"] })
}

/// Casse la base sous les routes : une table absente est la forme la plus
/// simple d'une erreur SQL à l'exécution (même geste que
/// `panne_sql_journalisee.rs`).
fn casser(etat: &tune_server::state::AppState) {
    etat.backend
        .execute_batch("DROP TABLE settings")
        .expect("la table existe sur une base neuve");
}

// ---------------------------------------------------------------------------
// 1. Le témoin : le chemin nominal reste nominal
// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_cle_creee_est_relue_et_revocable() {
    let (app, _etat) = serveur().await;

    let (statut, corps, _) = poster(&app, CLES, demande_de_cle("outil de Rene")).await;
    assert_eq!(statut, StatusCode::CREATED, "creation : {corps}");
    let cle = corps["key"]
        .as_str()
        .expect("la clef est rendue une fois")
        .to_string();
    let id = corps["id"].as_str().unwrap().to_string();
    assert!(cle.starts_with("tunedev_"), "clef inattendue");

    let (statut, corps, texte) = lire(&app, CLES).await;
    assert_eq!(statut, StatusCode::OK);
    assert_eq!(
        corps["count"], 1,
        "la clef creee doit avoir ete PERSISTEE : {corps}"
    );

    // La liste ne rend qu'un aperçu : la clef entière n'y est plus.
    assert!(
        !texte.contains(&cle),
        "la clef complete ne doit jamais reapparaitre dans un listage"
    );
    assert_eq!(
        corps["api_keys"][0]["key_preview"],
        cle[..12].to_string() + "..."
    );

    let (statut, corps, _) = supprimer(&app, &format!("{CLES}/{id}")).await;
    assert_eq!(statut, StatusCode::OK, "revocation : {corps}");

    let (_, corps, _) = lire(&app, CLES).await;
    assert_eq!(
        corps["count"], 0,
        "la revocation doit avoir ete PERSISTEE : {corps}"
    );
}

#[tokio::test]
async fn une_revocation_inconnue_reste_un_404() {
    let (app, _etat) = serveur().await;
    let (statut, _, _) = supprimer(&app, &format!("{CLES}/jamais-vue")).await;
    assert_eq!(
        statut,
        StatusCode::NOT_FOUND,
        "un identifiant inconnu n'est pas une panne de stockage"
    );
}

// ---------------------------------------------------------------------------
// 2. Une panne de base se dit, et n'écrase rien
// ---------------------------------------------------------------------------

#[tokio::test]
async fn une_panne_de_base_ne_se_deguise_plus_en_liste_vide() {
    let (app, etat) = serveur().await;

    // Témoin : sur une base saine, les trois routes répondent normalement.
    let (statut_sain, _, _) = lire(&app, CLES).await;
    assert_eq!(statut_sain, StatusCode::OK);

    casser(&etat);

    for (nom, (statut, corps, _)) in [
        ("liste des clefs", lire(&app, CLES).await),
        (
            "creation de clef",
            poster(&app, CLES, demande_de_cle("x")).await,
        ),
        (
            "revocation",
            supprimer(&app, &format!("{CLES}/quelconque")).await,
        ),
        ("liste des webhooks", lire(&app, WEBHOOKS).await),
        (
            "creation de webhook",
            poster(
                &app,
                WEBHOOKS,
                json!({"url": "https://exemple.test/hook", "events": ["track.started"]}),
            )
            .await,
        ),
        (
            "test des webhooks",
            poster(&app, &format!("{WEBHOOKS}/test"), json!({})).await,
        ),
        ("greffons installes", lire(&app, INSTALLES).await),
    ] {
        assert_eq!(
            statut,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{nom} : une base en panne doit se dire, pas rendre une liste vide ni un succes \
             ({corps})"
        );
        assert_eq!(corps["error"], "storage_failure", "{nom} : {corps}");
    }
}

// ---------------------------------------------------------------------------
// 3. Un JSON illisible ne détruit pas ce qu'il y avait
// ---------------------------------------------------------------------------

#[tokio::test]
async fn un_contenu_illisible_est_refuse_sans_etre_ecrase_ni_recite() {
    let (app, etat) = serveur().await;
    let settings = SettingsRepo::with_backend(etat.backend.clone());

    // Une valeur corrompue qui contient une clef ressemblant à une vraie : si
    // le message d'erreur citait le contenu, le secret partirait dans la
    // réponse HTTP et dans le journal.
    const SECRET: &str = "tunedev_deadbeefdeadbeefdeadbeefdeadbeef";
    let corrompu = format!("[{{\"id\":\"a\",\"key\":\"{SECRET}\"");
    settings.set("developer_api_keys", &corrompu).unwrap();

    let (statut, corps, texte) = lire(&app, CLES).await;
    assert_eq!(statut, StatusCode::INTERNAL_SERVER_ERROR, "{corps}");
    assert!(
        !texte.contains(SECRET) && !texte.contains("deadbeef"),
        "la reponse d'erreur cite le contenu stocke — un secret sort par la porte de derriere :\n\
         {texte}"
    );

    // La création refuse aussi : c'est ici que la #2795 perdait tout, en
    // repartant d'une liste vide et en la réécrivant.
    let (statut, corps, _) = poster(&app, CLES, demande_de_cle("nouvelle")).await;
    assert_eq!(statut, StatusCode::INTERNAL_SERVER_ERROR, "{corps}");

    assert_eq!(
        settings.get("developer_api_keys").unwrap().as_deref(),
        Some(corrompu.as_str()),
        "la valeur d'origine a ete ecrasee : c'est exactement la perte que la #2795 decrit"
    );
}

// ---------------------------------------------------------------------------
// 4. Deux créations concurrentes sont toutes les deux conservées
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn des_creations_concurrentes_sont_toutes_conservees() {
    let (app, _etat) = serveur().await;

    const CREATIONS: usize = 24;

    let mut taches = Vec::new();
    for i in 0..CREATIONS {
        let app = app.clone();
        taches.push(tokio::spawn(async move {
            let (statut, corps, _) =
                poster(&app, CLES, demande_de_cle(&format!("outil {i}"))).await;
            assert_eq!(statut, StatusCode::CREATED, "creation {i} : {corps}");
            corps["id"].as_str().unwrap().to_string()
        }));
    }

    let mut identifiants = std::collections::HashSet::new();
    for t in taches {
        identifiants.insert(t.await.unwrap());
    }
    assert_eq!(
        identifiants.len(),
        CREATIONS,
        "des identifiants sont en double"
    );

    let (_, corps, _) = lire(&app, CLES).await;
    assert_eq!(
        corps["count"], CREATIONS,
        "des creations simultanees se sont ecrasees : {corps}"
    );

    // Chacune est bien celle qui a été rendue à son appelant.
    let listees: std::collections::HashSet<String> = corps["api_keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|k| k["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(listees, identifiants);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn des_webhooks_concurrents_sont_tous_conserves() {
    let (app, _etat) = serveur().await;

    const CREATIONS: usize = 16;

    let mut taches = Vec::new();
    for i in 0..CREATIONS {
        let app = app.clone();
        taches.push(tokio::spawn(async move {
            let (statut, corps, _) = poster(
                &app,
                WEBHOOKS,
                json!({
                    "url": format!("https://exemple.test/hook/{i}"),
                    "events": ["track.started"],
                }),
            )
            .await;
            assert_eq!(statut, StatusCode::CREATED, "webhook {i} : {corps}");
        }));
    }
    for t in taches {
        t.await.unwrap();
    }

    let (_, corps, _) = lire(&app, WEBHOOKS).await;
    assert_eq!(corps["count"], CREATIONS, "{corps}");
}

// ---------------------------------------------------------------------------
// 5. Le journal ne peut pas porter l'adresse d'un webhook
// ---------------------------------------------------------------------------

/// L'adresse d'un webhook Slack ou Discord **est** le jeton : son chemin donne
/// le droit de publier. `origine_seule` est ce qui permet de nommer une
/// destination dans une trace sans la livrer.
#[test]
fn une_adresse_de_webhook_ne_survit_pas_a_sa_reduction_en_origine() {
    use tune_server::routes::developer_api::origine_seule;

    assert_eq!(
        origine_seule("https://hooks.slack.com/services/T000/B000/XXXXsecretXXXX"),
        "https://hooks.slack.com"
    );
    assert_eq!(
        origine_seule("https://hote.test/chemin?jeton=secret#ancre"),
        "https://hote.test"
    );
    // Les identifiants glissés dans l'autorité disparaissent aussi.
    assert_eq!(
        origine_seule("https://rene:motdepasse@hote.test/x"),
        "https://hote.test"
    );
    assert_eq!(origine_seule("pas-une-url"), "(adresse illisible)");
    assert_eq!(origine_seule("https:///chemin"), "(adresse illisible)");
}
