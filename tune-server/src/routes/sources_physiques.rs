//! #5065 — les sources physiques (CD, entrées…), agrégées par le cœur.
//!
//! * `GET  /api/v1/sources`            — la liste du registre commun ;
//! * `POST /api/v1/sources/{id}/jouer` — `{ "zone_id": 3, "piste": 5 }`,
//!   délégué au greffon propriétaire de la source.
//!
//! Le registre lui-même est `tune_core::sources_physiques` ; les greffons
//! natifs y inscrivent leurs sources par l'orchestrateur de leurs
//! `HostServices`. L'événement `sources.changed` part du registre.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;
use tune_core::sources_physiques::{DemandeJouer, ErreurJouer, RegistreSources};

pub fn router<S>(registre: Arc<RegistreSources>) -> Router<S> {
    Router::new()
        .route("/", get(lister))
        .route("/{id}/jouer", post(jouer))
        .with_state(registre)
}

async fn lister(State(registre): State<Arc<RegistreSources>>) -> Response {
    Json(registre.lister()).into_response()
}

async fn jouer(
    State(registre): State<Arc<RegistreSources>>,
    Path(id): Path<String>,
    Json(demande): Json<DemandeJouer>,
) -> Response {
    match registre.jouer(&id, demande).await {
        Ok(corps) => Json(corps).into_response(),
        Err(ErreurJouer::Inconnue) => (
            StatusCode::NOT_FOUND,
            Json(json!({
                "error": "source_inconnue",
                "message": format!("Aucune source « {id} » sur ce serveur."),
            })),
        )
            .into_response(),
        Err(ErreurJouer::NonJouable { greffon }) => (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "lecture_non_prise_en_charge",
                "message": format!("Le greffon « {greffon} » ne sait pas jouer la source « {id} »."),
            })),
        )
            .into_response(),
        Err(ErreurJouer::Refus(r)) => (
            StatusCode::from_u16(r.statut).unwrap_or(StatusCode::CONFLICT),
            Json(json!({ "error": r.motif, "message": r.message })),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use axum::body::Body;
    use axum::http::Request;
    use serde_json::Value;
    use tower::ServiceExt;
    use tune_core::sources_physiques::{EtatSource, JoueurSource, RefusSource, Source, TypeSource};

    async fn appel(
        r: Router<()>,
        methode: &str,
        uri: &str,
        corps: Option<Value>,
    ) -> (StatusCode, Value) {
        let req = Request::builder().method(methode).uri(uri);
        let req = match corps {
            Some(c) => req
                .header("content-type", "application/json")
                .body(Body::from(c.to_string()))
                .unwrap(),
            None => req.body(Body::empty()).unwrap(),
        };
        let rep = r.oneshot(req).await.unwrap();
        let code = rep.status();
        let octets = axum::body::to_bytes(rep.into_body(), 1 << 20)
            .await
            .unwrap();
        (code, serde_json::from_slice(&octets).unwrap_or(Value::Null))
    }

    fn source(etat: EtatSource) -> Source {
        Source {
            id: "entree:factice".into(),
            genre: TypeSource::Entree,
            greffon: "factice".into(),
            nom: "Factice".into(),
            etat,
            detail: json!({ "frequence": 48000 }),
        }
    }

    struct Joueur;
    #[async_trait]
    impl JoueurSource for Joueur {
        async fn jouer(&self, id: &str, d: DemandeJouer) -> Result<Value, RefusSource> {
            if d.piste == Some(99) {
                return Err(RefusSource {
                    statut: 409,
                    motif: "aucun_disque".into(),
                    message: "Le lecteur est vide.".into(),
                });
            }
            Ok(json!({ "source": id, "zone_id": d.zone_id }))
        }
    }

    /// Témoin : `GET /sources` suit l'inscription, la mise à jour et le
    /// retrait d'un greffon factice.
    #[tokio::test]
    async fn get_sources_suit_le_registre() {
        let reg = Arc::new(RegistreSources::new());
        let (code, v) = appel(router(reg.clone()), "GET", "/", None).await;
        assert_eq!((code, v), (StatusCode::OK, json!([])));

        reg.inscrire(source(EtatSource::Silence), None).unwrap();
        let (_, v) = appel(router(reg.clone()), "GET", "/", None).await;
        assert_eq!(
            v,
            json!([{ "id": "entree:factice", "type": "entree", "greffon": "factice",
                     "nom": "Factice", "etat": "silence", "detail": { "frequence": 48000 } }])
        );
        reg.mettre_a_jour(source(EtatSource::Signal));
        let (_, v) = appel(router(reg.clone()), "GET", "/", None).await;
        assert_eq!(v[0]["etat"], "signal");
        reg.retirer("factice", "entree:factice");
        let (_, v) = appel(router(reg.clone()), "GET", "/", None).await;
        assert_eq!(v, json!([]));
    }

    /// Témoin : `jouer` délègue ; inconnue → 404 ; sans joueur → 409 ;
    /// refus du greffon → son statut et son motif.
    #[tokio::test]
    async fn jouer_delegue_404_et_409() {
        let reg = Arc::new(RegistreSources::new());
        let corps = json!({ "zone_id": 3 });
        let (code, v) = appel(
            router(reg.clone()),
            "POST",
            "/inconnue/jouer",
            Some(corps.clone()),
        )
        .await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{v}");
        assert_eq!(v["error"], "source_inconnue");

        reg.inscrire(source(EtatSource::Signal), None).unwrap();
        let (code, v) = appel(
            router(reg.clone()),
            "POST",
            "/entree:factice/jouer",
            Some(corps.clone()),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"], "lecture_non_prise_en_charge");

        reg.inscrire(source(EtatSource::Signal), Some(Arc::new(Joueur)))
            .unwrap();
        let (code, v) = appel(
            router(reg.clone()),
            "POST",
            "/entree:factice/jouer",
            Some(corps),
        )
        .await;
        assert_eq!(code, StatusCode::OK, "{v}");
        assert_eq!(v, json!({ "source": "entree:factice", "zone_id": 3 }));

        let (code, v) = appel(
            router(reg.clone()),
            "POST",
            "/entree:factice/jouer",
            Some(json!({ "zone_id": 3, "piste": 99 })),
        )
        .await;
        assert_eq!(code, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"], "aucun_disque");
    }
}
