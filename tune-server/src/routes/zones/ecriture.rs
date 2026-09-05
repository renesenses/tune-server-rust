use super::*;

/// Les valeurs que `output_type` peut prendre — celles que l'orchestrateur sait
/// router (`orchestrator.rs`). Une zone dont le type est inconnu ne joue nulle
/// part : la refuser à l'écriture vaut mieux que la découvrir au premier « Lire ».
pub(super) const TYPES_DE_SORTIE: [&str; 8] = [
    "local",
    "browser",
    "dlna",
    "openhome",
    "chromecast",
    "bluos",
    "squeezebox",
    "oaat",
];

/// Les modes DSD reconnus par `should_dsd_passthrough` et `dop_requested`.
/// Tout le reste retombe dans le fourre-tout « auto » sans le dire.
pub(super) const MODES_DSD: [&str; 4] = ["auto", "native", "pcm", "dop"];

/// Une écriture du PATCH a échoué côté base : **journaliser**, puis 500.
///
/// Ces retours étaient muets : trente blocs rendaient
/// `(INTERNAL_SERVER_ERROR, e)` sans qu'aucune ligne ne parte dans les
/// journaux. Un 500 signalé par un testeur ne laissait donc **aucune trace
/// exploitable** — c'est ce qui a rendu #1964 impossible à instruire, et il a
/// fallu écrire à Gérard pour lui demander le corps de la réponse que le
/// serveur avait déjà entre les mains.
pub(super) fn echec_ecriture(
    zone_id: i64,
    champ: &str,
    valeur: &str,
    erreur: String,
) -> axum::response::Response {
    tracing::error!(
        zone_id,
        champ,
        valeur,
        erreur = %erreur,
        "zone_patch_write_failed"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("écriture impossible du champ « {champ} » : {erreur}"),
    )
        .into_response()
}

/// La requête elle-même est fautive : **journaliser**, puis 400.
///
/// 500 veut dire « le serveur a un défaut ». L'envoyer pour une valeur que le
/// client aurait pu corriger lui interdit de faire la différence entre ce qu'il
/// doit réparer et ce qu'il doit signaler.
pub(super) fn refus_de_valeur(
    zone_id: i64,
    champ: &str,
    valeur: &str,
    raison: &str,
) -> axum::response::Response {
    warn!(zone_id, champ, valeur, raison, "zone_patch_rejected");
    (
        StatusCode::BAD_REQUEST,
        format!("champ « {champ} » : {raison} (reçu : « {valeur} »)"),
    )
        .into_response()
}

pub(super) async fn patch_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PatchZone>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());

    // La zone existe-t-elle ? Sans ce contrôle, un PATCH sur un identifiant
    // inconnu exécutait la trentaine d'UPDATE — qui touchent zéro ligne et
    // réussissent — avant que `get_zone` ne rende 404 tout à la fin. Le 404
    // était juste, mais il arrivait après trente écritures inutiles et ne
    // disait pas laquelle avait échoué en cas de vrai problème.
    let zone_before = match repo.get(id) {
        Ok(Some(zone)) => zone,
        Ok(None) => {
            warn!(zone_id = id, "zone_patch_unknown_zone");
            return (StatusCode::NOT_FOUND, format!("zone {id} inconnue")).into_response();
        }
        Err(e) => return echec_ecriture(id, "zone", &id.to_string(), e),
    };

    let volume_demande = match valider_le_patch(id, &zone_before, &body) {
        Ok(volume) => volume,
        Err(reponse) => return reponse,
    };
    // Volume et mute sont des commandes, pas de simples préférences. Le
    // renderer doit les accepter avant qu'un PATCH puisse annoncer leur
    // réussite ou laisser une valeur mensongère en base. Si le PATCH change
    // aussi de sortie, la commande vise explicitement la nouvelle sortie.
    let command_device_id = body
        .output_device_id
        .as_deref()
        .or(zone_before.output_device_id.as_deref());
    if let Err(reponse) =
        commander_la_sortie(&state, id, command_device_id, &body, volume_demande).await
    {
        return reponse;
    }
    if let Err(reponse) =
        persister_le_patch(&state, &repo, id, &zone_before, command_device_id, &body).await
    {
        return reponse;
    }

    if body.brand.is_some() || body.model.is_some() {
        push_device_correction(&state, id).await;
        // Dans la foulée : les réglages qui marchent chez cet utilisateur pour
        // cet appareil identifié (#1743) — c'est au moment où il nomme son
        // appareil qu'on sait à quoi rattacher le préréglage.
        push_device_preset(&state, id).await;
    }

    get_zone(State(state), Path(id)).await.into_response()
}

