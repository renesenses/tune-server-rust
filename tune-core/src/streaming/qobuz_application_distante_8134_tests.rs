//! #8134 — une application servie par la configuration distante et refusée
//! par Qobuz ne doit ni faire tomber la session, ni passer pour un mauvais mot
//! de passe. Faux Qobuz sur la boucle locale (aucun appel réel, identifiants
//! factices) qui rejoue la séquence du journal : 403 sur `user/get`, nouvelle
//! application distante, puis 401 « Invalid username/email and password
//! combination » sur `user/login` avec elle.

use super::*;
use axum::response::IntoResponse;
use serde_json::json;
use std::sync::{Arc, Mutex as StdMutex};

const ANCIENNE: &str = "app-ancienne";
const NOUVELLE: &str = "app-nouvelle";
const MAUVAIS_MOT_DE_PASSE: &str = "Invalid username/email and password combination";
const APPEL_ANONYME: &str = "User authentication is required";

/// Ce que le faux Qobuz sert, et le journal des `app_id` reçus par `user/login`.
#[derive(Default)]
struct Etat {
    /// L'application que sert la configuration distante.
    distante: String,
    /// Les applications dont `user/login` et `user/get` acceptent la session.
    acceptees: Vec<String>,
    /// Les applications refusées sous la forme « appel anonyme » (01/11) ;
    /// les autres refusées le sont sous la forme « mauvais mot de passe ».
    refusees_comme_application: Vec<String>,
    /// `user/get` répond 403 quelle que soit l'application.
    user_get_403: bool,
    connexions: Vec<String>,
}

fn refus(message: &str) -> axum::response::Response {
    (
        axum::http::StatusCode::UNAUTHORIZED,
        axum::Json(json!({"status": "error", "code": 401, "message": message})),
    )
        .into_response()
}

