use super::*;

/// Remonte au catalogue communautaire la marque/modele corriges d'une zone.
///
/// Ne part que si l'override est complet (marque ET modele) : une correction
/// partielle n'apprend rien de reutilisable au catalogue.
///
/// Soumis au meme consentement que la telemetrie (`TUNE_TELEMETRY`) : c'est la
/// porte deja etablie pour « cette instance parle-t-elle au cloud », et en
/// ajouter une seconde pour la meme question fragmenterait le reglage sans
/// rien clarifier.
///
/// Volontairement anonyme : ni identifiant d'instance, ni nom de zone. Le
/// serveur n'attend pas la reponse et n'echoue jamais la-dessus.
/// Réglages de renderer non-défaut d'une zone, sous la forme partagée avec le
/// catalogue communautaire (clés du RendererConfig + trim). Vide quand la zone
/// est aux défauts — un préréglage qui ne règle rien n'apprend rien.
pub(super) fn renderer_settings_snapshot(
    state: &AppState,
    zone_id: i64,
) -> serde_json::Map<String, Value> {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let mut out = serde_json::Map::new();
    if repo.get_dlna_native_flac(zone_id) {
        out.insert("dlna_native_flac".into(), json!(true));
    }
    if repo.get_alac_passthrough(zone_id) {
        out.insert("alac_passthrough".into(), json!(true));
    }
    if repo.get_aac_passthrough(zone_id) {
        out.insert("aac_passthrough".into(), json!(true));
    }
    if repo.get_dlna_lpcm(zone_id) {
        out.insert("dlna_lpcm".into(), json!(true));
    }
    if repo.get_dlna_cap_16bit(zone_id) {
        out.insert("dlna_cap_16bit".into(), json!(true));
    }
    if repo.get_dlna_wav24(zone_id) {
        out.insert("dlna_wav24".into(), json!(true));
    }
    let delay = repo.get_dlna_play_delay_ms(zone_id);
    if delay > 0 {
        out.insert("dlna_play_delay_ms".into(), json!(delay));
    }
    // #2263 — même famille que `dlna_play_delay_ms` : un réglage qu'on garde
    // parce que CET appareil-là le demande. Absent du relevé tant qu'il est au
    // défaut, comme tous ses voisins ici.
    if settings
        .get(&crate::config::cle_silence_upnp(zone_id))
        .ok()
        .flatten()
        .as_deref()
        == Some("true")
    {
        out.insert("upnp_silence".into(), json!(true));
    }
    let trim = settings
        .get(&format!("zone_{zone_id}_gain_trim_db"))
        .ok()
        .flatten()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0);
    if trim != 0.0 {
        out.insert("gain_trim_db".into(), json!(trim));
    }
    out
}

/// Identité (marque, modèle) d'une zone pour le catalogue communautaire :
/// override utilisateur d'abord, sinon détection UPnP de l'appareil assigné.
pub(super) async fn zone_identity_for_catalog(
    state: &AppState,
    zone_id: i64,
) -> Option<(String, String)> {
    let settings = SettingsRepo::with_backend(state.backend.clone());
    let key = |k: &str| {
        settings
            .get(&format!("zone_{zone_id}_{k}"))
            .ok()
            .flatten()
            .filter(|v| !v.trim().is_empty())
    };
    let (mut brand, mut model) = (key("brand"), key("model"));
    if brand.is_none() || model.is_none() {
        let zone = ZoneRepo::with_backend(state.backend.clone())
            .get(zone_id)
            .ok()
            .flatten()?;
        let devices = state.scanner.devices().await;
        let detected = zone
            .output_device_id
            .as_deref()
            .and_then(|did| devices.iter().find(|d| d.id == did));
        if brand.is_none() {
            brand = detected.and_then(|d| d.manufacturer.clone());
        }
        if model.is_none() {
            model = detected.and_then(|d| d.model.clone());
        }
    }
    match (brand, model) {
        (Some(b), Some(m)) => Some((b, m)),
        _ => None,
    }
}

