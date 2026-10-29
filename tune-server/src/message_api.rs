//! Les phrases que l'API rend à l'utilisateur : un code stable, une clé, la
//! langue de l'interface (#7077).
//!
//! ## Le défaut
//!
//! Les routes composaient leurs refus en français, en dur :
//! `{"error": "réglage non enregistré : …"}`, `(400, "La file d'attente est
//! vide : …")`. Le client web, qui n'a que ce texte, le recopiait à l'écran :
//! un anglophone lisait du français (testeur Alex, 26/10/2026).
//!
//! ## Le contrat
//!
//! Un refus porte :
//!
//! * `error` et `code` : le **code stable**, en ASCII, que les clients
//!   comparent et que le web peut traduire lui-même ;
//! * `message` : la phrase, dans la langue de la requête (le `?lang=` puis
//!   l'`Accept-Language` que le web pose sur chaque appel, repli anglais) ;
//! * `message_key` : la clé de `i18n_server.json` dont elle vient. C'est le
//!   signal, pour le web, que `message` est **traduit** et peut s'afficher ;
//!   sans lui, il affiche un message générique et range le texte dans les
//!   détails ;
//! * `params` : les valeurs interpolées, pour qu'un client compose sa propre
//!   phrase ;
//! * `cause` (facultatif) : le texte technique — erreur de base, stderr d'une
//!   commande. Jamais présenté comme la phrase de l'interface.
//!
//! Un corps de succès qui porte une phrase (`message`) suit la même règle,
//! via [`avec_message`].
//!
//! La langue vient du task-local posé par
//! [`crate::routes::zones::textes_signal::avec_langue`] sur toutes les routes :
//! aucune route n'a besoin d'extraire ses en-têtes pour parler la bonne langue.
//! Hors requête (test qui appelle un gestionnaire directement), l'anglais.
//!
//! La garde `garde_francais_api` interdit qu'une chaîne française revienne
//! dans ces champs.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Map, Value, json};

/// La langue de la requête en cours, ou l'anglais hors requête.
pub fn langue() -> &'static str {
    crate::routes::zones::textes_signal::langue_de_la_requete().unwrap_or(crate::i18n::FALLBACK)
}

/// La phrase de `cle` dans `lang`, `{nom}` remplacé par sa valeur.
pub fn texte_en(lang: &str, cle: &str, params: &[(&str, String)]) -> String {
    let mut s = crate::i18n::t(lang, cle);
    for (nom, valeur) in params {
        s = s.replace(&format!("{{{nom}}}"), valeur);
    }
    s
}

/// La phrase de `cle` dans la langue de la requête en cours.
pub fn texte(cle: &str, params: &[(&str, String)]) -> String {
    texte_en(langue(), cle, params)
}

/// Pose `message`, `message_key` et `params` dans un corps JSON objet.
///
/// Le corps garde ses autres champs : un écran qui lit `racines`,
/// `confirm_purge_required` ou `valid` continue de les trouver.
pub fn avec_message(corps: Value, cle: &str, params: &[(&str, String)]) -> Value {
    avec_message_en(corps, langue(), cle, params)
}

/// [`avec_message`] dans une langue donnée.
pub fn avec_message_en(corps: Value, lang: &str, cle: &str, params: &[(&str, String)]) -> Value {
    let mut map = match corps {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    map.insert("message".into(), json!(texte_en(lang, cle, params)));
    map.insert("message_key".into(), json!(cle));
    if !params.is_empty() {
        let p: Map<String, Value> = params
            .iter()
            .map(|(nom, valeur)| ((*nom).to_string(), json!(valeur)))
            .collect();
        map.insert("params".into(), Value::Object(p));
    }
    Value::Object(map)
}

/// Le corps d'un refus : `error` = `code` = le code stable, plus la phrase.
pub fn corps_refus(code: &str, cle: &str, params: &[(&str, String)]) -> Value {
    avec_message(json!({ "error": code, "code": code }), cle, params)
}

/// Un refus complet, prêt à rendre.
pub fn refus(statut: StatusCode, code: &str, cle: &str, params: &[(&str, String)]) -> Response {
    (statut, Json(corps_refus(code, cle, params))).into_response()
}

/// Un refus qui garde sa cause technique dans `cause`.
pub fn refus_avec_cause(
    statut: StatusCode,
    code: &str,
    cle: &str,
    params: &[(&str, String)],
    cause: impl std::fmt::Display,
) -> Response {
    let mut corps = corps_refus(code, cle, params);
    corps["cause"] = json!(cause.to_string());
    (statut, Json(corps)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hors_requete_la_phrase_est_en_anglais_et_porte_sa_cle() {
        let corps = corps_refus("queue_empty", "api.playback.queueEmptyNothingToSave", &[]);
        assert_eq!(corps["error"], json!("queue_empty"));
        assert_eq!(corps["code"], json!("queue_empty"));
        assert_eq!(
            corps["message_key"],
            json!("api.playback.queueEmptyNothingToSave")
        );
        let m = corps["message"].as_str().unwrap();
        assert!(m.starts_with("The queue is empty"), "{m}");
    }

    #[test]
    fn chaque_langue_rend_sa_phrase_et_interpole_les_parametres() {
        let mut vues = std::collections::HashSet::new();
        for lang in crate::i18n::SUPPORTED {
            let corps = avec_message_en(
                json!({ "racines": [] }),
                lang,
                "api.network.shareHasLibraryFolders",
                &[("count", "3".into())],
            );
            let m = corps["message"].as_str().unwrap().to_string();
            assert!(m.contains('3'), "{lang} : {m}");
            assert!(!m.contains("{count}"), "{lang} : jeton non remplacé : {m}");
            assert_eq!(corps["params"]["count"], json!("3"));
            assert_eq!(corps["racines"], json!([]), "l'enveloppe survit");
            vues.insert(m);
        }
        assert_eq!(vues.len(), crate::i18n::SUPPORTED.len(), "{vues:?}");
    }

    #[tokio::test]
    async fn dans_une_requete_la_langue_de_l_interface_gagne() {
        use axum::Router;
        use axum::body::Body;
        use axum::http::Request;
        use axum::routing::get;
        use tower::ServiceExt;

        let app = Router::new()
            .route(
                "/r",
                get(|| async {
                    refus(
                        StatusCode::BAD_REQUEST,
                        "queue_empty",
                        "api.playback.queueEmptyNothingToSave",
                        &[],
                    )
                }),
            )
            .layer(axum::middleware::from_fn(
                crate::routes::zones::textes_signal::avec_langue,
            ));
        let resp = app
            .oneshot(
                Request::get("/r")
                    .header("accept-language", "de-DE,de;q=0.9")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let octets = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let corps: Value = serde_json::from_slice(&octets).unwrap();
        assert_eq!(
            corps["message"],
            json!(texte_en("de", "api.playback.queueEmptyNothingToSave", &[]))
        );
        assert_ne!(
            corps["message"],
            json!(texte_en("en", "api.playback.queueEmptyNothingToSave", &[]))
        );
    }
}