async fn faux_qobuz(etat: Etat) -> (QobuzService, Arc<StdMutex<Etat>>) {
    let etat = Arc::new(StdMutex::new(etat));
    let e = etat.clone();
    let app = axum::Router::new().fallback(move |uri: axum::http::Uri| {
        let e = e.clone();
        async move {
            let mut s = e.lock().unwrap();
            let app_id = uri
                .query()
                .unwrap_or("")
                .split('&')
                .find_map(|kv| kv.strip_prefix("app_id="))
                .unwrap_or("")
                .to_string();
            let acceptee = s.acceptees.contains(&app_id);
            match uri.path() {
                "/config" => axum::Json(json!({"qobuz": {
                    "app_id": s.distante,
                    "app_secret": format!("secret-{}", s.distante),
                }}))
                .into_response(),
                "/user/login" => {
                    s.connexions.push(app_id.clone());
                    if acceptee {
                        axum::Json(json!({
                            "user_auth_token": format!("jeton-{app_id}"),
                            "user": {"display_name": "Testeur", "credential": {"label": "Studio"}},
                        }))
                        .into_response()
                    } else if s.refusees_comme_application.contains(&app_id) {
                        refus(APPEL_ANONYME)
                    } else {
                        refus(MAUVAIS_MOT_DE_PASSE)
                    }
                }
                "/user/get" if s.user_get_403 => axum::http::StatusCode::FORBIDDEN.into_response(),
                "/user/get" if acceptee => axum::Json(json!({"id": 1})).into_response(),
                "/user/get" => refus(APPEL_ANONYME),
                _ => axum::http::StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    let ecoute = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let adresse = ecoute.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(ecoute, app).await;
    });
    let base = format!("http://{adresse}");
    let svc = QobuzService::avec_bases_de_test(
        String::new(),
        String::new(),
        BasesQobuz {
            api: base.clone(),
            proxy: base.clone(),
            config_distante: format!("{base}/config"),
        },
    );
    (svc, etat)
}

fn identifiants() -> serde_json::Value {
    json!({"username": "testeur@example.org", "password": "mot-de-passe-juste"})
}

/// Une session ouverte avec l'ancienne application, puis la configuration
/// distante qui en sert une nouvelle que Qobuz refuse.
async fn session_ouverte_puis_application_distante_refusee() -> (QobuzService, Arc<StdMutex<Etat>>)
{
    let (mut svc, etat) = faux_qobuz(Etat {
        distante: ANCIENNE.into(),
        acceptees: vec![ANCIENNE.into()],
        ..Default::default()
    })
    .await;
    svc.authenticate(&identifiants())
        .await
        .expect("première connexion, avec l'ancienne application");
    {
        let mut s = etat.lock().unwrap();
        s.distante = NOUVELLE.into();
        s.connexions.clear();
    }
    (svc, etat)
}

/// Le cas du journal : 403 sur `user/get`, reconnexion automatique, nouvelle
/// application distante refusée en « mauvais mot de passe ».
#[tokio::test]
async fn le_403_puis_l_application_distante_refusee_garde_la_session_8134() {
    let (mut svc, etat) = session_ouverte_puis_application_distante_refusee().await;
    etat.lock().unwrap().user_get_403 = true;

    let issue = svc.refresh_if_needed().await;

    assert!(
        svc.auth_status().await.authenticated && !svc.session_expired(),
        "identifiants refusés alors que l'identifiant précédent était accepté : \
         la session est tombée ({issue:?}, connexions {:?})",
        etat.lock().unwrap().connexions
    );
    assert_eq!(
        etat.lock().unwrap().connexions,
        [NOUVELLE, ANCIENNE],
        "la nouvelle application est essayée, puis une seule fois la précédente"
    );
    assert_eq!(
        svc.app_id, ANCIENNE,
        "la précédente, acceptée, est conservée"
    );
}

/// L'utilisateur ressaisit son mot de passe : même séquence, même issue.
#[tokio::test]
async fn ressaisir_le_mot_de_passe_reussit_avec_l_application_precedente_8134() {
    let (mut svc, etat) = session_ouverte_puis_application_distante_refusee().await;

    let statut = svc.authenticate(&identifiants()).await;

    assert!(
        statut.as_ref().is_ok_and(|s| s.authenticated),
        "identifiants refusés alors que l'identifiant précédent était accepté : {statut:?}"
    );
    assert_eq!(etat.lock().unwrap().connexions, [NOUVELLE, ANCIENNE]);
    assert_eq!(svc.app_id, ANCIENNE);
    // La suivante essaie encore la distante : si Qobuz finit par l'accepter,
    // elle est adoptée sans redémarrage.
    etat.lock().unwrap().acceptees.push(NOUVELLE.into());
    svc.authenticate(&identifiants())
        .await
        .expect("distante acceptée");
    assert_eq!(svc.app_id, NOUVELLE);
}

/// Les deux applications refusent le mot de passe : c'est bien lui.
#[tokio::test]
async fn les_deux_applications_refusent_le_mot_de_passe_c_est_un_refus_d_identifiants_8134() {
    let (mut svc, etat) = session_ouverte_puis_application_distante_refusee().await;
    etat.lock().unwrap().acceptees.clear();

    let e = svc.authenticate(&identifiants()).await.unwrap_err();

    assert!(matches!(e, TuneError::Credentials(_)), "{e:?}");
    assert_eq!(etat.lock().unwrap().connexions, [NOUVELLE, ANCIENNE]);
}

/// La précédente est refusée comme application : Qobuz refuse l'application,
/// le mot de passe n'est pas en cause.
#[tokio::test]
async fn la_precedente_refusee_comme_application_donne_un_refus_de_l_application_8134() {
    let (mut svc, etat) = session_ouverte_puis_application_distante_refusee().await;
    {
        let mut s = etat.lock().unwrap();
        s.acceptees.clear();
        s.refusees_comme_application.push(ANCIENNE.into());
    }

    let e = svc.authenticate(&identifiants()).await.unwrap_err();

    assert!(
        matches!(e, TuneError::AppRefused(_)),
        "refus de l'application présenté comme : {e:?}"
    );
}

/// Au redémarrage, la session enregistrée est éprouvée avec l'application qui
/// l'a ouverte : la nouvelle application distante ne l'efface pas.
#[tokio::test]
async fn la_session_restauree_n_est_pas_effacee_par_l_application_distante_8134() {
    let (mut svc, etat) = faux_qobuz(Etat {
        distante: NOUVELLE.into(),
        acceptees: vec![ANCIENNE.into()],
        ..Default::default()
    })
    .await;
    assert!(svc.restore_tokens(&json!({
        "user_auth_token": "jeton-enregistre",
        "username": "Testeur",
        "app_id": ANCIENNE,
        "app_secret": "secret-ancienne",
    })));

    svc.post_restore().await;

    assert!(
        svc.auth_status().await.authenticated && !svc.session_expired(),
        "la session restaurée est tombée avec l'application distante"
    );
    assert_eq!(svc.app_id, ANCIENNE);
    assert!(etat.lock().unwrap().connexions.is_empty());
}

/// Sans session antérieure, l'application distante est adoptée comme avant, et
/// un refus sans repli garde sa lecture habituelle.
#[tokio::test]
async fn sans_session_anterieure_l_application_distante_est_adoptee_8134() {
    let (mut svc, etat) = faux_qobuz(Etat {
        distante: NOUVELLE.into(),
        ..Default::default()
    })
    .await;
    let e = svc.authenticate(&identifiants()).await.unwrap_err();
    assert!(matches!(e, TuneError::Credentials(_)), "{e:?}");
    assert_eq!(etat.lock().unwrap().connexions, [NOUVELLE], "aucun repli");

    etat.lock().unwrap().acceptees.push(NOUVELLE.into());
    svc.authenticate(&identifiants()).await.expect("connexion");
    assert_eq!(svc.app_id, NOUVELLE);
}
