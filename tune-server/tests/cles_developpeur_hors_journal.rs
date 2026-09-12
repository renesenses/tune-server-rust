//! Aucune clef d'API développeur n'atteint le journal — même tronquée (#2795).
//!
//! ## Pourquoi cet essai existe
//!
//! Les clefs du Developer API sont stockées **en clair** dans la table
//! `settings`, et rendues une seule fois à leur créateur. Le seul rempart qui
//! reste est qu'elles ne fuient nulle part ailleurs. Un `info!(key = %…)` ajouté
//! un jour de mise au point suffirait à les répandre dans un journal que
//! l'utilisateur nous envoie en pièce jointe d'un rapport de bug.
//!
//! L'essai lit donc le journal **au niveau TRACE** — tout ce que le serveur
//! sait dire — pendant qu'une clef est créée, listée, révoquée, puis pendant
//! que les chemins d'erreur (JSON corrompu, base absente) se déclenchent. La
//! clef ne doit apparaître nulle part, ni entière, ni par un fragment.
//!
//! ## Pourquoi un binaire de test à lui seul, et un seul essai dedans
//!
//! Même leçon que `panne_sql_journalisee.rs` : `tracing` met en cache, **pour
//! tout le processus**, la décision « ce point d'appel intéresse-t-il
//! quelqu'un ? ». Un abonné posé au milieu d'un binaire qui lance des essais en
//! parallèle rend des captures vides sans prévenir. Ici l'abonné est global,
//! installé en premier, et ce binaire ne contient qu'un essai.
//!
//! ⚠️ `tune-server` porte `autotests = false` : sans sa cible `[[test]]` dans
//! `tune-server/Cargo.toml`, ce fichier ne serait jamais compilé.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::license::Tier;

#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn texte(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

const CLES: &str = "/api/v1/developer/api-keys";
const WEBHOOKS: &str = "/api/v1/developer/webhooks";

async fn envoyer(app: &axum::Router, requete: Request<Body>) -> (StatusCode, Value) {
    let reponse = app.clone().oneshot(requete).await.unwrap();
    let statut = reponse.status();
    let octets = axum::body::to_bytes(reponse.into_body(), usize::MAX)
        .await
        .unwrap();
    let corps: Value = serde_json::from_slice(&octets).unwrap_or(json!(null));
    (statut, corps)
}

#[tokio::test]
async fn aucune_clef_ni_adresse_de_webhook_n_atteint_le_journal() {
    let capture = JournalCapture::default();
    // TRACE : la revendication la plus forte possible — même le journal le plus
    // bavard que ce serveur sache produire ne porte pas de secret.
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un essai : l'abonne global est libre");

    let etat = tune_server::state::AppState::new(":memory:", 0, Default::default())
        .expect("base en memoire");
    etat.license.update_from_server(Tier::Premium, None).await;
    let app = tune_server::routes::router(etat.clone());

    // --- Le chemin nominal : création, listage, révocation ------------------

    let (statut, corps) = envoyer(
        &app,
        Request::post(CLES)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({"name": "outil", "scopes": ["read"]}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "creation : {corps}");
    let cle = corps["key"]
        .as_str()
        .expect("la clef est rendue")
        .to_string();
    let id = corps["id"].as_str().unwrap().to_string();

    // Une adresse dont le CHEMIN est le secret, comme chez Slack.
    const CHEMIN_SECRET: &str = "T000secretB000";
    let (statut, corps) = envoyer(
        &app,
        Request::post(WEBHOOKS)
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "url": format!("https://exemple.test/services/{CHEMIN_SECRET}"),
                    "events": ["track.started"],
                })
                .to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::CREATED, "webhook : {corps}");

    let (statut, _) = envoyer(&app, Request::get(CLES).body(Body::empty()).unwrap()).await;
    assert_eq!(statut, StatusCode::OK);

    let (statut, _) = envoyer(
        &app,
        Request::delete(format!("{CLES}/{id}"))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(statut, StatusCode::OK);

    // --- Les chemins d'erreur, ceux que la #2795 ajoute ---------------------

    // JSON corrompu qui CONTIENT une clef : si le message d'erreur citait la
    // valeur, elle passerait dans la trace.
    const SECRET_STOCKE: &str = "tunedev_cafebabecafebabecafebabecafebabe";
    let settings = SettingsRepo::with_backend(etat.backend.clone());
    settings
        .set(
            "developer_api_keys",
            &format!("[{{\"id\":\"a\",\"key\":\"{SECRET_STOCKE}\""),
        )
        .unwrap();

    let (statut, _) = envoyer(&app, Request::get(CLES).body(Body::empty()).unwrap()).await;
    assert_eq!(
        statut,
        StatusCode::INTERNAL_SERVER_ERROR,
        "le contenu illisible doit produire l'erreur dont on lit le journal"
    );

    // Puis la panne de base, l'autre branche journalisée.
    etat.backend.execute_batch("DROP TABLE settings").unwrap();
    let (statut, _) = envoyer(&app, Request::get(CLES).body(Body::empty()).unwrap()).await;
    assert_eq!(statut, StatusCode::INTERNAL_SERVER_ERROR);

    // --- Le verdict --------------------------------------------------------

    let journal = capture.texte();

    // Témoin : la capture n'est pas vide, sinon l'essai passerait pour de
    // mauvaises raisons (c'est le piège que `tracing` tend).
    assert!(
        journal.contains("developer_api_key_created"),
        "le journal capture est vide ou muet : l'essai ne prouve rien.\n{journal}"
    );
    assert!(
        journal.contains("developer_api_stockage_en_echec"),
        "les branches d'erreur n'ont rien journalise : rien n'est observe.\n{journal}"
    );

    // Le verdict lui-même. Le fragment compte autant que la clef entière :
    // « même tronquée » n'est pas une figure de style, huit caractères de
    // préfixe suffisent à corréler deux journaux.
    assert!(
        !journal.contains(&cle),
        "une clef d'API developpeur est apparue dans le journal"
    );
    assert!(
        !journal.contains(&cle[8..24]),
        "un FRAGMENT de clef est apparu dans le journal — meme tronquee, une \
         clef n'a rien a y faire"
    );
    assert!(
        !journal.contains(SECRET_STOCKE),
        "la valeur stockee illisible a ete recopiee dans le journal"
    );
    assert!(
        !journal.contains("cafebabe"),
        "un fragment de la valeur stockee a ete recopie dans le journal"
    );
    assert!(
        !journal.contains(CHEMIN_SECRET),
        "le chemin d'une adresse de webhook est dans le journal — chez Slack ou \
         Discord, ce chemin EST le jeton"
    );
    // …mais l'origine, elle, doit y être : une trace qui ne dit plus rien du
    // tout ne vaut pas mieux qu'une trace qui en dit trop.
    assert!(
        journal.contains("https://exemple.test"),
        "l'origine du webhook doit rester lisible pour le diagnostic.\n{journal}"
    );
}
