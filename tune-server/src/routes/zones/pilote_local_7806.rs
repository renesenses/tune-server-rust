//! #7806 — le pilote réellement emprunté par une zone locale Windows, et les
//! sorties ASIO présentes qu'aucune zone ne porte.
//!
//! Journal du testeur (fil 2248, rc10, Denafrips Pontus) : ASIO choisi, l'hôte
//! ASIO ouvert, la sortie `local:DENAFRIPS USB DAC` enregistrée mais SANS zone
//! (`local_audio_zone_manual_creation_required`). Le réexamen périodique, qui
//! énumère en WASAPI depuis #3245, remettait « This Computer »
//! (`local:Haut-parleurs`) en ligne, et toutes les lectures DSD y partaient :
//! WASAPI, DSD converti en PCM 176,4/352,8 kHz. Rien ne disait que cette zone
//! n'était pas en ASIO.
//!
//! Deux informations, posées par le serveur :
//!
//! - `pilote_local` (`"wasapi"` ou `"asio"`) sur chaque zone locale Windows
//!   dont la sortie est au registre — toujours, pas seulement quand elle sort
//!   du backend choisi (`hors_backend_choisi`, #5353) ;
//! - `GET /zones/asio-sans-zone` : les sorties ASIO présentes qu'aucune zone
//!   VISIBLE ne porte. Le client y accroche « Créer la zone ASIO », qui passe
//!   par `POST /zones` (une zone masquée y est ressuscitée).

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use tune_core::db::zone_repo::ZoneRepo;

use crate::state::AppState;

/// LA règle, pure : le pilote à annoncer pour une zone locale.
///
/// `backend_sortie` est `LocalOutput::audio_backend` (rectifié par l'hôte
/// d'origine, #1770), `None` quand la sortie n'est pas connue. Hors Windows,
/// rien : ni WASAPI ni ASIO n'y existent. Sous Windows, toute valeur autre
/// qu'`asio` (`auto`, `wasapi`) ouvre l'hôte par défaut, WASAPI
/// (`select_host`).
pub(crate) fn pilote_effectif(
    backend_sortie: Option<&str>,
    on_windows: bool,
) -> Option<&'static str> {
    if !on_windows {
        return None;
    }
    let b = backend_sortie?.trim();
    if b.is_empty() {
        return None;
    }
    Some(if b.eq_ignore_ascii_case("asio") {
        "asio"
    } else {
        "wasapi"
    })
}

/// Pose `pilote_local` sur la charge utile d'une zone. Rien n'est posé quand
/// le pilote n'est pas connu : une absence n'est pas un « WASAPI ».
pub(crate) fn injecter_pilote_local(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    backend_sortie: Option<&str>,
    on_windows: bool,
) {
    if let Some(p) = pilote_effectif(backend_sortie, on_windows) {
        obj.insert("pilote_local".into(), json!(p));
    }
}

/// Les sorties ASIO `(device_id, nom)` qu'aucune zone visible ne porte —
/// règle pure. `portee` dit si une zone VISIBLE porte ce `device_id` ; une
/// zone supprimée (masquée) ne compte pas : l'utilisateur ne la voit plus.
pub(crate) fn sans_zone(
    sorties_asio: Vec<(String, String)>,
    portee: impl Fn(&str) -> bool,
) -> Vec<(String, String)> {
    sorties_asio
        .into_iter()
        .filter(|(id, _)| !portee(id))
        .collect()
}