/// Premier temps de `patch_zone` : les refus que la route prononce seule,
/// avant toute écriture. Rend le volume linéaire demandé, s'il y en a un.
/// Bloc sorti tel quel (REF-4 phase 2, #2219), ses `return` enrobés d'`Err`.
fn valider_le_patch(
    id: i64,
    zone_before: &Zone,
    body: &PatchZone,
) -> Result<Option<f64>, axum::response::Response> {
    // Les valeurs que cette route peut juger seule, avant toute écriture : un
    // PATCH est atomique du point de vue de l'utilisateur, il ne doit pas
    // laisser la moitié de ses champs écrits derrière lui.
    if let Some(ref ot) = body.output_type
        && !TYPES_DE_SORTIE.contains(&ot.as_str())
    {
        return Err(refus_de_valeur(
            id,
            "output_type",
            ot,
            &format!(
                "type de sortie inconnu (attendu : {})",
                TYPES_DE_SORTIE.join(", ")
            ),
        ));
    }
    if let Some(ref mode) = body.dsd_mode
        && !MODES_DSD.contains(&mode.as_str())
    {
        return Err(refus_de_valeur(
            id,
            "dsd_mode",
            mode,
            &format!("mode DSD inconnu (attendu : {})", MODES_DSD.join(", ")),
        ));
    }
    // #2271 — un mode inconnu est REFUSE, jamais range en base. Sans ce
    // garde-fou une faute de frappe s'ecrirait telle quelle et la lecture
    // tolerante de `get_autoplay_mode` la rattraperait en `similar` : la zone
    // se mettrait a enchainer alors que l'auditeur croyait l'eteindre.
    if let Some(ref mode) = body.autoplay_mode
        && AutoplayMode::from_str_stocke(mode).is_none()
    {
        return Err(refus_de_valeur(
            id,
            "autoplay_mode",
            mode,
            &format!(
                "mode de continuation inconnu (attendu : {})",
                AutoplayMode::NOMS.join(", ")
            ),
        ));
    }
    if let Some(vol) = body.volume
        && !(0..=100).contains(&vol)
    {
        return Err(refus_de_valeur(
            id,
            "volume",
            &vol.to_string(),
            "hors de 0..100",
        ));
    }
    // #1274 — `volume` et `volume_db` sont exclusifs, et la validation du dB
    // vit dans `volume_scale`. Ce PATCH ne peut pas déléguer complètement :
    // son champ historique est un entier 0..100, il doit donc le ramener sur
    // 0..1 lui-même. Le refus, lui, est rendu sous la forme que le reste du
    // handler emploie.
    let volume_demande = match tune_core::audio::volume_scale::demande_lineaire(
        body.volume.map(f64::from).map(|v| v / 100.0),
        body.volume_db,
    ) {
        Ok(v) => Some(v),
        // Aucun des deux champs n'est présent : ce PATCH ne parle pas de
        // volume, et c'est le cas le plus courant.
        Err(_) if body.volume.is_none() && body.volume_db.is_none() => None,
        Err(motif) => {
            let recu = match (body.volume, body.volume_db) {
                (Some(v), Some(db)) => format!("volume={v} volume_db={db}"),
                (_, Some(db)) => db.to_string(),
                (Some(v), _) => v.to_string(),
                _ => String::new(),
            };
            return Err(refus_de_valeur(id, "volume_db", &recu, motif));
        }
    };
    if let Some(ref device_id) = body.output_device_id
        && device_id.trim().is_empty()
    {
        // Une chaîne vide n'efface pas la sortie, elle la rend introuvable :
        // la zone reste « configurée » et ne joue nulle part.
        return Err(refus_de_valeur(
            id,
            "output_device_id",
            device_id,
            "vide — pour retirer la sortie, envoyer output_type",
        ));
    }
    if let Some(ref name) = body.name
        && name.trim().is_empty()
    {
        return Err(refus_de_valeur(id, "name", name, "vide"));
    }

    // Ce refus précède strictement la première écriture : un PATCH qui porte
    // d'autres champs ne doit rien modifier si l'accord manque.
    if fixed_volume_confirmation_required(&zone_before, &body) {
        warn!(zone_id = id, "fixed_volume_confirmation_required");
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "full_volume_confirmation_required",
                "message": "Enabling fixed volume raises this zone to full scale (100%). Confirm with `confirm_full_volume` to proceed.",
            })),
        )
            .into_response());
    }

    // Volume et mute sont des commandes, pas de simples préférences. Le
    // renderer doit les accepter avant qu'un PATCH puisse annoncer leur
    // réussite ou laisser une valeur mensongère en base. Si le PATCH change
    // aussi de sortie, la commande vise explicitement la nouvelle sortie.
    Ok(volume_demande)
}

