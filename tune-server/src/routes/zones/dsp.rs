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
    let crossfeed_status =
        crossfeed_status_de_zone(&state, id, crossfeed["enabled"].as_bool().unwrap_or(false)).await;
    // #4685 — additif : un client qui l'ignore voit le même écran qu'avant.
    let level_compensation = compensation_de_niveau_de_zone(&state, id).await;

    match repo.get_dsp_config(id) {
        Ok((preset_id, enabled)) => Json(json!({
            "zone_id": id,
            "dsp_preset_id": preset_id,
            "dsp_enabled": enabled,
            "eq_profile": eq_profile.unwrap_or_default(),
            "crossfeed": crossfeed,
            "crossfeed_status": crossfeed_status,
            "level_compensation": level_compensation,
            "crossfeed_limits": crossfeed_limits(),
        }))
        .into_response(),
        Err(_) => Json(json!({
            "zone_id": id,
            "eq_profile": eq_profile.unwrap_or_default(),
            "crossfeed": crossfeed,
            "crossfeed_status": crossfeed_status,
            "level_compensation": level_compensation,
            "crossfeed_limits": crossfeed_limits(),
        }))
        .into_response(),
    }
}

/// #4685 — l'interrupteur de compensation de niveau et ce qu'il vaut sur
/// cette zone, pour l'écran.
///
/// ```json
/// { "enabled": true, "eq_db": -10.62, "crossfeed_db": -1.05,
///   "compensation_db": 11.67, "rendered_db": 3.0, "unrendered_db": 8.67,
///   "volume": 0.708, "local_output_only": false, "applied_by": "output_volume" }
/// ```
///
/// `eq_db` / `crossfeed_db` : ce que chaque étage fait au niveau MOYEN
/// (négatif = il en retire), calculé depuis le filtre par les mêmes
/// chargeurs que la lecture — 0 quand l'étage n'est pas actif sur la zone.
/// `compensation_db` : ce qui est rendu par le volume quand l'interrupteur
/// est ouvert, 0 sinon. C'est une DEMANDE : à volume plein, le rabot à
/// l'unité la mange (la ligne `local_gain_rabote_a_l_unite` le dit au
/// journal). `local_output_only` : faux depuis #5071 — une zone réseau la
/// reçoit aussi. `applied_by` dit PAR OÙ : `output_volume` (sortie locale,
/// par le volume raboté à l'unité) ou `stream_gain` (zone réseau, gain cuit
/// dans le flux après l'égaliseur, borné à la crête : il peut rendre MOINS
/// que `compensation_db` sur une piste dont les crêtes n'ont pas la place).
///
/// #5069 — `rendered_db` / `unrendered_db` : ce que le volume COURANT de la
/// zone peut réellement en rendre, et ce qu'il ne peut pas. Sur la sortie
/// locale (`applied_by = output_volume`), la demande est multipliée au volume
/// puis rabotée à l'unité (`effective_volume_units`, ligne
/// `local_gain_rabote_a_l_unite`) : au volume maximal, rien ne passe.
/// L'écran annonçait pourtant « +8.4 dB rendus par le volume » à 100 % — un
/// testeur perdait 8,4 dB sans que rien ne le lui dise. `volume` est le
/// volume linéaire (0..1) sur lequel ce partage est calculé. Le ReplayGain de
/// la piste, propre à chaque morceau, n'y entre pas. Sur une zone réseau
/// (`stream_gain`), le volume ne rabote pas le gain cuit dans le flux :
/// `rendered_db` y vaut la demande, `unrendered_db` 0.
pub(super) async fn compensation_de_niveau_de_zone(state: &AppState, zone_id: i64) -> Value {
    let enabled = state.orchestrator.zone_compensation_de_niveau(zone_id);
    let (eq_db, crossfeed_db) = state.orchestrator.gain_moyen_du_dsp_de_zone(zone_id);
    let sortie_locale = ZoneRepo::with_backend(state.backend.clone())
        .get(zone_id)
        .ok()
        .flatten()
        .and_then(|z| z.output_device_id)
        .is_none_or(|id| id.starts_with("local:"));
    // `+ 0.0` : pas de « -0 » dans le JSON quand rien n'est à rendre.
    let arrondi = |db: f64| (db * 100.0).round() / 100.0 + 0.0;
    let compensation_db = if enabled {
        arrondi(-(eq_db + crossfeed_db))
    } else {
        0.0
    };
    let volume = volume_de_zone(state, zone_id).await;
    // #5069 × #5071 : le partage par le volume ne vaut que pour la sortie
    // locale. Une zone réseau reçoit le gain cuit dans son flux, que le volume
    // ne rabote pas.
    let (rendu, non_rendu) = if sortie_locale {
        part_rendue_par_le_volume(compensation_db, volume)
    } else {
        (compensation_db, 0.0)
    };
    json!({
        "enabled": enabled,
        "eq_db": arrondi(eq_db),
        "crossfeed_db": arrondi(crossfeed_db),
        "compensation_db": compensation_db,
        "rendered_db": arrondi(rendu),
        "unrendered_db": arrondi(non_rendu),
        "volume": (volume * 1000.0).round() / 1000.0,
        "local_output_only": false,
        "applied_by": if sortie_locale { "output_volume" } else { "stream_gain" },
    })
}

