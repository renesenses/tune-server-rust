//! #6120 — rattacher À LA MAIN une zone locale orpheline à un périphérique
//! ASIO présent (décision de Bertrand du 12/10/2026).
//!
//! `outputs::identite_de_sortie` refuse, à bon droit, de ré-associer seule une
//! zone ASIO (`Refus::BackendNonReassociable`) : l'identifiant d'un pilote ASIO
//! EST son nom, et un pilote générique porte le même nom pour des appareils
//! différents. Ce refus automatique laissait la zone de Gros Bidon (fil 2215,
//! zone 14) orpheline, réglages compris, et l'obligeait à recréer une zone.
//!
//! Ici, c'est l'UTILISATEUR qui désigne l'appareil : le doute qui fonde le
//! refus automatique est levé par son choix. La route reste étroite :
//!
//! - `GET  /zones/{id}/rattacher` : les sorties ASIO présentes qu'aucune autre
//!   zone ne porte ;
//! - `POST /zones/{id}/rattacher` `{"device_id": "local:<pilote>"}` : rattache.
//!
//! Refus nommés ([`RefusDeRattachement`]) : zone inconnue, masquée ou non
//! locale ; zone qui n'est pas orpheline (sa sortie est présente) ; cible
//! absente ; cible qui ne s'ouvre pas en ASIO ; cible déjà portée par une autre
//! zone (ce serait une fusion — c'est la route `fusionner-dans`).
//!
//! L'écriture est celle de la ré-association automatique
//! (`ZoneRepo::appliquer_identite_de_sortie`) : `output_device_id`, puis
//! `output_endpoint_id` (`asio:<pilote>`, la forme de `cpal::DeviceId` sous
//! ASIO). Réglages, volume et file suivent la ligne.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;
use tracing::info;

use tune_core::db::zone_repo::{Zone, ZoneRepo};

use crate::state::AppState;

/// Pourquoi un rattachement est refusé.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefusDeRattachement {
    ZoneInconnue,
    ZoneMasquee,
    ZoneNonLocale,
    ZoneNonOrpheline,
    CibleInvalide,
    CibleAbsente,
    CibleNonAsio,
    CibleDejaPortee(i64),
}

impl RefusDeRattachement {
    pub(crate) fn code(self) -> &'static str {
        match self {
            Self::ZoneInconnue => "zone_inconnue",
            Self::ZoneMasquee => "zone_masquee",
            Self::ZoneNonLocale => "zone_non_locale",
            Self::ZoneNonOrpheline => "zone_non_orpheline",
            Self::CibleInvalide => "cible_invalide",
            Self::CibleAbsente => "cible_absente",
            Self::CibleNonAsio => "cible_non_asio",
            Self::CibleDejaPortee(_) => "cible_deja_portee",
        }
    }

    fn statut(self) -> StatusCode {
        match self {
            Self::ZoneInconnue | Self::ZoneMasquee => StatusCode::NOT_FOUND,
            Self::ZoneNonLocale | Self::CibleInvalide => StatusCode::BAD_REQUEST,
            Self::ZoneNonOrpheline
            | Self::CibleAbsente
            | Self::CibleNonAsio
            | Self::CibleDejaPortee(_) => StatusCode::CONFLICT,
        }
    }

    fn message(self) -> String {
        match self {
            Self::ZoneInconnue => "zone inconnue".into(),
            Self::ZoneMasquee => "zone supprimée".into(),
            Self::ZoneNonLocale => "seule une zone locale se rattache à une sortie ASIO".into(),
            Self::ZoneNonOrpheline => {
                "la sortie de cette zone est présente : rien à rattacher".into()
            }
            Self::CibleInvalide => "device_id attendu sous la forme local:<pilote>".into(),
            Self::CibleAbsente => "cette sortie n'est pas présente sur la machine".into(),
            Self::CibleNonAsio => "cette sortie ne s'ouvre pas en ASIO".into(),
            Self::CibleDejaPortee(z) => {
                format!("la zone {z} porte déjà cette sortie : fusionner plutôt que rattacher")
            }
        }
    }

    fn reponse(self, zone_id: i64) -> Response {
        let mut corps = json!({
            "error": self.code(),
            "message": self.message(),
            "zone_id": zone_id,
        });
        if let Self::CibleDejaPortee(z) = self {
            corps["zone_qui_porte_la_cible"] = json!(z);
        }
        (self.statut(), Json(corps)).into_response()
    }
}