/// Deuxième temps : volume et sourdine sont des commandes à la sortie, pas
/// des préférences. Bloc sorti tel quel, ses `return` enrobés d'`Err`.
async fn commander_la_sortie(
    state: &AppState,
    id: i64,
    command_device_id: Option<&str>,
    body: &PatchZone,
    volume_demande: Option<f64>,
) -> Result<(), axum::response::Response> {
    // #1274 — même garde-fou que sur PUT/POST …/volume : ce PATCH est la
    // troisième porte d'écriture du volume, et la consigne y arrive aussi en
    // dB. `command_device_id` porte déjà la sortie VISÉE, celle que ce même
    // PATCH est peut-être en train d'attribuer.
    if let Some(db) = body.volume_db
        && let Some(motif) = refus_de_resolution_volume(&state, command_device_id, db).await
    {
        return Err(refus_de_valeur(id, "volume_db", &db.to_string(), &motif));
    }

    // #1274 — `volume_demande` porte déjà la valeur linéaire, qu'elle vienne
    // du pour-cent entier ou des dB. L'orchestrateur la reçoit en `f64` et la
    // garde telle quelle dans l'état de lecture, vers le device et en base
    // (la colonne n'arrondit plus au pour-cent depuis #2886).
    if let Some(volume) = volume_demande
        && let Err(error) = state
            .orchestrator
            .set_volume(id, volume, command_device_id)
            .await
    {
        return Err(crate::routes::playback::output_command_error_response(
            error,
        ));
    }
    if let Some(muted) = body.muted
        && let Err(error) = state
            .orchestrator
            .set_mute(id, muted, command_device_id)
            .await
    {
        return Err(crate::routes::playback::output_command_error_response(
            error,
        ));
    }

    /// Écrit un champ, ou s'arrête en journalisant la cause.
    ///
    /// Une macro et non une closure : chaque échec doit **sortir** du handler,
    /// et une closure ne peut pas rendre la main à sa place. C'est aussi ce qui
    /// garantit qu'aucun des trente blocs ne puisse redevenir muet — il n'y a
    /// plus qu'un seul endroit où le `return` est écrit.
    Ok(())
}

