use super::*;

pub(super) async fn get_zone_dsp(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let eq_key = format!("zone_{id}_eq_profile");
    let eq_profile: Option<tune_core::audio::eq::EqProfile> = settings
        .get(&eq_key)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());

    // Headphone crossfeed config (local output only). Defaults when unset:
    // disabled, amount 0.30, delay 0.30 ms.
    let crossfeed = read_crossfeed_config(&settings, id);
    // …et ce que ce réglage VAUT sur CETTE zone (#2742). Additif : l'objet
    // `crossfeed` ci-dessus est publié tel quel, un client qui ignore ce
    // champ voit le même écran qu'avant.
    let crossfeed_status = crossfeed_status_de_zone(
        &state.backend,
        id,
        crossfeed["enabled"].as_bool().unwrap_or(false),
    );

    match repo.get_dsp_config(id) {
        Ok((preset_id, enabled)) => Json(json!({
            "zone_id": id,
            "dsp_preset_id": preset_id,
            "dsp_enabled": enabled,
            "eq_profile": eq_profile.unwrap_or_default(),
            "crossfeed": crossfeed,
            "crossfeed_status": crossfeed_status,
        }))
        .into_response(),
        Err(_) => Json(json!({
            "zone_id": id,
            "eq_profile": eq_profile.unwrap_or_default(),
            "crossfeed": crossfeed,
            "crossfeed_status": crossfeed_status,
        }))
        .into_response(),
    }
}

/// Cache of computed convolver responses, keyed by zone id. The value pairs
/// the filter fingerprint (path + size + mtime) with the full response body:
/// re-uploading an IR rewrites the file, so the fingerprint changes and the
/// entry is recomputed on the next read — no explicit invalidation hook needed.
pub(super) static CONVOLVER_RESPONSE_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<i64, (String, Value)>>,
> = std::sync::OnceLock::new();

/// `GET /zones/{id}/convolver/response` — frequency response of the zone's FIR
/// convolver, for visualisation. Not premium-gated: applying an IR is, reading
/// the resulting curve is not.
///
/// The running convolver only keeps its IR in FFT-partitioned form, so the taps
/// are re-read from the persisted IR file (`ir_path_{zone_id}` setting — same
/// source of truth as `restore_convolvers` and the transcode path). Multi-
/// channel IRs are summarised by channel 0: averaging L/R taps would let
/// inter-channel phase differences cancel and distort the magnitude curve.
pub(super) async fn convolver_response(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    match repo.get(id) {
        Ok(Some(_)) => {}
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "zone not found"})),
            )
                .into_response();
        }
    }

    let ir_path = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get(&format!("ir_path_{id}"))
        .ok()
        .flatten()
        .filter(|p| !p.is_empty());
    let Some(ir_path) = ir_path else {
        return Json(json!({"loaded": false})).into_response();
    };
    let Ok(meta) = std::fs::metadata(&ir_path) else {
        // Path persisted but file gone (moved data dir…): nothing to plot.
        return Json(json!({"loaded": false})).into_response();
    };
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let fingerprint = format!("{ir_path}|{}|{mtime}", meta.len());

    let cache = CONVOLVER_RESPONSE_CACHE.get_or_init(Default::default);
    if let Some((fp, body)) = cache.lock().expect("convolver cache poisoned").get(&id) {
        if *fp == fingerprint {
            return Json(body.clone()).into_response();
        }
    }

    // ~200 log-spaced points × up to 128k taps of f64 accumulation: fast, but
    // not "handler on the async runtime" fast — compute on the blocking pool.
    let computed = tokio::task::spawn_blocking(move || -> Result<Value, String> {
        let (ir, sample_rate) = tune_core::audio::convolver::Convolver::read_ir_taps(&ir_path)?;
        if sample_rate == 0 {
            return Err("IR sample rate is 0".into());
        }
        let taps = &ir[0]; // channel 0 (see handler doc)
        let f_hi = 20_000.0f64.min(sample_rate as f64 * 0.45);
        let freqs = tune_core::audio::convolver::log_freq_grid(200, 20.0, f_hi);
        let points: Vec<Value> =
            tune_core::audio::convolver::fir_frequency_response(taps, sample_rate, &freqs)
                .into_iter()
                .map(|p| {
                    json!({
                        "f": (p.freq_hz * 10.0).round() / 10.0,
                        "db": (p.magnitude_db * 100.0).round() / 100.0,
                        "phase_deg": (p.phase_deg * 100.0).round() / 100.0,
                    })
                })
                .collect();
        let latency_ms = taps.len() as f64 / 2.0 / sample_rate as f64 * 1000.0;
        Ok(json!({
            "loaded": true,
            "taps": taps.len(),
            "sample_rate": sample_rate,
            "latency_ms": (latency_ms * 10.0).round() / 10.0,
            "points": points,
        }))
    })
    .await;

    match computed {
        Ok(Ok(body)) => {
            cache
                .lock()
                .expect("convolver cache poisoned")
                .insert(id, (fingerprint, body.clone()));
            Json(body).into_response()
        }
        Ok(Err(e)) => {
            warn!(zone_id = id, error = %e, "convolver_response_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("read IR: {e}")})),
            )
                .into_response()
        }
        Err(e) => {
            warn!(zone_id = id, error = %e, "convolver_response_join_failed");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "response computation failed"})),
            )
                .into_response()
        }
    }
}

