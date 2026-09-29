//! #5353 — le backend que le chemin du signal NOMME pour une zone locale.
//!
//! Le panneau tirait son transport (« ASIO (exclusive) », « WASAPI (shared —
//! Windows mixer) »…) d'une seule valeur, globale au processus :
//! `active_backend_name(display_audio_backend())`, c'est-à-dire
//! `OBSERVED_BACKEND` (`tune-core/src/outputs/local/etat_backend.rs`) avec le
//! réglage en repli. Cette valeur dit ce que le dernier `select_host` a ouvert,
//! pas ce qu'ouvre LA sortie de la zone affichée.
//!
//! Or, ASIO configuré, une machine porte des sorties de deux hôtes : les
//! pilotes ASIO enregistrés au démarrage, et les noms WASAPI (« Speakers »)
//! que le rescan à chaud énumère en WASAPI forcé (`background.rs`,
//! `scan_backend`) et enregistre. Ceux-là s'ouvrent en WASAPI :
//! `LocalOutput::with_origin_host` a rectifié leur backend
//! (`tune_core::config::openable_local_backend`, #1770). Le rescan ne note pas
//! le backend observé (#4667) : `OBSERVED_BACKEND` reste à « ASIO », et une
//! zone « local:Speakers » s'affichait « ASIO (exclusive) » pendant que la
//! lecture passait par WASAPI (Jean-François, fil 2018 : « Le choix est bien
//! ASIO malgré l'erreur WASAPI »).
//!
//! La règle ci-dessous ne corrige que ce mensonge-là : un libellé ASIO sur une
//! sortie dont le backend ouvrable n'est pas ASIO. Tout le reste passe tel
//! quel — le repli observé (ASIO sans périphérique → WASAPI) garde son nom, et
//! une zone non locale n'est pas touchée.

use crate::state::AppState;

/// LA règle, pure : le nom de backend à afficher pour une sortie.
///
/// `global` est ce que rend `active_backend_name` ; `ouvrable` le backend sous
/// lequel la sortie enregistrée de la zone sera ouverte
/// (`LocalOutput::audio_backend`), `None` s'il n'est pas connu (zone non
/// locale, sortie absente du registre).
///
/// Une sortie qui ne s'ouvre pas en ASIO s'ouvre sous l'hôte par défaut
/// (`select_host`), qui est WASAPI sur la seule plateforme où ASIO existe.
pub(crate) fn backend_affiche_pour_la_sortie(
    global: &'static str,
    ouvrable: Option<&str>,
) -> &'static str {
    match ouvrable {
        Some(b) if global == "ASIO" && !b.trim().eq_ignore_ascii_case("asio") => "WASAPI",
        _ => global,
    }
}

/// Le backend sous lequel s'ouvre la sortie enregistrée de la zone
/// (`LocalOutput::audio_backend`, rectifié par l'hôte d'origine, #1770).
///
/// `None` pour une zone non locale, une sortie absente du registre, ou une
/// construction sans `local-audio`. Même accès au registre que
/// `output_capabilities`, sur les mêmes routes : la sortie est lue, rien n'est
/// ouvert ni énuméré.
#[cfg(feature = "local-audio")]
pub(crate) async fn backend_de_la_sortie_de_la_zone(
    state: &AppState,
    output_device_id: Option<&str>,
) -> Option<String> {
    let device_id = output_device_id.filter(|id| id.starts_with("local:"))?;
    let sortie = { state.outputs.lock().await.get(device_id) }?;
    let sortie = sortie.lock().await;
    sortie
        .as_any()
        .downcast_ref::<tune_core::outputs::local::LocalOutput>()
        .map(|locale| locale.audio_backend().trim().to_ascii_lowercase())
}

/// Sans sortie locale compilée, aucune zone n'a de backend local.
#[cfg(not(feature = "local-audio"))]
pub(crate) async fn backend_de_la_sortie_de_la_zone(
    _state: &AppState,
    _output_device_id: Option<&str>,
) -> Option<String> {
    None
}

