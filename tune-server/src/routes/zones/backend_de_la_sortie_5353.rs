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

/// Le backend à afficher pour la zone dont la sortie est `output_device_id`.
///
/// Même accès au registre que `output_capabilities`, sur les mêmes routes : la
/// sortie est lue, rien n'est ouvert ni énuméré.
#[cfg(feature = "local-audio")]
pub(crate) async fn backend_affiche_de_la_zone(
    state: &AppState,
    output_device_id: Option<&str>,
    global: &'static str,
) -> &'static str {
    let Some(device_id) = output_device_id.filter(|id| id.starts_with("local:")) else {
        return global;
    };
    let Some(sortie) = ({ state.outputs.lock().await.get(device_id) }) else {
        return global;
    };
    let sortie = sortie.lock().await;
    let ouvrable = sortie
        .as_any()
        .downcast_ref::<tune_core::outputs::local::LocalOutput>()
        .map(|locale| locale.audio_backend().to_string());
    backend_affiche_pour_la_sortie(global, ouvrable.as_deref())
}

/// Sans sortie locale compilée, il n'y a aucun backend local à rectifier.
#[cfg(not(feature = "local-audio"))]
pub(crate) async fn backend_affiche_de_la_zone(
    _state: &AppState,
    _output_device_id: Option<&str>,
    global: &'static str,
) -> &'static str {
    global
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

    /// Les quatre charges utiles qui portent `signal_path` passent par la
    /// règle. Un site qui repasserait la valeur globale compile : sans cette
    /// garde, rien ne l'attraperait.
    #[test]
    fn les_quatre_sites_du_chemin_du_signal_lisent_la_sortie_de_la_zone() {
        let sites: [(&str, &str, usize); 3] = [
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
            ("tune-server/src/routes/ws.rs", include_str!("../ws.rs"), 1),
        ];
        for (chemin, source, attendu) in sites {
            let appels = source.matches("backend_affiche_de_la_zone(").count();
            assert_eq!(
                appels, attendu,
                "{chemin} : {appels} appel(s) à backend_affiche_de_la_zone, {attendu} attendu(s) — \
                 un chemin du signal y nomme de nouveau le backend GLOBAL (#5353)"
            );
        }
    }
}