/// GET /zones/{id}/device-presets — les préréglages communautaires pour
/// l'appareil de la zone (#1743). Proxy serveur vers mozaiklabs : le
/// navigateur ne parle jamais au site (CORS, vie privée), et un site
/// injoignable rend une liste vide — jamais une erreur, la page Appareils
/// n'a pas à dépendre du réseau extérieur.
pub(super) async fn get_device_presets(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let empty = || Json(json!({"presets": []})).into_response();
    let Some((brand, model)) = zone_identity_for_catalog(&state, id).await else {
        return empty();
    };
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(id)
        .ok()
        .flatten();
    let Ok(client) = tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    else {
        return empty();
    };
    let mut req = client
        .get("https://mozaiklabs.fr/api/v1/community/devices/presets")
        .query(&[("brand", brand.as_str()), ("model", model.as_str())]);
    if let Some(ot) = zone.as_ref().and_then(|z| z.output_type.clone()) {
        req = req.query(&[("output_type", ot)]);
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => match r.json::<Value>().await {
            Ok(v) => Json(v).into_response(),
            Err(_) => empty(),
        },
        Ok(r) => {
            tracing::debug!(status = %r.status(), "device_presets_fetch_non_success");
            empty()
        }
        Err(e) => {
            tracing::debug!(error = %e, "device_presets_fetch_failed");
            empty()
        }
    }
}

/// Partage les réglages de renderer d'une zone identifiée avec le catalogue
/// communautaire (#1743). Mêmes principes que push_device_correction :
/// anonyme, gaté télémétrie, best-effort en tâche de fond. Ne part que si
/// marque ET modèle sont connus et qu'au moins un réglage diffère des
/// défauts.
pub(super) async fn push_device_preset(state: &AppState, zone_id: i64) {
    // #3383 : `is_enabled_for` et non `is_enabled` — un refus pose dans
    // l'interface arrete cette remontee de catalogue comme le ferait
    // `TUNE_TELEMETRY=false`.
    if !tune_core::cloud::telemetry::TelemetryReporter::is_enabled_for(&SettingsRepo::with_backend(
        state.backend.clone(),
    )) {
        return;
    }
    let Some((brand, model)) = zone_identity_for_catalog(state, zone_id).await else {
        return;
    };
    let settings_map = renderer_settings_snapshot(state, zone_id);
    if settings_map.is_empty() {
        return;
    }
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten();
    let payload = json!({
        "brand": brand,
        "model": model,
        "output_type": zone.and_then(|z| z.output_type),
        "settings": Value::Object(settings_map),
    });
    tokio::spawn(async move {
        let Ok(client) = tune_core::http::client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        else {
            return;
        };
        match client
            .post("https://mozaiklabs.fr/api/v1/community/devices/presets")
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => tracing::debug!(status = %r.status(), "device_preset_pushed"),
            Err(e) => tracing::debug!(error = %e, "device_preset_push_failed"),
        }
    });
}

/// La charge utile d'une correction de marque/modèle, **sans réseau** — pour
/// que sa forme soit vérifiable (l'adresse, elle, est en dur dans ce fichier).
///
/// Le champ non corrigé part en chaîne vide et non en null : côté site, ces
/// colonnes entrent dans la clé d'unicité, où un null est « jamais égal » —
/// chaque renvoi créerait une ligne de plus au lieu d'incrémenter le compteur.
///
/// 🔴 **Seul l'OUI part, jamais la MAC complète** (#3589). Le site n'en garde
/// de toute façon que les trois premiers octets et écarte les trois derniers à
/// la réception ; mais ceux-là identifient un appareil chez quelqu'un, et cet
/// envoi est « volontairement anonyme » (entête de ce fichier). Ce qui ne part
/// pas ne peut pas fuiter. La clé est **absente** plutôt que nulle quand la MAC
/// est inconnue.
pub(super) fn charge_utile_correction(
    detected_manufacturer: Option<String>,
    detected_model: Option<String>,
    brand: Option<String>,
    model: Option<String>,
    output_type: Option<String>,
    mac: Option<&str>,
) -> Value {
    let mut payload = json!({
        "detected_manufacturer": detected_manufacturer,
        "detected_model": detected_model,
        "brand": brand.unwrap_or_default(),
        "model": model.unwrap_or_default(),
        "output_type": output_type,
    });
    if let (Some(oui), Some(obj)) = (
        mac.and_then(tune_core::discovery::mac::oui_prefix),
        payload.as_object_mut(),
    ) {
        obj.insert("oui".into(), json!(oui));
    }
    payload
}