/// Le backend à afficher pour la zone dont la sortie est `output_device_id`.
pub(crate) async fn backend_affiche_de_la_zone(
    state: &AppState,
    output_device_id: Option<&str>,
    global: &'static str,
) -> &'static str {
    let ouvrable = backend_de_la_sortie_de_la_zone(state, output_device_id).await;
    backend_affiche_pour_la_sortie(global, ouvrable.as_deref())
}

/// La sortie de la zone s'ouvre-t-elle HORS du backend choisi ? — règle pure.
///
/// Décision de Bertrand (29/09, #5353) : ASIO choisi, une zone liée à une
/// sortie WASAPI (« This Computer », « Speakers ») reste jouable, mais elle
/// est SIGNALÉE. La comparaison porte sur la seule frontière qui existe :
/// ASIO ou pas. `auto` et `wasapi` ouvrent le même hôte (`select_host`) ;
/// les comparer en chaînes signalerait à tort toutes les zones d'une machine
/// réglée en « wasapi » dont les sorties sont nées sous « auto ».
pub(crate) fn hors_backend_choisi(backend_configure: &str, backend_sortie: &str) -> bool {
    let asio = |b: &str| b.trim().eq_ignore_ascii_case("asio");
    asio(backend_configure) != asio(backend_sortie)
}