/// Ce que la règle doit savoir de la zone et de la cible.
#[derive(Debug, Clone)]
pub(crate) struct Constat<'a> {
    pub zone: Option<&'a Zone>,
    pub zone_masquee: bool,
    /// La sortie actuelle de la zone est-elle enregistrée ?
    pub sortie_actuelle_presente: bool,
    pub cible: &'a str,
    /// `LocalOutput::audio_backend` de la cible ; `None` = cible absente.
    pub backend_de_la_cible: Option<&'a str>,
    /// Une AUTRE zone (visible ou masquée) porte-t-elle déjà la cible ?
    pub cible_portee_par: Option<i64>,
}

/// LA règle, pure.
pub(crate) fn decider(c: &Constat<'_>) -> Result<(), RefusDeRattachement> {
    let zone = c.zone.ok_or(RefusDeRattachement::ZoneInconnue)?;
    if c.zone_masquee {
        return Err(RefusDeRattachement::ZoneMasquee);
    }
    if zone.output_type.as_deref() != Some("local") {
        return Err(RefusDeRattachement::ZoneNonLocale);
    }
    if c.sortie_actuelle_presente {
        return Err(RefusDeRattachement::ZoneNonOrpheline);
    }
    if !c.cible.starts_with("local:") || c.cible.len() <= "local:".len() {
        return Err(RefusDeRattachement::CibleInvalide);
    }
    let backend = c
        .backend_de_la_cible
        .ok_or(RefusDeRattachement::CibleAbsente)?;
    if !backend.trim().eq_ignore_ascii_case("asio") {
        return Err(RefusDeRattachement::CibleNonAsio);
    }
    if let Some(autre) = c.cible_portee_par
        && Some(autre) != zone.id
    {
        return Err(RefusDeRattachement::CibleDejaPortee(autre));
    }
    Ok(())
}

/// Pose `sortie_absente` sur la charge utile d'une zone locale : sa sortie
/// n'est pas enregistrée (zone orpheline). Le client y accroche l'action
/// « Rattacher ». Rien n'est posé pour une zone non locale.
pub(crate) fn injecter_sortie_absente(
    obj: &mut serde_json::Map<String, serde_json::Value>,
    zone: &Zone,
    sorties_enregistrees: &std::collections::HashSet<String>,
) {
    if zone.output_type.as_deref() != Some("local") {
        return;
    }
    let absente = zone
        .output_device_id
        .as_deref()
        .is_some_and(|d| !sorties_enregistrees.contains(d));
    obj.insert("sortie_absente".into(), json!(absente));
}

/// Les sorties locales enregistrées qui s'ouvrent en ASIO : `(device_id,
/// nom du pilote)`.
#[cfg(feature = "local-audio")]
async fn sorties_asio(state: &AppState) -> Vec<(String, String)> {
    let ids = state.outputs.lock().await.list();
    let mut asio = Vec::new();
    for id in ids.into_iter().filter(|id| id.starts_with("local:")) {
        if super::backend_de_la_sortie_de_la_zone(state, Some(&id))
            .await
            .is_some_and(|b| b.eq_ignore_ascii_case("asio"))
        {
            let nom = id.trim_start_matches("local:").to_string();
            asio.push((id, nom));
        }
    }
    asio.sort();
    asio
}

#[cfg(not(feature = "local-audio"))]
async fn sorties_asio(_state: &AppState) -> Vec<(String, String)> {
    Vec::new()
}

/// La zone qui porte `device_id`, masquées comprises.
fn zone_qui_porte(repo: &ZoneRepo, device_id: &str) -> Option<i64> {
    repo.get_by_device_id(device_id)
        .ok()
        .flatten()
        .and_then(|z| z.id)
}