/// Read the `zone_{id}_crossfeed` settings row into a normalised JSON object,
/// falling back to defaults (disabled, amount 0.30, delay 0.30 ms) for any
/// missing/invalid field. Shape: `{ enabled, amount, delay_ms }`.
pub(super) fn read_crossfeed_config(
    settings: &tune_core::db::settings_repo::SettingsRepo,
    id: i64,
) -> Value {
    let stored: Option<Value> = settings
        .get(&format!("zone_{id}_crossfeed"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok());
    let v = stored.unwrap_or(Value::Null);
    let enabled = v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(false);
    let amount = v.get("amount").and_then(|a| a.as_f64()).unwrap_or(0.30);
    let delay_ms = v.get("delay_ms").and_then(|d| d.as_f64()).unwrap_or(0.30);
    json!({
        "enabled": enabled,
        "amount": amount,
        "delay_ms": delay_ms,
    })
}

/// Ce que le crossfeed VAUT sur cette zone-ci, à côté de ce que le réglage
/// demande — #2742.
///
/// Le crossfeed n'est installé qu'à trois endroits, tous derrière la même
/// double garde `device_id.starts_with("local:")` +
/// `downcast_ref::<LocalOutput>()` (`orchestrator.rs` : chemin de lecture,
/// `refresh_zone_crossfeed`, `refresh_zone_pure_dsp`). Une zone réseau n'a donc
/// aucun chemin de code — pendant que cette route-ci offrait le réglage, le
/// persistait, et le relisait sans un mot. Tades : « Crossfeed n'a aucune
/// action ».
///
/// La règle elle-même vit dans `tune_core::audio::crossfeed` et ne lit aucune
/// base : ici on ne fait que lui passer les deux faits qu'elle attend — la
/// sortie de la zone et son mode PURE. Une seule règle, donc pas de dérive
/// possible entre cet écran et le son.
pub(super) fn crossfeed_status_de_zone(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    zone_id: i64,
    requested: bool,
) -> tune_core::audio::crossfeed::CrossfeedStatus {
    let device = ZoneRepo::with_backend(backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id);
    tune_core::audio::crossfeed::crossfeed_status(
        requested,
        tune_core::audio::crossfeed::crossfeed_runs_on_output(device.as_deref()),
        tune_core::audio::audiophile::zone_enabled(backend, zone_id),
    )
}

pub(super) async fn set_zone_dsp(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    // Premium gate: DSP & EQ mutations require Premium. Le refus parle la
    // langue de l'application (#2419) — c'est le même écran « Égaliseur » que
    // `POST /zones/{id}/eq`, et il tire ses deux moitiés d'ici et de là.
    if let Err(resp) = crate::premium_guard::require_premium_localise(
        &state.license,
        tune_core::license::Feature::DspEq,
        &headers,
    )
    .await
    {
        return resp;
    }

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());

    // Handle eq_profile if present
    let mut eq_applique_a_chaud = false;
    if let Some(eq_val) = body.get("eq_profile") {
        if let Ok(profile) =
            serde_json::from_value::<tune_core::audio::eq::EqProfile>(eq_val.clone())
        {
            let key = format!("zone_{id}_eq_profile");
            let _ = settings.set(&key, &serde_json::to_string(&profile).unwrap_or_default());
            // Persister ne suffit pas : sans ceci le reglage n'atteint le son
            // qu'a la piste SUIVANTE sur une zone locale (#1725). `POST
            // /zones/{id}/eq` le fait deja ; cette route ecrit la MEME cle et
            // ne le faisait pas.
            eq_applique_a_chaud = state.orchestrator.apply_eq_change(id).await;
        }
    }

    // Handle crossfeed sub-object if present (local-output headphone effect).
    // Same premium gate (Feature::DspEq) as the EQ path above. Ranges clamped:
    // amount 0..0.5, delay_ms 0..5. Persisted to `zone_{id}_crossfeed`.
    let mut crossfeed_saved: Option<Value> = None;
    let mut cf_applique_a_chaud = false;
    // #2742 — publié dès que le corps porte un `crossfeed`, pour que la réponse
    // au CLIC dise déjà si le réglage aura le moindre effet.
    let mut crossfeed_status: Option<tune_core::audio::crossfeed::CrossfeedStatus> = None;
    if let Some(cf_val) = body.get("crossfeed") {
        let enabled = cf_val
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let amount = cf_val
            .get("amount")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.30)
            .clamp(0.0, 0.5);
        let delay_ms = cf_val
            .get("delay_ms")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.30)
            .clamp(0.0, 5.0);
        let normalised = json!({
            "enabled": enabled,
            "amount": amount,
            "delay_ms": delay_ms,
        });
        let key = format!("zone_{id}_crossfeed");
        let _ = settings.set(
            &key,
            &serde_json::to_string(&normalised).unwrap_or_default(),
        );
        crossfeed_saved = Some(normalised);
        // Meme raison que pour l'egaliseur juste au-dessus : persister ne
        // suffit pas. Sans ceci, activer le crossfeed ou deplacer `amount` /
        // `delay_ms` en ecoutant ne changeait rien avant la piste suivante
        // (#1786).
        cf_applique_a_chaud = state.orchestrator.refresh_zone_crossfeed(id).await;
        // #2742 — et si la zone ne peut PAS faire tourner de crossfeed, le
        // serveur le dit au lieu d'enregistrer en silence. Journalisé au
        // moment du CLIC, pas à la lecture : c'est ici que l'utilisateur
        // croit avoir obtenu quelque chose.
        let statut = crossfeed_status_de_zone(&state.backend, id, enabled);
        if statut.unavailable {
            warn!(
                zone_id = id,
                requested = enabled,
                reason = statut
                    .reason
                    .map(tune_core::audio::crossfeed::CrossfeedConstraint::code),
                "zone_crossfeed_sans_effet"
            );
        }
        crossfeed_status = Some(statut);
    }

    let preset_id = body["dsp_preset_id"].as_i64();
    let enabled = body["dsp_enabled"].as_bool().unwrap_or(false);
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let _ = repo.update_dsp(id, preset_id, enabled);

    Json(json!({
        "zone_id": id,
        "dsp_preset_id": preset_id,
        "dsp_enabled": enabled,
        "eq_profile": body.get("eq_profile"),
        "crossfeed": crossfeed_saved,
        // #2742 — la moitié qui manquait : ce que ce réglage VAUT sur cette
        // zone. `null` quand le corps ne portait pas de `crossfeed` (rien n'a
        // été demandé, il n'y a rien à répondre). `unavailable: true` doit
        // VERROUILLER le contrôle côté client, `detail` l'expliquer.
        "crossfeed_status": crossfeed_status,
        // Meme contrat que `POST /zones/{id}/eq` : vrai quand le reglage vient
        // d'atteindre le son d'un flux en cours. Faux ne signale PAS un echec
        // (rien ne joue, zone non locale, mode PURE) — c'est ce qui permet a un
        // client de dire « prendra effet a la piste suivante » au lieu de
        // laisser croire a un egaliseur muet.
        "eq_applied_live": eq_applique_a_chaud,
        // Idem pour le crossfeed (#1786).
        "crossfeed_applied_live": cf_applique_a_chaud,
    }))
    .into_response()
}