/// Troisième temps : chaque préférence persistée par la macro `ecrire!`, qui
/// journalise l'échec, et les rafraîchissements de la sortie vivante qui
/// l'accompagnent. Bloc sorti tel quel, ses `return` enrobés d'`Err`.
async fn persister_le_patch(
    state: &AppState,
    repo: &ZoneRepo,
    id: i64,
    zone_before: &Zone,
    command_device_id: Option<&str>,
    body: &PatchZone,
) -> Result<(), axum::response::Response> {
    /// Écrit un champ, ou s'arrête en journalisant la cause.
    ///
    /// Une macro et non une closure : chaque échec doit **sortir** du handler,
    /// et une closure ne peut pas rendre la main à sa place. C'est aussi ce qui
    /// garantit qu'aucun des trente blocs ne puisse redevenir muet — il n'y a
    /// plus qu'un seul endroit où le `return` est écrit.
    macro_rules! ecrire {
        ($champ:literal, $valeur:expr, $ecriture:expr) => {
            if let Err(e) = $ecriture {
                return Err(echec_ecriture(id, $champ, &$valeur.to_string(), e));
            }
        };
    }

    if let Some(ref name) = body.name {
        ecrire!("name", name, repo.update_name(id, name));
    }
    // volume/muted ont été confirmés et persistés par l'orchestrateur ci-dessus.
    if let Some(ref device_id) = body.output_device_id {
        ecrire!(
            "output_device_id",
            device_id,
            repo.update_output_device(id, device_id)
        );
    }
    if let Some(ref ot) = body.output_type {
        ecrire!("output_type", ot, repo.update_output_type(id, ot));
    }
    if let Some(gapless) = body.gapless_enabled {
        ecrire!(
            "gapless_enabled",
            gapless,
            repo.update_gapless_enabled(id, gapless)
        );
    }
    if let Some(ms) = body.sync_delay_ms {
        ecrire!("sync_delay_ms", ms, repo.update_sync_delay(id, ms));
    }
    if let Some(rate) = body.max_sample_rate {
        ecrire!(
            "max_sample_rate",
            rate.map(|r| r.to_string()).unwrap_or_else(|| "null".into()),
            repo.update_max_sample_rate(id, rate)
        );
    }
    if let Some(fixed) = body.fixed_volume {
        // #2395 — le mode bit-perfect fait UN saut, annoncé et réversible.
        //
        // Seules les TRANSITIONS agissent : un PATCH qui réaffirme l'état
        // courant ne commande rien. C'est ce qui rend le saut unique — sans
        // cette garde, chaque `{"fixed_volume": true}` d'un client bavard
        // renverrait 100 % à l'appareil, et on aurait remplacé la réassertion
        // à la lecture par une réassertion au PATCH.
        let etait_fixe = zone_before.fixed_volume;
        ecrire!("fixed_volume", fixed, repo.update_fixed_volume(id, fixed));
        if fixed && !etait_fixe {
            // Mémoriser AVANT de commander : une fois le 100 % appliqué, la
            // valeur d'origine n'est plus lisible nulle part. L'échec de la
            // mémorisation coûte la restauration, pas le mode — il est dit au
            // journal, il n'interrompt pas l'armement.
            if let Err(error) =
                tune_core::audio::fixed_volume::remember(&state.backend, id, zone_before.volume)
            {
                warn!(zone_id = id, %error, "fixed_volume_memoire_non_ecrite");
            }
            // `arm_fixed_volume` et non `set_volume` : ce dernier sort au plus
            // tôt sur une zone désormais `fixed_volume` et ne parlerait pas au
            // device. C'est ici, et nulle part ailleurs, que le 100 % part.
            if let Err(error) = state
                .orchestrator
                .arm_fixed_volume(id, command_device_id)
                .await
            {
                return Err(crate::routes::playback::output_command_error_response(
                    error,
                ));
            }
        } else if !fixed && etait_fixe {
            // Sortie du mode : rendre le volume d'avant. `update_fixed_volume`
            // est déjà écrit ci-dessus, donc `set_volume` ne sort plus au plus
            // tôt et commande réellement l'appareil.
            //
            // Sans mémoire (zone armée par une version antérieure à ce
            // correctif, ou écriture perdue), on ne devine pas : la zone reste
            // à 100 % et l'utilisateur garde la main. Commander une valeur
            // inventée serait le défaut qu'on corrige, à l'envers.
            match tune_core::audio::fixed_volume::take(&state.backend, id) {
                Some(pourcent) => {
                    if let Err(error) = state
                        .orchestrator
                        .set_volume(id, pourcent / 100.0, command_device_id)
                        .await
                    {
                        return Err(crate::routes::playback::output_command_error_response(
                            error,
                        ));
                    }
                    info!(zone_id = id, volume = pourcent, "fixed_volume_restaure");
                }
                None => info!(zone_id = id, "fixed_volume_sans_memoire_rien_a_restaurer"),
            }
        }
    }
    // #2271 — les deux champs visent la MEME colonne. `autoplay_mode` est le
    // plus precis, il gagne ; `autoplay_enabled` n'est applique que seul, pour
    // que les clients qui ne connaissent que lui continuent de fonctionner.
    if let Some(ref mode) = body.autoplay_mode {
        // Deja valide plus haut : le `unwrap_or` n'est pas atteignable.
        let mode = AutoplayMode::from_str_stocke(mode).unwrap_or_default();
        ecrire!(
            "autoplay_mode",
            mode.as_str(),
            repo.update_autoplay_mode(id, mode)
        );
    } else if let Some(autoplay) = body.autoplay_enabled {
        ecrire!(
            "autoplay_enabled",
            autoplay,
            repo.update_autoplay_enabled(id, autoplay)
        );
    }
    if let Some(ref mode) = body.dsd_mode {
        ecrire!("dsd_mode", mode, repo.update_dsd_mode(id, mode));
    }
    if let Some(offset) = body.lyrics_offset_ms {
        // Borne large mais finie : au-dela d'une minute ce n'est plus un
        // reglage de latence, et une valeur folle desynchroniserait tout.
        let clamped = offset.clamp(-60_000, 60_000);
        ecrire!(
            "lyrics_offset_ms",
            clamped,
            repo.update_lyrics_offset_ms(id, clamped)
        );
    }
    if let Some(native_flac) = body.dlna_native_flac {
        ecrire!(
            "dlna_native_flac",
            native_flac,
            repo.update_dlna_native_flac(id, native_flac)
        );
    }
    if let Some(passthrough) = body.alac_passthrough {
        ecrire!(
            "alac_passthrough",
            passthrough,
            repo.update_alac_passthrough(id, passthrough)
        );
    }
    if let Some(passthrough) = body.aac_passthrough {
        ecrire!(
            "aac_passthrough",
            passthrough,
            repo.update_aac_passthrough(id, passthrough)
        );
    }
    if let Some(lpcm) = body.dlna_lpcm {
        ecrire!("dlna_lpcm", lpcm, repo.update_dlna_lpcm(id, lpcm));
    }
    if let Some(cap) = body.dlna_cap_16bit {
        ecrire!("dlna_cap_16bit", cap, repo.update_dlna_cap_16bit(id, cap));
    }
    if let Some(wav24) = body.dlna_wav24 {
        ecrire!("dlna_wav24", wav24, repo.update_dlna_wav24(id, wav24));
    }
    if let Some(delay) = body.dlna_play_delay_ms {
        let delay = delay.max(0) as u64;
        ecrire!(
            "dlna_play_delay_ms",
            delay,
            repo.update_dlna_play_delay_ms(id, delay)
        );
        // Apply live to the already-registered output so the new delay takes
        // effect on the next play without a rebuild/restart. 0 = fall back to the
        // config default (`[device_delays]` / `dlna_play_delay_ms`) by name.
        if let Some(device_id) = repo.get(id).ok().flatten().and_then(|z| z.output_device_id) {
            let output = { state.outputs.lock().await.get(&device_id) };
            if let Some(output) = output {
                let guard = output.lock().await;
                // `name()` is an OutputTarget trait method → read it on the trait
                // object before downcasting to the concrete DlnaOutput.
                let effective = if delay > 0 {
                    delay
                } else {
                    state.config.play_delay_for(guard.name())
                };
                if let Some(dlna) = guard.as_any().downcast_ref::<DlnaOutput>() {
                    dlna.set_play_delay(effective);
                }
            }
        }
    }
    // Marque / modèle choisis par l'utilisateur → settings zone_{id}_brand/model.
    // Chaîne vide = suppression de l'override (retour à la détection UPnP).
    if let Some(ref brand) = body.brand {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{id}_brand");
        let r = if brand.trim().is_empty() {
            settings.delete(&key)
        } else {
            settings.set(&key, brand.trim())
        };
        ecrire!("brand", brand, r);
    }
    if let Some(ref model) = body.model {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{id}_model");
        let r = if model.trim().is_empty() {
            settings.delete(&key)
        } else {
            settings.set(&key, model.trim())
        };
        ecrire!("model", model, r);
    }
    // Opt-in MediaRenderer UPnP (#1750) → setting zone_{id}_upnp_renderer.
    if let Some(enabled) = body.upnp_renderer {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{id}_upnp_renderer");
        let r = if enabled {
            settings.set(&key, "true")
        } else {
            settings.delete(&key)
        };
        ecrire!("upnp_renderer", enabled, r);
        // Annonce (ou retrait de l'annonce) sans attendre le cycle de 10 min.
        crate::routes::upnp_media_renderer::advertiser_wakeup().notify_one();
    }
    // Silence UPnP (#2263) → setting zone_{id}_upnp_silence. Même forme que
    // `upnp_renderer` : clé supprimée à la désactivation.
    if let Some(enabled) = body.upnp_silence {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = crate::config::cle_silence_upnp(id);
        let r = if enabled {
            settings.set(&key, "true")
        } else {
            settings.delete(&key)
        };
        ecrire!("upnp_silence", enabled, r);
        // Appliqué en DIRECT à la sortie déjà enregistrée : persister ne suffit
        // pas, sans cela cocher la case en écoutant ne changerait rien avant la
        // piste suivante — même piège que le `dlna_play_delay_ms` ci-dessus.
        if let Some(device_id) = repo.get(id).ok().flatten().and_then(|z| z.output_device_id) {
            let output = { state.outputs.lock().await.get(&device_id) };
            if let Some(output) = output {
                let guard = output.lock().await;
                if let Some(dlna) = guard.as_any().downcast_ref::<DlnaOutput>() {
                    dlna.set_upnp_silence(enabled);
                    // Ce que l'utilisateur vient d'accepter, écrit noir sur
                    // blanc dans le journal : l'option n'est pas muette.
                    info!(
                        zone = id,
                        device = %device_id,
                        silence = enabled,
                        abonnable = dlna.peut_s_abonner(),
                        "zone_silence_upnp — position estimée et déplacement façade différé quand armé"
                    );
                }
            }
        }
    }
    // Sortie mono (#2362) → setting zone_{id}_mono_downmix. Même forme que
    // `upnp_renderer` juste au-dessus : la clé est supprimée à la désactivation
    // plutôt qu'écrite à « false », pour que l'absence de clé et le défaut
    // désarmé soient un seul et même état.
    if let Some(enabled) = body.mono_downmix {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{id}_mono_downmix");
        let r = if enabled {
            settings.set(&key, "true")
        } else {
            settings.delete(&key)
        };
        ecrire!("mono_downmix", enabled, r);
        // #3254 — dire au JOURNAL, au moment du clic, que ce clic n'obtiendra
        // rien. La réponse porte déjà `mono_downmix_status` (la route rend la
        // fiche complète via `get_zone`), mais c'est ici que l'utilisateur croit
        // avoir obtenu quelque chose.
        //
        // ⚠️ On ne se sert PAS de la valeur rendue par `refresh_zone_mono_downmix`
        // comme signal de disponibilité : elle vaut `false` aussi bien parce que
        // la zone n'est pas locale que parce qu'aucune sortie n'est ouverte — la
        // même ambiguïté que `crossfeed_applied_live`. La règle, elle, ne dépend
        // que de la zone.
        let statut = tune_core::audio::mono_downmix::mono_downmix_status(
            enabled,
            tune_core::audio::mono_downmix::mono_downmix_runs_on_output(
                // La zone RELUE, pas `zone_before` : le même PATCH a pu changer
                // `output_device_id` quelques lignes plus haut, et c'est la
                // sortie d'APRÈS qui décide si le repli agira.
                repo.get(id)
                    .ok()
                    .flatten()
                    .and_then(|z| z.output_device_id)
                    .as_deref(),
            ),
            tune_core::audio::audiophile::zone_enabled(&state.backend, id),
        );
        if statut.unavailable {
            warn!(
                zone_id = id,
                requested = enabled,
                reason = statut.reason.map(|r| r.code()).unwrap_or_default(),
                "zone_mono_downmix_sans_effet — le réglage est enregistré mais rien ne l'applique sur cette zone"
            );
        }
        // Persister ne suffit pas : sans ceci, cocher la case en écoutant ne
        // changerait rien avant la piste suivante (#1725, #1786). Or ce
        // réglage-ci se vérifie précisément à l'oreille, musique en cours.
        state.orchestrator.refresh_zone_mono_downmix(id).await;
    }
    // Trim de gain par renderer → setting zone_{id}_gain_trim_db (±12 dB, 0 = efface).
    if let Some(db) = body.gain_trim_db {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = format!("zone_{id}_gain_trim_db");
        let clamped = db.clamp(-12.0, 12.0);
        let r = if clamped == 0.0 {
            settings.delete(&key)
        } else {
            settings.set(&key, &format!("{clamped}"))
        };
        ecrire!("gain_trim_db", clamped, r);
        // Effet immédiat : re-pousser le volume courant au device (le trim est
        // composé dans orchestrator.set_volume). Sans ça, il faudrait attendre
        // le prochain coup de curseur.
        if let Ok(Some(z)) = repo.get(id) {
            if !z.fixed_volume {
                if let Some(ref did) = z.output_device_id {
                    if let Err(error) = state
                        .orchestrator
                        .set_volume(id, z.volume / 100.0, Some(did))
                        .await
                    {
                        warn!(zone_id = id, error = %error, "gain_trim_volume_refresh_failed");
                    }
                }
            }
        }
    }
    // Correction de marque/modele : la remonter a mozaiklabs.fr.
    //
    // Le catalogue d'appareils est fige dans le binaire ; ces corrections sont
    // la seule matiere qui permette de le faire evoluer a partir du parc reel.
    // Envoi anonyme et sans attente : la reponse HTTP a l'utilisateur ne doit
    // dependre en rien de la disponibilite du site.
    Ok(())
}