pub(super) async fn push_device_correction(state: &AppState, zone_id: i64) {
    // #3383 : `is_enabled_for` et non `is_enabled` — un refus pose dans
    // l'interface arrete cette remontee de catalogue comme le ferait
    // `TUNE_TELEMETRY=false`.
    if !tune_core::cloud::telemetry::TelemetryReporter::is_enabled_for(&SettingsRepo::with_backend(
        state.backend.clone(),
    )) {
        return;
    }

    let settings = SettingsRepo::with_backend(state.backend.clone());
    let brand = settings
        .get(&format!("zone_{zone_id}_brand"))
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty());
    let model = settings
        .get(&format!("zone_{zone_id}_model"))
        .ok()
        .flatten()
        .filter(|v| !v.trim().is_empty());
    // L'un OU l'autre suffit. Exiger les deux écartait le cas le plus fréquent :
    // la marque seule est corrigée, parce que c'est elle que la déduction par OUI
    // se trompe, tandis que le modèle est généralement bien annoncé par
    // l'appareil. Ces corrections partielles ne partaient jamais, et le catalogue
    // communautaire — qui n'existe que pour les recueillir — s'en trouvait privé
    // de sa matière la plus courante.
    if brand.is_none() && model.is_none() {
        return;
    }

    let zone = match ZoneRepo::with_backend(state.backend.clone()).get(zone_id) {
        Ok(Some(z)) => z,
        _ => return,
    };
    let devices = state.scanner.devices().await;
    let detected = zone
        .output_device_id
        .as_deref()
        .and_then(|did| devices.iter().find(|d| d.id == did));

    // #3589 — l'OUI qui a produit la mauvaise déduction.
    //
    // Le commentaire du contrôleur qui reçoit ces corrections le dit depuis
    // toujours : « c'est elle que la déduction par OUI se trompe le plus
    // souvent » (voir le bloc juste au-dessus). Le serveur déduisait la marque
    // des trois premiers octets de la MAC — `vendor_for_mac`,
    // `tune-core/src/discovery/mac.rs` — et se trompait, mais rien ne remontait
    // QUELLE entrée de la table l'avait causé : chaque correction était perdue
    // pour la ligne qui l'avait provoquée.
    //
    // 🔴 Seul l'OUI part, jamais la MAC complète. Le site n'en garde de toute
    // façon que les trois premiers octets et écarte les trois derniers à la
    // réception — mais ceux-là identifient un appareil chez quelqu'un, et cet
    // envoi est « volontairement anonyme » (entête de ce fichier). Ce qui ne
    // part pas ne peut pas fuiter. Le champ `oui` est accepté par le site au
    // même titre que `mac`.
    let payload = charge_utile_correction(
        detected.and_then(|d| d.manufacturer.clone()),
        detected.and_then(|d| d.model.clone()),
        brand,
        model,
        zone.output_type,
        detected.and_then(|d| d.mac_address.as_deref()),
    );

    tokio::spawn(async move {
        let Ok(client) = tune_core::http::client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        else {
            return;
        };
        match client
            .post("https://mozaiklabs.fr/api/v1/community/devices")
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => tracing::debug!(status = %r.status(), "device_correction_pushed"),
            Err(e) => tracing::debug!(error = %e, "device_correction_push_failed"),
        }
    });
}

/// POST /zones/{id}/renderer-capabilities — on-demand "discovery check" for the
/// renderer-config UI. Probes the zone's DLNA renderer via GetProtocolInfo and
/// returns which audio formats its `Sink` advertises (FLAC, WAV/LPCM 16 & 24,
/// ALAC/AAC, MP3, DSD), so the user can pick a sensible output override with
/// evidence. Only meaningful for dlna/openhome zones with a live renderer.
pub(super) async fn renderer_capabilities(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let zone = match repo.get(id) {
        Ok(Some(z)) => z,
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "zone_not_found" })),
            )
                .into_response();
        }
    };

    if !matches!(zone.output_type.as_deref(), Some("dlna") | Some("openhome")) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({
                "error": "not_a_dlna_renderer",
                "message": "Renderer capability discovery is only available for DLNA/OpenHome zones.",
            })),
        )
            .into_response();
    }

    let Some(device_id) = zone.output_device_id.as_deref() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "no_output_device" })),
        )
            .into_response();
    };

    // The GetProtocolInfo probe needs the registered DlnaOutput (it holds the
    // ConnectionManager URL). If the renderer hasn't been played yet it may not
    // be registered — try to register it from the discovered device first, same
    // as create_zone does, so the check works without playing a track first.
    let mut output = { state.outputs.lock().await.get(device_id) };
    if output.is_none() {
        let disc = {
            let scanner = &state.scanner;
            let devices = scanner.devices().await;
            devices.iter().find(|d| d.id == device_id).cloned()
        };
        if let Some(dev) = disc {
            register_dlna_output_from_device(&dev, &state).await;
            output = state.outputs.lock().await.get(device_id);
        }
    }

    let Some(output) = output else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "probed": false,
                "reason": "renderer_offline",
            })),
        )
            .into_response();
    };

    // Hold the output lock for the SOAP round-trip (on-demand, user-initiated,
    // rare) — same pattern the orchestrator uses for its per-track probe.
    let caps = {
        let guard = output.lock().await;
        match guard.as_any().downcast_ref::<DlnaOutput>() {
            Some(dlna) => dlna.probe_capabilities().await,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "error": "not_a_dlna_output" })),
                )
                    .into_response();
            }
        }
    };

    // Une sonde reussie est un fait d'interet communautaire : quel format cet
    // appareil annonce-t-il vraiment ? Remontee anonyme, apres la reponse a
    // l'UI (spawn best-effort), jamais pour une sonde vide (`probed: false`,
    // qui ne dit rien de l'appareil).
    if caps.probed {
        push_device_caps(&state, id, &caps).await;
    }

    Json(json!(caps)).into_response()
}