/// #5069 — le volume linéaire (0..1) de la zone, pris à la même source que
/// `GET /zones/{id}` (`routes/zones/lecture.rs`) : l'état de lecture s'il est
/// connu, sinon la colonne persistée (échelle 0..100).
async fn volume_de_zone(state: &AppState, zone_id: i64) -> f64 {
    let vivant = state.playback.get_state(zone_id).await.volume;
    let v = if vivant > 0.0 {
        vivant
    } else {
        ZoneRepo::with_backend(state.backend.clone())
            .get(zone_id)
            .ok()
            .flatten()
            .map_or(1.0, |z| z.volume / 100.0)
    };
    if v.is_finite() {
        v.clamp(0.0, 1.0)
    } else {
        1.0
    }
}

/// #5069 — partage une compensation demandée entre ce que le volume `volume`
/// (linéaire, 0..1) rend et ce que le rabot à l'unité mange.
///
/// Le gain effectif est `volume × 10^(compensation/20)`, borné à 1 : la marge
/// disponible est donc `−20·log10(volume)` dB. Une compensation négative (une
/// atténuation) passe toujours en entier. Rend `(rendu, non_rendu)`, en dB.
pub(super) fn part_rendue_par_le_volume(compensation_db: f64, volume: f64) -> (f64, f64) {
    if !compensation_db.is_finite() {
        return (0.0, 0.0);
    }
    if compensation_db <= 0.0 {
        return (compensation_db, 0.0);
    }
    let marge_db = if volume <= 0.0 {
        f64::INFINITY
    } else {
        (-20.0 * volume.min(1.0).log10()).max(0.0)
    };
    let rendu = compensation_db.min(marge_db);
    (rendu, compensation_db - rendu)
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
    if let Some((fp, body)) = cache.lock().expect("convolver cache poisoned").get(&id)
        && *fp == fingerprint
    {
        return Json(body.clone()).into_response();
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

/// Les bornes du crossfeed, publiées pour que le bout des curseurs d'un client
/// soit EXACTEMENT celui que le serveur applique (#4683). `amount_max` est le
/// point mono (Side entièrement replié) : c'est lui que « 100 % » désigne.
///
/// #5081 — et celles de l'ombre de la tête. Leur présence est aussi ce qui
/// dit à un client que ce serveur connaît le filtre : un serveur d'avant ne
/// les publie pas, et le client cache alors ses contrôles.
pub(crate) fn crossfeed_limits() -> Value {
    json!({
        "amount_max": tune_core::audio::crossfeed::MAX_AMOUNT,
        "delay_ms_max": tune_core::audio::crossfeed::MAX_DELAY_MS,
        "cutoff_hz_min": tune_core::audio::crossfeed::COUPURE_MIN_HZ,
        "cutoff_hz_max": tune_core::audio::crossfeed::COUPURE_MAX_HZ,
        "slope_db_per_octave_min": tune_core::audio::crossfeed::PENTE_MIN_DB_OCT,
        "slope_db_per_octave_max": tune_core::audio::crossfeed::PENTE_MAX_DB_OCT,
    })
}

/// #5081 — les trois champs de l'ombre de la tête, normalisés et bornés, lus
/// dans `v`, et à défaut dans `repli` (le réglage déjà enregistré) : un client
/// d'avant #5081, qui n'envoie que `{ enabled, amount, delay_ms }`, ne ferme
/// pas le filtre qu'un autre écran a ouvert. Sans l'un ni l'autre : éteint,
/// 700 Hz, 6 dB/oct.
pub(crate) fn ombre_normalisee(v: &Value, repli: &Value) -> Value {
    let champ = |cle: &str| v.get(cle).or_else(|| repli.get(cle));
    let (cutoff_hz, slope) = tune_core::audio::crossfeed::borner_ombre(
        champ("cutoff_hz")
            .and_then(|c| c.as_f64())
            .unwrap_or(f64::from(tune_core::audio::crossfeed::COUPURE_DEFAUT_HZ)),
        champ("slope_db_per_octave")
            .and_then(|p| p.as_f64())
            .unwrap_or(f64::from(tune_core::audio::crossfeed::PENTE_DEFAUT_DB_OCT)),
    );
    json!({
        "head_shadow_enabled": champ("head_shadow_enabled")
            .and_then(|e| e.as_bool())
            .unwrap_or(false),
        "cutoff_hz": cutoff_hz,
        "slope_db_per_octave": slope,
    })
}

/// Read the `zone_{id}_crossfeed` settings row into a normalised JSON object,
/// falling back to defaults (disabled, amount 0.30, delay 0.30 ms) for any
/// missing/invalid field. Shape: `{ enabled, amount, delay_ms }`, plus the
/// #5081 head-shadow fields `{ head_shadow_enabled, cutoff_hz,
/// slope_db_per_octave }` (off, 700 Hz, 6 dB/oct when never written).
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
    let ombre = ombre_normalisee(&v, &Value::Null);
    json!({
        "enabled": enabled,
        "amount": amount,
        "delay_ms": delay_ms,
        "head_shadow_enabled": ombre["head_shadow_enabled"],
        "cutoff_hz": ombre["cutoff_hz"],
        "slope_db_per_octave": ombre["slope_db_per_octave"],
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
/// ⚠️ **Ce commentaire décrit l'état d'AVANT LAT-F1** et n'est gardé que pour
/// la trace du défaut d'origine. Depuis que le bras progressif porte le
/// crossfeed (`StreamingDsp`), une zone RÉSEAU peut l'entendre — sous deux
/// conditions que cette fonction va chercher : l'opt-in `dsp_progressif_reseau`
/// et le LPCM annoncé par le renderer.
///
/// La règle elle-même vit dans `tune_core::audio::crossfeed` et ne lit aucune
/// base : ici on ne fait que lui passer les faits qu'elle attend. Une seule
/// règle, donc pas de dérive possible entre cet écran et le son.
///
/// La sonde LPCM est interrogée à **16 bits**. C'est le plancher du bras
/// progressif : un renderer qui refuse déjà le LPCM 16 n'a aucun chemin, quelle
/// que soit la piste. Un renderer qui l'accepte en 16 et le refuse en 24 verra
/// son crossfeed s'appliquer sur les pistes 16 bits seulement — une vérité qui
/// dépend de la PISTE, que cette route ne connaît pas et ne prétend donc pas
/// dire. Le champ `detail` reste vrai dans les deux cas.
pub(super) async fn crossfeed_status_de_zone(
    state: &AppState,
    zone_id: i64,
    requested: bool,
) -> tune_core::audio::crossfeed::CrossfeedStatus {
    // #4511 — la sortie d'abord, les droits ensuite : voir `avec_les_droits`.
    // Ne jamais réécrire les réglages stockés quand l'accès change.
    let premium = state
        .license
        .check_feature(tune_core::license::Feature::Crossfeed)
        .await;
    let greffon_actif = tune_core::audio::premium_plugins::enabled(
        &SettingsRepo::with_backend(state.backend.clone()),
        "crossfeed",
    );
    let backend = &state.backend;
    let zone = ZoneRepo::with_backend(backend.clone())
        .get(zone_id)
        .ok()
        .flatten();
    let device = zone.as_ref().and_then(|z| z.output_device_id.clone());
    let est_reseau = tune_core::orchestrator::is_network_output_type(
        zone.as_ref().and_then(|z| z.output_type.as_deref()),
    );
    let progressif_arme = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone())
        .get("dsp_progressif_reseau")
        .ok()
        .flatten()
        .as_deref()
        == Some("true");
    // La sonde ne sert que pour une zone RÉSEAU : ne pas réveiller le réseau
    // pour une zone locale. Depuis #2742 (24/09), elle compte aussi opt-in
    // fermé : un renderer qui annonce le LPCM reçoit le crossfeed des pistes
    // de la bibliothèque en WAV progressif, et le statut doit le dire. C'est la
    // MÊME sonde que la résolution (`dlna_accepte_lpcm`, mémorisée par
    // renderer) : l'écran et le son se répondent sur une seule règle.
    let renderer_accepte_lpcm = match (&device, est_reseau) {
        (Some(did), true) if !did.is_empty() => {
            state.orchestrator.dlna_accepte_lpcm(did, false).await
        }
        _ => false,
    };
    let sortie = tune_core::audio::crossfeed::crossfeed_status(
        requested,
        tune_core::audio::crossfeed::crossfeed_runs_on_output(device.as_deref()),
        est_reseau,
        tune_core::audio::audiophile::zone_enabled(backend, zone_id),
        progressif_arme,
        renderer_accepte_lpcm,
    );
    tune_core::audio::crossfeed::avec_les_droits(sortie, premium, greffon_actif)
}

pub(super) async fn set_zone_dsp(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    // Authorize the whole request before any write: EQ is free, crossfeed is
    // separately Premium. A mixed request must never partially mutate EQ.
    if body.get("crossfeed").is_some()
        && let Err(resp) = crate::premium_guard::require_premium_localise(
            &state.license,
            tune_core::license::Feature::Crossfeed,
            &headers,
        )
        .await
    {
        return resp;
    }

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());

    for (key, plugin) in [("eq_profile", "equalizer"), ("crossfeed", "crossfeed")] {
        if body.get(key).is_some()
            && let Err(response) = crate::premium_audio_plugins::require_installed(&state, plugin)
        {
            return response;
        }
    }
    // Handle eq_profile if present
    let mut eq_applique_a_chaud = false;
    let mut eq_portee: Option<tune_core::orchestrator::PorteeDuReglage> = None;
    if let Some(eq_val) = body.get("eq_profile")
        && let Ok(profile) =
            serde_json::from_value::<tune_core::audio::eq::EqProfile>(eq_val.clone())
    {
        let key = format!("zone_{id}_eq_profile");
        let _ = settings.set(&key, &serde_json::to_string(&profile).unwrap_or_default());
        // Persister ne suffit pas : sans ceci le reglage n'atteint le son
        // qu'a la piste SUIVANTE sur une zone locale (#1725). `POST
        // /zones/{id}/eq` le fait deja ; cette route ecrit la MEME cle et
        // ne le faisait pas.
        let portee = state.orchestrator.apply_eq_change_portee(id).await;
        eq_applique_a_chaud = portee == tune_core::orchestrator::PorteeDuReglage::Immediate;
        eq_portee = Some(portee);
    }

    // Handle crossfeed sub-object if present (local-output headphone effect).
    // Separate Premium crossfeed gate above. Ranges clamped by
    // `tune_core::audio::crossfeed::borner` (amount 0..0.5, delay_ms 0..5 — la
    // même borne que les préréglages, #4684). Persisted to `zone_{id}_crossfeed`.
    let mut crossfeed_saved: Option<Value> = None;
    let mut cf_applique_a_chaud = false;
    let mut cf_portee: Option<tune_core::orchestrator::PorteeDuReglage> = None;
    // #2742 — publié dès que le corps porte un `crossfeed`, pour que la réponse
    // au CLIC dise déjà si le réglage aura le moindre effet.
    let mut crossfeed_status: Option<tune_core::audio::crossfeed::CrossfeedStatus> = None;
    if let Some(cf_val) = body.get("crossfeed") {
        let enabled = cf_val
            .get("enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let (amount, delay_ms) = tune_core::audio::crossfeed::borner(
            cf_val
                .get("amount")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.30),
            cf_val
                .get("delay_ms")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.30),
        );
        // #5081 — l'ombre de la tête ; un champ absent du corps garde la
        // valeur enregistrée (`ombre_normalisee`).
        let ombre = ombre_normalisee(cf_val, &read_crossfeed_config(&settings, id));
        let normalised = json!({
            "enabled": enabled,
            "amount": amount,
            "delay_ms": delay_ms,
            "head_shadow_enabled": ombre["head_shadow_enabled"],
            "cutoff_hz": ombre["cutoff_hz"],
            "slope_db_per_octave": ombre["slope_db_per_octave"],
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
        // (#1786). #4680 — et la réponse dit QUAND : un booléen seul
        // confondait « rien ne joue », « piste suivante » et un retrait à chaud.
        let portee = state.orchestrator.refresh_zone_crossfeed_portee(id).await;
        cf_applique_a_chaud = portee == tune_core::orchestrator::PorteeDuReglage::Immediate;
        cf_portee = Some(portee);
        // #2742 — et si la zone ne peut PAS faire tourner de crossfeed, le
        // serveur le dit au lieu d'enregistrer en silence. Journalisé au
        // moment du CLIC, pas à la lecture : c'est ici que l'utilisateur
        // croit avoir obtenu quelque chose.
        let statut = crossfeed_status_de_zone(&state, id, enabled).await;
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

    // #4685 — l'interrupteur de compensation de niveau. Pas de garde Premium :
    // il ne crée aucun traitement, il rend par le volume ce que l'égaliseur
    // (gratuit) ou le crossfeed (Premium, déjà gardé) retirent.
    let mut compensation_appliquee_a_chaud = false;
    // #5071 — quand la bascule s'entend, comme `eq_portee` ; `null` sans
    // `level_compensation` dans le corps.
    let mut compensation_portee: Option<tune_core::orchestrator::PorteeDuReglage> = None;
    if let Some(enabled) = body
        .get("level_compensation")
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
    {
        let cle = tune_core::orchestrator::PlaybackOrchestrator::cle_compensation_de_niveau(id);
        let _ = settings.set(&cle, if enabled { "true" } else { "false" });
        // #5071 — sortie locale : à chaud, par le volume. Zone réseau : le
        // flux porte la compensation, il est refabriqué par le chemin même
        // d'un changement d'égaliseur (anti-rebond, plancher, flux conservé
        // quand rien ne change).
        let portee = state
            .orchestrator
            .apply_compensation_change_portee(id)
            .await;
        compensation_appliquee_a_chaud =
            portee == tune_core::orchestrator::PorteeDuReglage::Immediate;
        compensation_portee = Some(portee);
    }
    // Rendu à CHAQUE écriture : changer l'égaliseur ou le crossfeed change
    // aussi ce que la compensation rend.
    let level_compensation = compensation_de_niveau_de_zone(&state, id).await;

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
        // #4680 — quand l'égaliseur s'entend (`immediate`, `restart`,
        // `next_track`, `not_playing`) ; `null` sans `eq_profile` dans le corps.
        "eq_portee": eq_portee.map(|p| p.code()),
        // Idem pour le crossfeed (#1786).
        "crossfeed_applied_live": cf_applique_a_chaud,
        // #4680 — même contrat que `eq_portee` ; `null` sans `crossfeed`.
        "crossfeed_portee": cf_portee.map(|p| p.code()),
        // #4685 — l'interrupteur et ce qu'il rend, après cette écriture.
        "level_compensation": level_compensation,
        "level_compensation_applied_live": compensation_appliquee_a_chaud,
        "level_compensation_portee": compensation_portee.map(|p| p.code()),
    }))
    .into_response()
}

/// Preview the selected provider's prepared coefficients. The caller specifies
/// the intended rate; this endpoint never labels a preview as live measurement.
#[derive(serde::Deserialize)]
pub(super) struct EqResponseQuery {
    sample_rate: Option<u32>,
    channels: Option<u16>,
}
pub(super) async fn eq_response(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    axum::extract::Query(query): axum::extract::Query<EqResponseQuery>,
) -> axum::response::Response {
    let sample_rate = query.sample_rate.unwrap_or(44100);
    let channels = query.channels.unwrap_or(2);
    if !(8000..=768000).contains(&sample_rate) || !(1..=32).contains(&channels) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error":"invalid_format"})),
        )
            .into_response();
    }
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let profile = settings
        .get(&format!("zone_{id}_eq_profile"))
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    match tokio::task::spawn_blocking(move || {
        tune_core::audio::eq::EqProcessor::new(&profile, sample_rate, channels)
            .response(sample_rate)
    })
    .await
    {
        Ok(response) => {
            Json(json!({"zone_id":id,"configuration_preview":true,"response":response}))
                .into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error":"response_failed"})),
        )
            .into_response(),
    }
}