pub(super) async fn create_zone(
    State(state): State<AppState>,
    Json(body): Json<CreateZone>,
) -> impl IntoResponse {
    let output_type = body.output_type.as_deref();

    // Une sortie locale s'identifie par `local:{nom}` — c'est ce préfixe, et
    // lui seul, qui dit à l'orchestrateur « carte son » plutôt que « renderer
    // réseau » (`orchestrator.rs`, une dizaine de `starts_with("local:")`).
    //
    // Un client qui envoie le nom nu crée donc une zone que rien ne peut
    // jouer : la lecture part sur le chemin réseau, télécharge la piste
    // entière, la décode, la ré-encode, puis pousse une URL vers un appareil
    // qui n'existe pas. Plus d'une minute d'attente, et aucun son (DEvir,
    // #1823). La zone échappe en prime au dédoublonnage, qui regroupe par
    // `output_device_id` : elle double la zone correcte du même appareil.
    //
    // On répare ici plutôt qu'au seul appelant : le serveur se met à jour
    // avant le client, et un client déjà installé continuerait sinon à créer
    // des zones mortes.
    let device_id_normalise = body.output_device_id.as_deref().map(|d| {
        if output_type == Some("local") && !d.starts_with("local:") {
            warn!(
                device_id = d,
                corrige = format!("local:{d}"),
                "create_zone_local_device_id_sans_prefixe_corrige"
            );
            format!("local:{d}")
        } else {
            d.to_string()
        }
    });
    let output_device_id = device_id_normalise.as_deref();

    // If device already has a zone (visible OR hidden), return it (no premium check needed).
    // A previously soft-deleted zone (is_hidden=1) is resurrected so the user's
    // prior settings (volume, DSP, gapless, etc.) are preserved.
    if let Some(device_id) = output_device_id {
        let repo = ZoneRepo::with_backend(state.backend.clone());
        if let Ok(Some(existing)) = repo.get_by_device_id(device_id) {
            if let Some(id) = existing.id {
                // Unhide if the zone was soft-deleted
                if repo.is_device_hidden(device_id) {
                    info!(
                        zone_id = id,
                        device_id, "unhiding_previously_deleted_zone_via_api"
                    );
                    let _ = repo.unhide(id);
                    // Update name in case device was renamed
                    let _ = repo.update_name(id, &body.name);
                    if let Some(ref ot) = body.output_type {
                        let _ = repo.update_output_type(id, ot);
                    }
                }
                let _ = repo.update_online(id, true);
                // Le contrat client AVEC l'etat REEL. Une zone qui existe deja
                // peut etre en train de jouer : lui coller `state: "stopped"`
                // serait un second mensonge apres le volume. `build_zone_json`
                // sait deja produire ce contrat — s'en servir evite une
                // troisieme copie a faire deriver (#2284, revue JP Robbe).
                let v = crate::routes::playback::build_zone_json(&state, id).await;
                // Une zone masquee qui reapparait est un evenement : sans
                // annonce, les autres clients connectes ne la voient qu'au
                // prochain refetch independant.
                state
                    .event_bus
                    .emit("zone.updated", json!({ "zone_id": id }));
                info!(zone_id = id, device_id, "zone_already_exists_returning");
                return (StatusCode::OK, Json(v)).into_response();
            }
        }
    }

    // The free-tier zone cap is enforced at *activation* (first play) in
    // orchestrator.play(), not at creation: creating/discovering a zone is
    // always allowed and the zone starts dormant. This avoids blocking a free
    // user from creating their actual renderer just because auto-discovered
    // zones filled the old count. See PlaybackOrchestrator::enforce_zone_cap.

    // For DLNA/OpenHome zones, ensure the output is registered before persisting
    if let Some(device_id) = output_device_id {
        let is_dlna = matches!(output_type, Some("dlna") | Some("openhome"));
        if is_dlna {
            let already_registered = {
                let outputs = state.outputs.lock().await;
                outputs.get(device_id).is_some()
            };
            if !already_registered {
                // Look up the discovered device and register its DLNA output
                let scanner = &state.scanner;
                let devices = scanner.devices().await;

                let disc = devices.iter().find(|d| d.id == device_id);
                if let Some(dev) = disc {
                    let registered = register_dlna_output_from_device(dev, &state).await;
                    if !registered {
                        warn!(device_id, "create_zone_output_registration_failed");
                    }
                } else {
                    warn!(device_id, "create_zone_device_not_discovered");
                }
            }
        }

        // For local audio zones, verify the device exists in the OutputRegistry
        if matches!(output_type, Some("local")) && device_id.starts_with("local:") {
            let found = {
                let outputs = state.outputs.lock().await;
                outputs.get(device_id).is_some()
            };
            if !found {
                warn!(device_id, "create_zone_local_device_not_found");
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"detail": format!("Local audio device not found: {device_id}. Make sure the device is connected and detected.")})),
                )
                    .into_response();
            }
        }

        // #1281 — même appareil physique, seconde identité SSDP (DLNA +
        // OpenHome, ou deux UUID : buchardt A700). La découverte regroupe déjà
        // par hôte (`zone_id_by_host`), mais CE chemin manuel ne dédoublonnait
        // que par `output_device_id` exact : créer une zone depuis l'entrée
        // jumelle du sélecteur produisait une deuxième zone pour le même
        // renderer — « I tried creating a zone and it duplicates ». L'hôte
        // vient du registre des sorties (rempli à la découverte) ; s'il porte
        // déjà une zone visible, on la rend au lieu d'en créer une autre.
        if is_dlna {
            let host = { state.outputs.lock().await.host_of(device_id) };
            if let Some(host) = host {
                let repo = ZoneRepo::with_backend(state.backend.clone());
                if let Some(existing_id) = repo.zone_id_by_host(&host) {
                    let _ = repo.update_online(existing_id, true);
                    // Même contrat client que les deux autres retours
                    // anticipés (#2284) : l'état RÉEL de la zone.
                    let v = crate::routes::playback::build_zone_json(&state, existing_id).await;
                    state
                        .event_bus
                        .emit("zone.updated", json!({ "zone_id": existing_id }));
                    info!(
                        zone_id = existing_id,
                        device_id,
                        host = %host,
                        "zone_same_host_already_exists_returning"
                    );
                    return (StatusCode::OK, Json(v)).into_response();
                }
            }
        }
    }

    // Duplicate device assignment already handled above (early return)

    let repo = ZoneRepo::with_backend(state.backend.clone());
    match repo.create(&body.name, output_type, output_device_id) {
        Ok(id) => {
            info!(zone_id = id, name = %body.name, output_type = ?output_type, "zone_created");

            // Build the full zone object for both HTTP response and WS event
            let zone = repo.get(id).ok().flatten();
            let v =
                tune_core::db::zone_repo::zone_creee_contrat_client(zone.as_ref(), id, &body.name);

            // Emit with full zone data so clients can merge without re-fetching
            state.event_bus.emit(
                "zone.created",
                json!({
                    "id": id,
                    "zone": &v,
                }),
            );

            (StatusCode::CREATED, Json(v)).into_response()
        }
        Err(e) if e.contains("UNIQUE constraint failed") => {
            // Safety net: a hidden zone with this device_id blocked the INSERT.
            // Unhide it and return it instead of erroring.
            if let Some(device_id) = output_device_id {
                if let Ok(Some(existing)) = repo.get_by_device_id(device_id) {
                    if let Some(id) = existing.id {
                        warn!(
                            zone_id = id,
                            device_id, "unique_constraint_recovery_unhiding_zone"
                        );
                        let _ = repo.unhide(id);
                        let _ = repo.update_name(id, &body.name);
                        let _ = repo.update_online(id, true);
                        // Meme contrat, meme raison qu'au-dessus (#2284).
                        let v = crate::routes::playback::build_zone_json(&state, id).await;
                        state
                            .event_bus
                            .emit("zone.updated", json!({ "zone_id": id }));
                        return (StatusCode::OK, Json(v)).into_response();
                    }
                }
            }
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"detail": e})),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"detail": e})),
        )
            .into_response(),
    }
}