/// Partage le resultat du « Verifier le renderer » avec le catalogue
/// communautaire. La sonde GetProtocolInfo tourne sur le LAN de l'utilisateur
/// — le site ne peut pas interroger un appareil derriere une box ; seul le
/// RESULTAT peut voyager. Agrege par appareil cote site, c'est le rapport de
/// verification consolide sur le parc. Memes principes que
/// push_device_preset : anonyme, gate telemetrie, best-effort en tache de
/// fond, et ne part que si marque ET modele sont connus.
pub(super) async fn push_device_caps(
    state: &AppState,
    zone_id: i64,
    caps: &tune_core::outputs::dlna::RendererCapabilities,
) {
    // #3383 : `is_enabled_for` et non `is_enabled` — un refus pose dans
    // l'interface arrete cette remontee de catalogue comme le ferait
    // `TUNE_TELEMETRY=false`.
    if !tune_core::cloud::telemetry::TelemetryReporter::is_enabled_for(&SettingsRepo::with_backend(
        state.backend.clone(),
    )) {
        return;
    }
    let Some((brand, model)) = zone_identity_for_catalog(state, zone_id).await else {
        return;
    };
    let zone = ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten();
    // Les drapeaux seulement : `probed` est un etat de la sonde (garanti true
    // ici) et `sink` du debogage local qui n'a pas a voyager.
    let payload = json!({
        "brand": brand,
        "model": model,
        "output_type": zone.and_then(|z| z.output_type),
        "caps": {
            "flac": caps.flac,
            "wav": caps.wav,
            "lpcm16": caps.lpcm16,
            "lpcm24": caps.lpcm24,
            "alac": caps.alac,
            "aac": caps.aac,
            "mp3": caps.mp3,
            "dsd": caps.dsd,
        },
    });
    tokio::spawn(async move {
        let Ok(client) = tune_core::http::client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
        else {
            return;
        };
        match client
            .post("https://mozaiklabs.fr/api/v1/community/devices/caps")
            .json(&payload)
            .send()
            .await
        {
            Ok(r) => tracing::debug!(status = %r.status(), "device_caps_pushed"),
            Err(e) => tracing::debug!(error = %e, "device_caps_push_failed"),
        }
    });
}