/// Pose `backend_sortie` et `hors_backend_choisi` sur la charge utile d'une
/// zone. Rien n'est posé quand le backend de la sortie n'est pas connu : un
/// client ne doit pas prendre une absence pour un « non ».
pub(crate) fn injecter_backend_de_sortie(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    backend_configure: &str,
    backend_sortie: Option<&str>,
) {
    let Some(sortie) = backend_sortie else {
        return;
    };
    obj.insert("backend_sortie".into(), serde_json::json!(sortie));
    obj.insert(
        "hors_backend_choisi".into(),
        serde_json::json!(hors_backend_choisi(backend_configure, sortie)),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn une_sortie_wasapi_sous_asio_ne_s_affiche_pas_asio() {
        assert_eq!(
            backend_affiche_pour_la_sortie("ASIO", Some("wasapi")),
            "WASAPI",
            "« Speakers » s'ouvre en WASAPI : le panneau ne doit pas dire ASIO"
        );
    }

    #[test]
    fn table_de_la_regle() {
        // Le vrai pilote ASIO reste ASIO.
        assert_eq!(backend_affiche_pour_la_sortie("ASIO", Some("asio")), "ASIO");
        assert_eq!(
            backend_affiche_pour_la_sortie("ASIO", Some(" ASIO ")),
            "ASIO"
        );
        // Origine inconnue : rien à rectifier.
        assert_eq!(backend_affiche_pour_la_sortie("ASIO", None), "ASIO");
        // Un repli observé garde son nom.
        assert_eq!(
            backend_affiche_pour_la_sortie("WASAPI", Some("asio")),
            "WASAPI"
        );
        assert_eq!(
            backend_affiche_pour_la_sortie("WASAPI", Some("wasapi")),
            "WASAPI"
        );
        // Hors Windows, rien ne change.
        assert_eq!(
            backend_affiche_pour_la_sortie("CoreAudio", Some("auto")),
            "CoreAudio"
        );
        assert_eq!(backend_affiche_pour_la_sortie("ALSA", Some("auto")), "ALSA");
    }

    /// Le câblage de bout en bout : une sortie « Speakers » construite comme
    /// le rescan la construit (réglage `asio`, hôte d'origine WASAPI) est lue
    /// dans le registre et rectifiée ; un pilote ASIO ne l'est pas.
    #[cfg(feature = "local-audio")]
    #[tokio::test]
    async fn la_zone_speakers_sous_asio_lit_le_backend_de_sa_sortie() {
        use tune_core::outputs::local::LocalOutput;
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        {
            let mut registre = state.outputs.lock().await;
            registre.register(Box::new(
                LocalOutput::with_options_and_endpoint("Speakers".into(), None, false, "asio")
                    .with_origin_host("WASAPI"),
            ));
            registre.register(Box::new(
                LocalOutput::with_options_and_endpoint(
                    "Essence STX II ASIO(64)".into(),
                    None,
                    true,
                    "asio",
                )
                .with_origin_host("ASIO"),
            ));
        }
        assert_eq!(
            backend_affiche_de_la_zone(&state, Some("local:Speakers"), "ASIO").await,
            "WASAPI"
        );
        assert_eq!(
            backend_affiche_de_la_zone(&state, Some("local:Essence STX II ASIO(64)"), "ASIO").await,
            "ASIO"
        );
        // Sortie absente du registre, zone non locale : la valeur globale.
        assert_eq!(
            backend_affiche_de_la_zone(&state, Some("local:Absent"), "ASIO").await,
            "ASIO"
        );
        assert_eq!(
            backend_affiche_de_la_zone(&state, Some("uuid:dlna-1"), "ASIO").await,
            "ASIO"
        );
    }

    #[test]
    fn table_du_signalement() {
        // ASIO choisi : une sortie WASAPI est signalée, le pilote ASIO non.
        assert!(hors_backend_choisi("asio", "wasapi"));
        assert!(hors_backend_choisi(" ASIO ", "auto"));
        assert!(!hors_backend_choisi("asio", "asio"));
        // L'inverse : WASAPI choisi, une sortie encore ouverte en ASIO.
        assert!(hors_backend_choisi("wasapi", "asio"));
        // `auto` et `wasapi` ouvrent le même hôte : rien à signaler.
        assert!(!hors_backend_choisi("wasapi", "auto"));
        assert!(!hors_backend_choisi("auto", "wasapi"));
        assert!(!hors_backend_choisi("auto", "auto"));
    }

    /// L'API elle-même : `GET /zones` et `GET /zones/{id}` portent
    /// `backend_sortie` et `hors_backend_choisi` sur la zone « Speakers »
    /// quand ASIO est choisi, et la lecture n'y est pas refusée (aucune
    /// garde n'est ajoutée). Une zone dont la sortie n'est pas au registre ne
    /// porte PAS les champs : l'absence veut dire « on ne sait pas ».
    #[cfg(feature = "local-audio")]
    #[tokio::test]
    async fn get_zones_signale_la_zone_wasapi_quand_asio_est_choisi() {
        use axum::body::{Body, to_bytes};
        use axum::http::{Request, StatusCode};
        use serde_json::Value;
        use tower::ServiceExt;
        use tune_core::db::settings_repo::SettingsRepo;
        use tune_core::db::zone_repo::ZoneRepo;
        use tune_core::outputs::local::LocalOutput;

        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        SettingsRepo::with_backend(state.backend.clone())
            .set("local_audio_backend", "asio")
            .unwrap();
        let zones = ZoneRepo::with_backend(state.backend.clone());
        let speakers = zones
            .create("This Computer", Some("local"), Some("local:Speakers"))
            .unwrap();
        let pilote = zones
            .create(
                "STX II",
                Some("local"),
                Some("local:Essence STX II ASIO(64)"),
            )
            .unwrap();
        let absente = zones
            .create("DAC éteint", Some("local"), Some("local:DAC USB"))
            .unwrap();
        {
            let mut registre = state.outputs.lock().await;
            registre.register(Box::new(
                LocalOutput::with_options_and_endpoint("Speakers".into(), None, false, "asio")
                    .with_origin_host("WASAPI"),
            ));
            registre.register(Box::new(
                LocalOutput::with_options_and_endpoint(
                    "Essence STX II ASIO(64)".into(),
                    None,
                    true,
                    "asio",
                )
                .with_origin_host("ASIO"),
            ));
        }
        let router = crate::routes::router(state.clone());
        let lire = |url: String| {
            let router = router.clone();
            async move {
                let reponse = router
                    .oneshot(Request::builder().uri(&url).body(Body::empty()).unwrap())
                    .await
                    .unwrap();
                assert_eq!(reponse.status(), StatusCode::OK, "{url}");
                serde_json::from_slice::<Value>(
                    &to_bytes(reponse.into_body(), 1_000_000).await.unwrap(),
                )
                .unwrap()
            }
        };
        let liste = lire("/api/v1/zones".into()).await;
        let dans_la_liste = |id: i64| {
            liste
                .as_array()
                .unwrap()
                .iter()
                .find(|z| z["id"] == id)
                .cloned()
                .unwrap()
        };
        for zone in [
            dans_la_liste(speakers),
            lire(format!("/api/v1/zones/{speakers}")).await,
        ] {
            assert_eq!(zone["backend_sortie"], "wasapi", "{zone}");
            assert_eq!(
                zone["hors_backend_choisi"], true,
                "ASIO est choisi et « Speakers » s'ouvre en WASAPI : la zone doit être signalée"
            );
        }
        for zone in [
            dans_la_liste(pilote),
            lire(format!("/api/v1/zones/{pilote}")).await,
        ] {
            assert_eq!(zone["backend_sortie"], "asio", "{zone}");
            assert_eq!(zone["hors_backend_choisi"], false, "{zone}");
        }
        for zone in [
            dans_la_liste(absente),
            lire(format!("/api/v1/zones/{absente}")).await,
        ] {
            assert!(zone.get("backend_sortie").is_none(), "{zone}");
            assert!(zone.get("hors_backend_choisi").is_none(), "{zone}");
        }
    }

    /// Les trois charges utiles que le client range dans son magasin de
    /// zones portent le signalement : `GET /zones`, `GET /zones/{id}` et la
    /// réponse de lecture, que `syncZone` substitue à l'objet zone. Une seule
    /// qui l'oublierait ferait clignoter le badge à chaque lecture.
    #[test]
    fn les_trois_charges_utiles_de_zone_portent_le_signalement() {
        for (chemin, source, attendu) in [
            (
                "tune-server/src/routes/zones/lecture.rs",
                include_str!("lecture.rs"),
                2,
            ),
            (
                "tune-server/src/routes/playback.rs",
                include_str!("../playback.rs"),
                1,
            ),
        ] {
            let appels = source.matches("injecter_backend_de_sortie(").count();
            assert_eq!(
                appels, attendu,
                "{chemin} : {appels} appel(s) à injecter_backend_de_sortie, {attendu} attendu(s) (#5353)"
            );
        }
    }

    /// Les quatre charges utiles qui portent `signal_path` passent par la
    /// règle. Un site qui repasserait la valeur globale compile : sans cette
    /// garde, rien ne l'attraperait.
    #[test]
    fn les_quatre_sites_du_chemin_du_signal_lisent_la_sortie_de_la_zone() {
        // `lecture.rs` lit le backend de la sortie une fois par zone et en
        // tire aussi `backend_sortie` : il appelle la règle pure directement.
        let sites: [(&str, &str, &str, usize); 3] = [
            (
                "tune-server/src/routes/zones/lecture.rs",
                include_str!("lecture.rs"),
                "backend_affiche_pour_la_sortie(",
                2,
            ),
            (
                "tune-server/src/routes/playback.rs",
                include_str!("../playback.rs"),
                "backend_affiche_pour_la_sortie(",
                1,
            ),
            (
                "tune-server/src/routes/ws.rs",
                include_str!("../ws.rs"),
                "backend_affiche_de_la_zone(",
                1,
            ),
        ];
        for (chemin, source, motif, attendu) in sites {
            let appels = source.matches(motif).count();
            assert_eq!(
                appels, attendu,
                "{chemin} : {appels} appel(s) à {motif}…), {attendu} attendu(s) — \
                 un chemin du signal y nomme de nouveau le backend GLOBAL (#5353)"
            );
        }
    }
}