/// DELETE /zones — soft-delete every zone and clear the free-tier
/// activation markers, so a Free user whose 3-zone quota is consumed by
/// stale renderers can start over and explicitly re-create the zones he
/// wants (discovery never resurrects hidden zones, only POST /zones does).
pub(super) async fn delete_all_zones(State(state): State<AppState>) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let ids: Vec<i64> = repo
        .list()
        .map(|zs| zs.iter().filter_map(|z| z.id).collect())
        .unwrap_or_default();
    match repo.delete_all() {
        Ok(_) => {
            info!(count = ids.len(), "all_zones_deleted_quota_reset");
            for id in ids {
                state.event_bus.emit_typed(
                    tune_core::event_types::EventType::ZoneDeleted,
                    json!({"id": id}),
                );
            }
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub(super) async fn delete_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    match repo.delete(id) {
        Ok(_) => {
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::ZoneDeleted,
                json!({"id": id}),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub(super) async fn update_volume(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateVolume>,
) -> impl IntoResponse {
    // Normalise: web client sends 0.0–1.0, legacy clients may send 0–100.
    let lineaire = body.volume.map(|v| if v > 1.0 { v / 100.0 } else { v });
    // #1274 — l'arbitrage `volume` / `volume_db` et la conversion des dB
    // vivent dans `volume_scale`, pas ici. Cette route ne fait que ramener sa
    // convention historique sur 0..1 avant de la lui passer.
    let volume_f = match tune_core::audio::volume_scale::demande_lineaire(lineaire, body.volume_db)
    {
        Ok(v) => v,
        Err(motif) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": "invalid_volume", "message": motif })),
            )
                .into_response();
        }
    };
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let device_id = repo.get(id).ok().flatten().and_then(|z| z.output_device_id);
    // #1274 — la consigne en dB doit avoir un endroit où arriver. Si la
    // sortie de la zone ne parle au périphérique qu'en entiers, un dB sous son
    // premier pas ne baisse pas le son : il l'éteint. On le refuse en le
    // nommant, plutôt que de répondre 204 sur un silence.
    if let Some(db) = body.volume_db
        && let Some(motif) = refus_de_resolution_volume(&state, device_id.as_deref(), db).await
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "volume_db_hors_resolution", "message": motif })),
        )
            .into_response();
    }

    match state
        .orchestrator
        .set_volume(id, volume_f, device_id.as_deref())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => crate::routes::playback::output_command_error_response(error),
    }
}

pub(super) async fn update_muted(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<UpdateMuted>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let device_id = repo.get(id).ok().flatten().and_then(|z| z.output_device_id);
    match state
        .orchestrator
        .set_mute(id, body.muted, device_id.as_deref())
        .await
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => crate::routes::playback::output_command_error_response(error),
    }
}

pub(super) async fn rename_zone(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<RenameZone>,
) -> impl IntoResponse {
    let repo = ZoneRepo::with_backend(state.backend.clone());
    match repo.update_name(id, &body.name) {
        Ok(_) => {
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::ZoneUpdated,
                json!({ "id": id, "name": body.name }),
            );
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}