/// Register a DLNA output from a discovered device.
/// Fetches the device description XML to find AVTransport/RenderingControl URLs,
/// then registers the output in the global registry.
/// Returns true if registration succeeded.
pub(super) async fn register_dlna_output_from_device(
    dev: &tune_core::discovery::device::DiscoveredDevice,
    state: &AppState,
) -> bool {
    // First, try to get service URLs from the device's cached capabilities
    let svc_urls = dev
        .capabilities
        .get("service_urls")
        .and_then(|v| {
            serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone()).ok()
        })
        .unwrap_or_default();

    let av_url = svc_urls
        .get("avtransport")
        .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));
    let rc_url = svc_urls
        .get("renderingcontrol")
        .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));
    let cm_url = svc_urls
        .get("connectionmanager")
        .or_else(|| svc_urls.get("ConnectionManager"))
        .map(|p| format!("http://{}:{}{}", dev.host, dev.port, p));

    // If cached service URLs are available, use them
    if let (Some(av), Some(rc)) = (av_url, rc_url) {
        let delay =
            crate::config::resolve_play_delay(&state.backend, &state.config, &dev.id, &dev.name);
        let evt_urls = dev
            .capabilities
            .get("event_sub_urls")
            .and_then(|v| {
                serde_json::from_value::<std::collections::HashMap<String, String>>(v.clone()).ok()
            })
            .unwrap_or_default();
        let dlna = DlnaOutput::new(
            dev.name.clone(),
            dev.id.clone(),
            dev.host.clone(),
            av,
            rc,
            cm_url,
        )
        .with_play_delay(delay)
        .with_upnp_events(
            crate::startup::create_oh_listener().await,
            crate::discovery_setup::urls_evenements_dlna(&dev.host, dev.port, &evt_urls),
        )
        .with_upnp_silence(crate::config::resolve_upnp_silence(&state.backend, &dev.id));
        let mut outputs = state.outputs.lock().await;
        outputs.register(Box::new(dlna));
        info!(name = %dev.name, id = %dev.id, "dlna_output_registered_on_zone_create");
        return true;
    }

    // Fallback: fetch device description from location URL
    if let Some(ref location) = dev.location {
        match fetch_device_description(location).await {
            Ok(desc) => {
                if desc.is_media_renderer() || desc.is_openhome() {
                    let service_urls = desc.service_urls();
                    let av = service_urls.get("avtransport");
                    let rc = service_urls.get("renderingcontrol");
                    if let (Some(av_path), Some(rc_path)) = (av, rc) {
                        let base = format!("http://{}:{}", dev.host, dev.port);
                        let cm_path = service_urls
                            .get("connectionmanager")
                            .or_else(|| service_urls.get("ConnectionManager"))
                            .map(|p| format!("{base}{p}"));
                        let delay = crate::config::resolve_play_delay(
                            &state.backend,
                            &state.config,
                            &dev.id,
                            &dev.name,
                        );
                        let dlna = DlnaOutput::new(
                            dev.name.clone(),
                            dev.id.clone(),
                            dev.host.clone(),
                            format!("{base}{av_path}"),
                            format!("{base}{rc_path}"),
                            cm_path,
                        )
                        .with_play_delay(delay)
                        .with_upnp_events(
                            crate::startup::create_oh_listener().await,
                            crate::discovery_setup::urls_evenements_dlna(
                                &dev.host,
                                dev.port,
                                &desc.event_sub_urls(),
                            ),
                        )
                        .with_upnp_silence(
                            crate::config::resolve_upnp_silence(&state.backend, &dev.id),
                        );
                        let mut outputs = state.outputs.lock().await;
                        outputs.register(Box::new(dlna));
                        info!(name = %dev.name, id = %dev.id, "dlna_output_registered_via_description");
                        return true;
                    }
                }
            }
            Err(e) => {
                warn!(device = %dev.name, error = %e, "dlna_description_fetch_failed");
            }
        }
    }

    false
}

#[cfg(test)]
mod correction_tests {
    use super::charge_utile_correction;

    /// #3589 — la correction porte l'OUI qui a produit la mauvaise déduction,
    /// et RIEN de plus : les trois derniers octets ne quittent pas la maison.
    #[test]
    fn la_correction_porte_l_oui_et_pas_la_mac_complete() {
        let p = charge_utile_correction(
            Some("Yamaha Corporation".into()),
            Some("RX-V6A".into()),
            Some("Bluesound".into()),
            None,
            Some("dlna".into()),
            Some("00:A0:DE:12:34:56"),
        );
        assert_eq!(p["oui"], "00:A0:DE");
        let brut = serde_json::to_string(&p).unwrap();
        for octet in ["12:34:56", "123456"] {
            assert!(
                !brut.contains(octet),
                "les trois derniers octets ont fui : {brut}"
            );
        }
        // Le champ non corrigé part en chaîne vide, pas en null (clé d'unicité).
        assert_eq!(p["model"], "");
        assert_eq!(p["brand"], "Bluesound");
    }

    /// Les trois graphies que le site accepte arrivent toutes au même OUI.
    #[test]
    fn toutes_les_graphies_de_mac_donnent_le_meme_oui() {
        for graphie in ["AA:BB:CC:DD:EE:FF", "aa-bb-cc-dd-ee-ff", "aabbccddeeff"] {
            let p =
                charge_utile_correction(None, None, Some("X".into()), None, None, Some(graphie));
            assert_eq!(p["oui"], "AA:BB:CC", "graphie refusee : {graphie}");
        }
    }

    /// Sans MAC, la clé est ABSENTE — pas nulle, pas vide.
    #[test]
    fn sans_mac_la_cle_oui_n_est_pas_envoyee() {
        let p = charge_utile_correction(None, None, Some("X".into()), None, None, None);
        assert!(p.get("oui").is_none(), "clé oui envoyée sans MAC : {p}");
        let p = charge_utile_correction(
            None,
            None,
            Some("X".into()),
            None,
            None,
            Some("pas-une-mac"),
        );
        assert!(
            p.get("oui").is_none(),
            "OUI fabriqué depuis une non-MAC : {p}"
        );
    }
}