/// `GET /zones/{id}/rattacher` — les sorties ASIO auxquelles cette zone peut
/// être rattachée.
pub(crate) async fn candidats_au_rattachement(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Response {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let Some(zone) = repo.get(id).ok().flatten() else {
        return RefusDeRattachement::ZoneInconnue.reponse(id);
    };
    let candidats: Vec<serde_json::Value> = sorties_asio(&state)
        .await
        .into_iter()
        .filter(|(device_id, _)| {
            zone_qui_porte(&repo, device_id).is_none_or(|z| Some(z) == zone.id)
        })
        .map(|(device_id, nom)| json!({"device_id": device_id, "name": nom, "backend": "asio"}))
        .collect();
    (
        StatusCode::OK,
        Json(json!({"zone_id": id, "candidats": candidats})),
    )
        .into_response()
}

#[derive(Deserialize)]
pub(crate) struct CorpsDeRattachement {
    device_id: String,
}

/// `POST /zones/{id}/rattacher` — rattache la zone orpheline à la sortie ASIO
/// désignée.
pub(crate) async fn rattacher_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(corps): Json<CorpsDeRattachement>,
) -> Response {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zone = repo.get(id).ok().flatten();
    let cible = corps.device_id.trim().to_string();
    let enregistrees: std::collections::HashSet<String> =
        state.outputs.lock().await.list().into_iter().collect();
    let sortie_actuelle_presente = zone
        .as_ref()
        .and_then(|z| z.output_device_id.as_deref())
        .is_some_and(|d| enregistrees.contains(d));
    let backend_de_la_cible = if enregistrees.contains(&cible) {
        super::backend_de_la_sortie_de_la_zone(&state, Some(&cible)).await
    } else {
        None
    };
    let constat = Constat {
        zone: zone.as_ref(),
        zone_masquee: zone
            .as_ref()
            .and_then(|z| z.output_device_id.as_deref())
            .is_some_and(|d| repo.is_device_hidden(d)),
        sortie_actuelle_presente,
        cible: &cible,
        backend_de_la_cible: backend_de_la_cible.as_deref(),
        cible_portee_par: zone_qui_porte(&repo, &cible),
    };
    if let Err(refus) = decider(&constat) {
        tracing::warn!(zone_id = id, cible = %cible, motif = refus.code(), "rattachement_de_zone_refuse");
        return refus.reponse(id);
    }
    let ancien = zone
        .as_ref()
        .and_then(|z| z.output_device_id.clone())
        .unwrap_or_default();
    let endpoint = format!("asio:{}", cible.trim_start_matches("local:"));
    let ecrit = repo
        .update_output_device(id, &cible)
        .and_then(|()| repo.update_output_endpoint_id(id, &endpoint));
    if let Err(e) = ecrit {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "rattachement_echoue", "message": e, "zone_id": id})),
        )
            .into_response();
    }
    // #6774 — PURE / volume fixe désarmés par le changement d'appareil.
    tune_core::audio::reglages_de_volume::aligner_la_lecture(&state.backend, &state.playback, id)
        .await;
    info!(
        zone_id = id,
        ancien = %ancien,
        nouveau = %cible,
        endpoint_id = %endpoint,
        "zone_locale_rattachee_a_la_main"
    );
    state.event_bus.emit_typed(
        tune_core::event_types::EventType::ZoneUpdated,
        json!({"id": id}),
    );
    (
        StatusCode::OK,
        Json(json!({
            "zone_id": id,
            "ancien_device_id": ancien,
            "output_device_id": cible,
            "output_endpoint_id": endpoint,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(id: i64, output_type: &str, device: &str) -> Zone {
        Zone {
            id: Some(id),
            name: "Sortie SMSL SU-8".into(),
            output_type: Some(output_type.into()),
            output_device_id: Some(device.into()),
            volume: 30.0,
            muted: false,
            online: true,
            gapless_enabled: true,
            group_id: None,
            sync_delay_ms: 0,
            last_position_ms: 0,
            last_track_id: None,
            last_track_source: None,
            last_track_source_id: None,
            max_sample_rate: None,
            fixed_volume: false,
            autoplay_enabled: false,
        }
    }

    fn constat<'a>(z: &'a Zone, cible: &'a str, backend: Option<&'a str>) -> Constat<'a> {
        Constat {
            zone: Some(z),
            zone_masquee: false,
            sortie_actuelle_presente: false,
            cible,
            backend_de_la_cible: backend,
            cible_portee_par: None,
        }
    }

    #[test]
    fn une_zone_orpheline_se_rattache_a_une_sortie_asio_presente() {
        let z = zone(14, "local", "local:SMSL USB AUDIO");
        assert_eq!(
            decider(&constat(&z, "local:USB DAC ASIO", Some("asio"))),
            Ok(())
        );
    }

    #[test]
    fn les_refus_sont_nommes() {
        let z = zone(14, "local", "local:SMSL USB AUDIO");
        assert_eq!(
            decider(&constat(&z, "local:USB DAC ASIO", None)),
            Err(RefusDeRattachement::CibleAbsente)
        );
        assert_eq!(
            decider(&constat(&z, "local:Speakers", Some("wasapi"))),
            Err(RefusDeRattachement::CibleNonAsio)
        );
        assert_eq!(
            decider(&constat(&z, "uuid:abc", Some("asio"))),
            Err(RefusDeRattachement::CibleInvalide)
        );
        let mut c = constat(&z, "local:USB DAC ASIO", Some("asio"));
        c.cible_portee_par = Some(15);
        assert_eq!(decider(&c), Err(RefusDeRattachement::CibleDejaPortee(15)));
        let mut c = constat(&z, "local:USB DAC ASIO", Some("asio"));
        c.sortie_actuelle_presente = true;
        assert_eq!(decider(&c), Err(RefusDeRattachement::ZoneNonOrpheline));
        let mut c = constat(&z, "local:USB DAC ASIO", Some("asio"));
        c.zone_masquee = true;
        assert_eq!(decider(&c), Err(RefusDeRattachement::ZoneMasquee));
        let dlna = zone(3, "dlna", "uuid:salon");
        assert_eq!(
            decider(&constat(&dlna, "local:USB DAC ASIO", Some("asio"))),
            Err(RefusDeRattachement::ZoneNonLocale)
        );
        let mut c = constat(&z, "local:USB DAC ASIO", Some("asio"));
        c.zone = None;
        assert_eq!(decider(&c), Err(RefusDeRattachement::ZoneInconnue));
    }
}

/// La route, de bout en bout, sur un `AppState` réel : une sortie ASIO
/// enregistrée (sans matériel : `LocalOutput` n'ouvre rien à la construction).
#[cfg(all(test, feature = "local-audio"))]
mod route_tests {
    use super::*;
    use axum::body::to_bytes;

    struct Banc {
        _dossier: tempfile::TempDir,
        state: AppState,
    }

    fn monter() -> Banc {
        let dossier = tempfile::tempdir().unwrap();
        let chemin = dossier.path().join("tune.db");
        let state = AppState::new(chemin.to_str().unwrap(), 0, Default::default()).unwrap();
        Banc {
            _dossier: dossier,
            state,
        }
    }

    async fn enregistrer(state: &AppState, nom: &str, backend_configure: &str, hote: &str) {
        let sortie = tune_core::outputs::local::LocalOutput::with_options_and_endpoint(
            nom.to_string(),
            Some(format!("{hote}:{nom}")),
            true,
            backend_configure,
        )
        .with_origin_host(hote);
        state.outputs.lock().await.register(Box::new(sortie));
    }

    async fn corps(r: Response) -> (StatusCode, serde_json::Value) {
        let statut = r.status();
        let octets = to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (statut, serde_json::from_slice(&octets).unwrap())
    }

    async fn rattacher(state: &AppState, id: i64, cible: &str) -> (StatusCode, serde_json::Value) {
        corps(
            rattacher_zone(
                State(state.clone()),
                Path(id),
                Json(CorpsDeRattachement {
                    device_id: cible.into(),
                }),
            )
            .await,
        )
        .await
    }

    /// Le cas de Gros Bidon (fil 2215) : la zone 14 vise un pilote ASIO qui
    /// n'existe plus ; « USB DAC ASIO » est présent. Le rattachement garde la
    /// zone (son identifiant, son volume) et la pose sur le pilote présent.
    #[tokio::test]
    async fn la_zone_orpheline_est_rattachee_et_garde_ses_reglages() {
        let banc = monter();
        let repo = ZoneRepo::with_backend(banc.state.backend.clone());
        let id = repo
            .create(
                "Sortie SMSL SU-8",
                Some("local"),
                Some("local:SMSL USB AUDIO"),
            )
            .unwrap();
        repo.update_volume(id, 27.0).unwrap();
        enregistrer(&banc.state, "USB DAC ASIO", "asio", "asio").await;

        let (statut, candidats) =
            corps(candidats_au_rattachement(State(banc.state.clone()), Path(id)).await).await;
        assert_eq!(statut, StatusCode::OK);
        assert_eq!(
            candidats["candidats"][0]["device_id"], "local:USB DAC ASIO",
            "le pilote ASIO présent est proposé : {candidats}"
        );

        let (statut, reponse) = rattacher(&banc.state, id, "local:USB DAC ASIO").await;
        assert_eq!(statut, StatusCode::OK, "{reponse}");
        let zone = repo.get(id).unwrap().unwrap();
        assert_eq!(zone.output_device_id.as_deref(), Some("local:USB DAC ASIO"));
        assert_eq!(zone.volume, 27.0, "les réglages suivent la ligne");
        assert_eq!(
            repo.endpoint_id_de_la_sortie("local:USB DAC ASIO")
                .as_deref(),
            Some("asio:USB DAC ASIO")
        );
    }

    /// Une sortie qui ne s'ouvre pas en ASIO (nom WASAPI) est refusée, une
    /// sortie absente aussi, et rien n'est écrit.
    #[tokio::test]
    async fn une_cible_non_asio_ou_absente_est_refusee_sans_ecrire() {
        let banc = monter();
        let repo = ZoneRepo::with_backend(banc.state.backend.clone());
        let id = repo
            .create(
                "Sortie SMSL SU-8",
                Some("local"),
                Some("local:SMSL USB AUDIO"),
            )
            .unwrap();
        enregistrer(&banc.state, "Speakers", "asio", "wasapi").await;

        let (statut, reponse) = rattacher(&banc.state, id, "local:Speakers").await;
        assert_eq!(statut, StatusCode::CONFLICT);
        assert_eq!(reponse["error"], "cible_non_asio");

        let (statut, reponse) = rattacher(&banc.state, id, "local:USB DAC ASIO").await;
        assert_eq!(statut, StatusCode::CONFLICT);
        assert_eq!(reponse["error"], "cible_absente");

        assert_eq!(
            repo.get(id).unwrap().unwrap().output_device_id.as_deref(),
            Some("local:SMSL USB AUDIO")
        );
        let (_, candidats) =
            corps(candidats_au_rattachement(State(banc.state.clone()), Path(id)).await).await;
        assert_eq!(
            candidats["candidats"],
            json!([]),
            "aucun pilote ASIO proposé"
        );
    }

    /// Une zone dont la sortie est présente n'est pas orpheline ; une cible
    /// déjà portée par une autre zone n'est pas volée.
    #[tokio::test]
    async fn ni_zone_non_orpheline_ni_cible_deja_portee() {
        let banc = monter();
        let repo = ZoneRepo::with_backend(banc.state.backend.clone());
        enregistrer(&banc.state, "USB DAC ASIO", "asio", "asio").await;
        let porteuse = repo
            .create("SU-8 neuve", Some("local"), Some("local:USB DAC ASIO"))
            .unwrap();
        let orpheline = repo
            .create(
                "Sortie SMSL SU-8",
                Some("local"),
                Some("local:SMSL USB AUDIO"),
            )
            .unwrap();

        let (statut, reponse) = rattacher(&banc.state, orpheline, "local:USB DAC ASIO").await;
        assert_eq!(statut, StatusCode::CONFLICT);
        assert_eq!(reponse["error"], "cible_deja_portee");
        assert_eq!(reponse["zone_qui_porte_la_cible"], porteuse);

        let (statut, reponse) = rattacher(&banc.state, porteuse, "local:USB DAC ASIO").await;
        assert_eq!(statut, StatusCode::CONFLICT);
        assert_eq!(reponse["error"], "zone_non_orpheline");
    }
}