/// `GET /zones/asio-sans-zone`
pub(crate) async fn sorties_asio_sans_zone_route(State(state): State<AppState>) -> Response {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let presentes = super::rattacher_asio_6120::sorties_asio(&state).await;
    let orphelines = sans_zone(presentes, |id| {
        repo.get_by_device_id(id).ok().flatten().is_some() && !repo.is_device_hidden(id)
    });
    if !orphelines.is_empty() {
        tracing::debug!(
            sorties = ?orphelines.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
            "local_audio_asio_output_without_zone"
        );
    }
    let sorties: Vec<serde_json::Value> = orphelines
        .into_iter()
        .map(|(device_id, nom)| json!({"device_id": device_id, "name": nom, "backend": "asio"}))
        .collect();
    Json(json!({ "sorties": sorties })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_du_pilote() {
        // Windows : ASIO reste ASIO, tout le reste ouvre WASAPI.
        assert_eq!(pilote_effectif(Some("asio"), true), Some("asio"));
        assert_eq!(pilote_effectif(Some(" ASIO "), true), Some("asio"));
        assert_eq!(pilote_effectif(Some("wasapi"), true), Some("wasapi"));
        assert_eq!(pilote_effectif(Some("auto"), true), Some("wasapi"));
        // Inconnu : rien.
        assert_eq!(pilote_effectif(None, true), None);
        assert_eq!(pilote_effectif(Some("  "), true), None);
        // Hors Windows : jamais.
        assert_eq!(pilote_effectif(Some("asio"), false), None);
        assert_eq!(pilote_effectif(Some("auto"), false), None);
    }

    #[test]
    fn sans_zone_ecarte_les_sorties_portees() {
        let sorties = vec![
            ("local:ASIO4ALL v2".to_string(), "ASIO4ALL v2".to_string()),
            (
                "local:DENAFRIPS USB DAC".to_string(),
                "DENAFRIPS USB DAC".to_string(),
            ),
        ];
        let reste = sans_zone(sorties, |id| id == "local:ASIO4ALL v2");
        assert_eq!(
            reste,
            vec![(
                "local:DENAFRIPS USB DAC".to_string(),
                "DENAFRIPS USB DAC".to_string()
            )]
        );
    }

    /// Le journal du fil 2248, rejoué : « This Computer » sur les
    /// haut-parleurs (nom WASAPI énuméré par le réexamen, ASIO choisi), une
    /// zone sur ASIO4ALL, et le Denafrips enregistré sans zone.
    ///
    /// - `GET /zones` : `pilote_local` dit WASAPI pour « This Computer » et
    ///   ASIO pour ASIO4ALL (Windows simulé par la règle pure, le banc tourne
    ///   sous Linux : on vérifie ici que la valeur injectée suit la sortie) ;
    /// - `GET /zones/asio-sans-zone` nomme le Denafrips, et lui seul ;
    /// - après `POST /zones` sur ce `device_id`, il n'est plus nommé.
    #[cfg(feature = "local-audio")]
    #[tokio::test]
    async fn le_denafrips_sans_zone_est_signale_puis_ne_l_est_plus() {
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use serde_json::Value;
        use tower::ServiceExt;
        use tune_core::db::settings_repo::SettingsRepo;
        use tune_core::outputs::local::LocalOutput;

        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        SettingsRepo::with_backend(state.backend.clone())
            .set("local_audio_backend", "asio")
            .unwrap();
        let zones = ZoneRepo::with_backend(state.backend.clone());
        zones
            .create("This Computer", Some("local"), Some("local:Haut-parleurs"))
            .unwrap();
        zones
            .create("ASIO4ALL v2", Some("local"), Some("local:ASIO4ALL v2"))
            .unwrap();
        {
            let mut registre = state.outputs.lock().await;
            registre.register(Box::new(
                LocalOutput::with_options_and_endpoint("Haut-parleurs".into(), None, false, "asio")
                    .with_origin_host("WASAPI"),
            ));
            for nom in ["ASIO4ALL v2", "DENAFRIPS USB DAC"] {
                registre.register(Box::new(
                    LocalOutput::with_options_and_endpoint(nom.into(), None, true, "asio")
                        .with_origin_host("ASIO"),
                ));
            }
        }
        let app = crate::routes::router(state.clone());
        let get = |uri: &'static str| {
            let app = app.clone();
            async move {
                let r = app
                    .oneshot(Request::get(uri).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(r.status(), StatusCode::OK, "{uri}");
                let b = to_bytes(r.into_body(), 1 << 22).await.unwrap();
                serde_json::from_slice::<Value>(&b).unwrap()
            }
        };

        // Le pilote suit la sortie de CHAQUE zone (règle câblée sous Windows).
        let liste = get("/api/v1/zones").await;
        let liste = liste.as_array().cloned().unwrap();
        let backend_de = |dev: &str| {
            liste
                .iter()
                .find(|z| z["output_device_id"] == dev)
                .and_then(|z| z["backend_sortie"].as_str().map(str::to_string))
        };
        assert_eq!(
            pilote_effectif(backend_de("local:Haut-parleurs").as_deref(), true),
            Some("wasapi"),
            "« This Computer » sous ASIO joue en WASAPI : {liste:?}"
        );
        assert_eq!(
            pilote_effectif(backend_de("local:ASIO4ALL v2").as_deref(), true),
            Some("asio")
        );
        // Ce binaire-ci n'est pas Windows : le champ n'est pas posé.
        if !cfg!(target_os = "windows") {
            assert!(liste.iter().all(|z| z.get("pilote_local").is_none()));
        }

        let orphelines = get("/api/v1/zones/asio-sans-zone").await;
        assert_eq!(
            orphelines["sorties"],
            json!([{"device_id": "local:DENAFRIPS USB DAC", "name": "DENAFRIPS USB DAC", "backend": "asio"}]),
            "le DAC ASIO sans zone est signalé, ASIO4ALL (qui a sa zone) et les haut-parleurs (WASAPI) non"
        );

        // Une zone supprimée (masquée) ne compte pas comme une zone.
        let id = zones
            .create(
                "DENAFRIPS USB DAC",
                Some("local"),
                Some("local:DENAFRIPS USB DAC"),
            )
            .unwrap();
        assert_eq!(
            get("/api/v1/zones/asio-sans-zone").await["sorties"],
            json!([])
        );
        zones.delete(id).unwrap();
        assert_eq!(
            get("/api/v1/zones/asio-sans-zone").await["sorties"][0]["device_id"],
            "local:DENAFRIPS USB DAC",
            "une zone supprimée laisse le DAC sans zone"
        );
    }

    /// Le câblage réel : la pose commune aux trois charges utiles de zone
    /// (`GET /zones`, `GET /zones/{id}`, réponse de lecture) porte
    /// `pilote_local` sous Windows, à côté de `backend_sortie`. Sans le
    /// correctif, le champ n'existe sur aucune d'elles.
    #[test]
    fn la_pose_commune_des_zones_porte_le_pilote_sous_windows() {
        use super::super::backend_de_la_sortie_5353::injecter_backend_de_sortie_sur;
        // « This Computer » sous ASIO : sortie rectifiée en WASAPI (#1770).
        let mut obj = serde_json::Map::new();
        injecter_backend_de_sortie_sur(&mut obj, "asio", Some("wasapi"), true);
        assert_eq!(obj["pilote_local"], "wasapi");
        assert_eq!(obj["hors_backend_choisi"], true);
        // Le Denafrips, une fois sa zone créée.
        let mut obj = serde_json::Map::new();
        injecter_backend_de_sortie_sur(&mut obj, "asio", Some("asio"), true);
        assert_eq!(obj["pilote_local"], "asio");
        // Machine en `auto` : WASAPI, sans alerte.
        let mut obj = serde_json::Map::new();
        injecter_backend_de_sortie_sur(&mut obj, "auto", Some("auto"), true);
        assert_eq!(obj["pilote_local"], "wasapi");
        assert_eq!(obj["hors_backend_choisi"], false);
        // Hors Windows ou sortie inconnue : pas de champ.
        let mut obj = serde_json::Map::new();
        injecter_backend_de_sortie_sur(&mut obj, "auto", Some("auto"), false);
        assert!(obj.get("pilote_local").is_none());
        let mut obj = serde_json::Map::new();
        injecter_backend_de_sortie_sur(&mut obj, "asio", None, true);
        assert!(obj.is_empty());
    }

    #[test]
    fn injecter_pose_le_pilote_sous_windows_seulement() {
        let mut obj = serde_json::Map::new();
        injecter_pilote_local(&mut obj, Some("auto"), true);
        assert_eq!(obj["pilote_local"], "wasapi");
        let mut obj = serde_json::Map::new();
        injecter_pilote_local(&mut obj, Some("asio"), false);
        assert!(obj.get("pilote_local").is_none());
        let mut obj = serde_json::Map::new();
        injecter_pilote_local(&mut obj, None, true);
        assert!(obj.get("pilote_local").is_none());
    }
}
