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

    Ok(())
}

/// Écrit un champ, ou s'arrête en journalisant la cause.
///
/// Une macro et non une closure : chaque échec doit **sortir** du handler,
/// et une closure ne peut pas rendre la main à sa place. C'est aussi ce qui
/// garantit qu'aucun des trente blocs ne puisse redevenir muet — il n'y a
/// plus qu'un seul endroit où le `return` est écrit.
///
/// REF-4 phase 2 (#2219) : les blocs vivent dans sept familles, une fonction
/// chacune. Une macro déclarée au module ne voit pas les locaux de la fonction
/// qui l'invoque (hygiène de `macro_rules!`) : `lier_ecrire_a!(state, id, $)`
/// déclare donc `ecrire!` DANS chaque famille, liée à SES `state` et `id`. Le
/// `$d:tt` reçoit le signe `$` lui-même — la seule façon stable d'écrire les
/// paramètres d'une macro depuis une macro. La règle d'`ecrire!`, elle, est
/// celle du bloc plat, inchangée.
macro_rules! lier_ecrire_a {
    ($state:ident, $id:ident, $d:tt) => {
        macro_rules! ecrire {
                    ($d champ:literal, $d valeur:expr, $d ecriture:expr) => {
                        if let Err(e) = $d ecriture {
                            return Err(echec_ecriture($id, $d champ, &$d valeur.to_string(), e));
                        }
                        // #3589 — la MARQUE d'auteur. Ce `PATCH` est la seule porte par
                        // laquelle un humain règle une zone : ce qui passe ici est, par
                        // définition, posé à la main. La marque survit au retour au
                        // défaut — c'est précisément ce que la convention « clé supprimée
                        // à la désactivation » ne sait pas dire, et sans quoi la
                        // préconfiguration écraserait la case que l'utilisateur DÉCOCHE.
                        super::preconfiguration::marquer_pose(&$state.backend, $id, $d champ);
                    };
                }
    };
}

/// Troisième temps : chaque préférence persistée par la macro `ecrire!`, qui
/// journalise l'échec, et les rafraîchissements de la sortie vivante qui
/// l'accompagnent. Sept familles de clés (REF-4 phase 2, #2219), appelées
/// DANS L'ORDRE des écritures du bloc plat d'origine — cet ordre est un
/// comportement : la sortie mono relit la zone APRÈS le changement de sortie,
/// le trim de gain repousse le volume APRÈS `fixed_volume`. Chaque famille
/// rend la main au premier échec, comme le bloc plat le faisait.
async fn persister_le_patch(
    state: &AppState,
    repo: &ZoneRepo,
    id: i64,
    zone_before: &Zone,
    command_device_id: Option<&str>,
    body: &PatchZone,
) -> Result<(), axum::response::Response> {
    persister_la_zone_et_sa_sortie(state, repo, id, body)?;
    persister_le_volume_fixe(state, repo, id, zone_before, command_device_id, body).await?;
    persister_la_lecture(state, repo, id, body)?;
    persister_le_reseau(state, repo, id, body).await?;
    persister_la_marque_et_le_modele(state, id, body)?;
    persister_l_upnp(state, repo, id, body).await?;
    persister_le_son(state, repo, id, body).await?;
    // Correction de marque/modele : la remonter a mozaiklabs.fr.
    //
    // Le catalogue d'appareils est fige dans le binaire ; ces corrections sont
    // la seule matiere qui permette de le faire evoluer a partir du parc reel.
    // Envoi anonyme et sans attente : la reponse HTTP a l'utilisateur ne doit
    // dependre en rien de la disponibilite du site.
    Ok(())
}

/// Famille 1 — la zone et sa sortie : `name`, `output_device_id`,
/// `output_type`, `gapless_enabled`, `sync_delay_ms`, `max_sample_rate`.
fn persister_la_zone_et_sa_sortie(
    state: &AppState,
    repo: &ZoneRepo,
    id: i64,
    body: &PatchZone,
) -> Result<(), axum::response::Response> {
    lier_ecrire_a!(state, id, $);
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
    Ok(())
}

/// Famille 2 — le volume fixe (#2395) : `fixed_volume`, et les deux commandes
/// à l'appareil qui n'agissent que sur une TRANSITION.
async fn persister_le_volume_fixe(
    state: &AppState,
    repo: &ZoneRepo,
    id: i64,
    zone_before: &Zone,
    command_device_id: Option<&str>,
    body: &PatchZone,
) -> Result<(), axum::response::Response> {
    lier_ecrire_a!(state, id, $);
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
    Ok(())
}

/// Famille 3 — la lecture : `autoplay_mode` / `autoplay_enabled` (#2271),
/// `dsd_mode`, `lyrics_offset_ms`.
fn persister_la_lecture(
    state: &AppState,
    repo: &ZoneRepo,
    id: i64,
    body: &PatchZone,
) -> Result<(), axum::response::Response> {
    lier_ecrire_a!(state, id, $);
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
    Ok(())
}

/// Famille 4 — le réseau : `dlna_native_flac`, `alac_passthrough`,
/// `aac_passthrough`, `dlna_lpcm`, `dlna_cap_16bit`, `dlna_wav24`,
/// `dlna_play_delay_ms` — ce dernier appliqué en direct à la sortie DLNA
/// déjà enregistrée.
async fn persister_le_reseau(
    state: &AppState,
    repo: &ZoneRepo,
    id: i64,
    body: &PatchZone,
) -> Result<(), axum::response::Response> {
    lier_ecrire_a!(state, id, $);
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
    Ok(())
}

/// Famille 5 — la marque et le modèle : `brand`, `model` (chaîne vide =
/// retour à la détection UPnP), `identite_appareil_effacee` (#3660).
fn persister_la_marque_et_le_modele(
    state: &AppState,
    id: i64,
    body: &PatchZone,
) -> Result<(), axum::response::Response> {
    lier_ecrire_a!(state, id, $);
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
    // #3660 — le vide FORCÉ. La chaîne vide ci-dessus efface l'OVERRIDE et
    // laisse revenir la détection ; ce drapeau-ci récuse la DÉTECTION. Deux
    // gestes distincts, parce que ce sont deux intentions distinctes : « je
    // n'ai plus d'avis » et « cet appareil n'est pas celui-là ».
    if let Some(efface) = body.identite_appareil_effacee {
        let settings = SettingsRepo::with_backend(state.backend.clone());
        let key = super::cle_identite_effacee(id);
        let r = if efface {
            settings.set(&key, "true")
        } else {
            settings.delete(&key)
        };
        ecrire!("identite_appareil_effacee", efface, r);
    }
    Ok(())
}

/// Famille 6 — UPnP : `upnp_renderer` (#1750), `upnp_silence` (#2263) — clé
/// supprimée à la désactivation, et le second appliqué en direct à la sortie
/// DLNA déjà enregistrée.
async fn persister_l_upnp(
    state: &AppState,
    repo: &ZoneRepo,
    id: i64,
    body: &PatchZone,
) -> Result<(), axum::response::Response> {
    lier_ecrire_a!(state, id, $);
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
    Ok(())
}

/// Famille 7 — le son : `mono_downmix` (#2362, avec le rafraîchisseur de la
/// sortie vivante qu'exige `eq_refresh_guard`) et `gain_trim_db`, qui
/// repousse le volume courant à l'appareil.
async fn persister_le_son(
    state: &AppState,
    repo: &ZoneRepo,
    id: i64,
    body: &PatchZone,
) -> Result<(), axum::response::Response> {
    lier_ecrire_a!(state, id, $);
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
    Ok(())
}

/// Que faire du nom demandé quand le périphérique a DÉJÀ une zone ?
/// (#1770, annexe 4)
///
/// ## L'asymétrie, mesurée
///
/// `POST /zones` porte une seule intention : « crée-moi une zone sur ce
/// périphérique, appelle-la X ». Quand une zone existe déjà pour ce
/// `output_device_id`, la route la rend telle quelle — `200 OK`,
/// `zone_already_exists_returning` — et le nom demandé n'était honoré que sur
/// la branche `is_device_hidden`. Sur une zone **visible** il était jeté
/// **sans un mot** : ni journal, ni code d'état différent, ni champ dans la
/// réponse. L'écran, lui, annonce « zone créée ». C'est aussi ce qui fait lire
/// « ma zone a happé les périphériques » là où le serveur a simplement rendu
/// une zone existante.
///
/// ## Ce que cette fonction NE tranche PAS
///
/// Deux conduites se valent, et le choix appartient à Bertrand — c'est
/// exactement ce que l'étiquette `keep-open` de #1770 réserve :
///
/// - **honorer** le nom sur une zone visible comme sur une masquée : cohérent,
///   mais cela renomme en silence une zone que quelqu'un d'autre écoute
///   peut-être ;
/// - **refuser** (`409`) en nommant la zone existante : honnête, mais c'est un
///   contrat client qui change, et tout client déjà installé lit ce `200`
///   comme un succès.
///
/// Cette fonction rend donc le comportement d'aujourd'hui, **inchangé**. Ce
/// qu'elle apporte est ailleurs : un SEUL endroit nommé où l'arbitrage se
/// posera, et la fin du silence sur la branche `Ecarte`. Le jour où
/// l'arbitrage est rendu, c'est un bras du `match` de `create_zone` qui change,
/// et les témoins de `nom_de_zone_existante_guard.rs` disent lequel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NomDeZoneExistante {
    /// Le nom demandé est déjà celui de la zone : rien à décider, et surtout
    /// rien à écrire en base.
    DejaLeBon,
    /// Zone MASQUÉE ressuscitée : elle reprend le nom demandé. Décidé de
    /// longue date (« Update name in case device was renamed »), inchangé ici.
    Honore,
    /// Zone VISIBLE : le nom demandé est écarté, la zone garde le sien. C'est
    /// le comportement d'aujourd'hui — et l'arbitrage en attente.
    Ecarte,
}

pub(super) fn nom_de_zone_existante(
    zone_masquee: bool,
    nom_actuel: &str,
    nom_demande: &str,
) -> NomDeZoneExistante {
    if nom_actuel == nom_demande {
        NomDeZoneExistante::DejaLeBon
    } else if zone_masquee {
        NomDeZoneExistante::Honore
    } else {
        NomDeZoneExistante::Ecarte
    }
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
                let masquee = repo.is_device_hidden(device_id);
                if masquee {
                    info!(
                        zone_id = id,
                        device_id, "unhiding_previously_deleted_zone_via_api"
                    );
                    if let Err(e) = repo.unhide(id) {
                        // Rendre `200 OK` ici, c'est annoncer « la voilà » d'une
                        // zone qui reste masquée : l'utilisateur ne la verra
                        // nulle part et croira l'avoir créée.
                        return echec_ecriture(id, "is_hidden", "0", e);
                    }
                    if let Some(ref ot) = body.output_type {
                        let _ = repo.update_output_type(id, ot);
                    }
                }
                // Le nom demandé : honoré, écarté, ou déjà le bon. UN seul
                // endroit décide (#1770, annexe 4), et aucune des trois
                // branches ne se tait.
                match nom_de_zone_existante(masquee, &existing.name, &body.name) {
                    NomDeZoneExistante::Honore => {
                        if let Err(e) = repo.update_name(id, &body.name) {
                            // Cette branche a DÉJÀ décidé d'honorer le nom :
                            // rendre `200 OK` avec l'ancienne fiche serait dire
                            // « c'est fait » d'une écriture qui a échoué.
                            return echec_ecriture(id, "name", &body.name, e);
                        }
                    }
                    // La perte cesse d'être silencieuse. Le code d'état et la
                    // fiche rendue ne changent PAS : trancher entre « honorer »
                    // et « 409 » appartient à Bertrand.
                    NomDeZoneExistante::Ecarte => warn!(
                        zone_id = id,
                        device_id,
                        nom_demande = %body.name,
                        nom_conserve = %existing.name,
                        "zone_existante_nom_demande_ecarte"
                    ),
                    NomDeZoneExistante::DejaLeBon => {}
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

            // #3589 — la zone vient de naître : c'est le SEUL instant où
            // « aucune marque d'auteur » signifie vraiment « personne n'a rien
            // réglé ». La provenance s'ouvre ici, la préconfiguration s'appuie
            // dessus, et l'ordre compte : sans la marque, `preconfigurer_zone`
            // refuse d'agir.
            super::preconfiguration::ouvrir_provenance(&state.backend, id);
            let (brand, model) = identite_detectee(&state, output_device_id).await;
            super::preconfiguration::preconfigurer_zone(
                &state.backend,
                id,
                brand.as_deref(),
                model.as_deref(),
                output_type,
            );

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
                        // Ce filet ne se déclenche QUE sur une zone masquée —
                        // c'est ce que dit la contrainte UNIQUE qui vient
                        // d'échouer. Honorer le nom est donc la branche déjà
                        // décidée, et un échec d'écriture ne peut pas ressortir
                        // en `200 OK` avec l'ancienne fiche (#1770, annexe 4).
                        if let Err(e) = repo.unhide(id) {
                            return echec_ecriture(id, "is_hidden", "0", e);
                        }
                        if let Err(e) = repo.update_name(id, &body.name) {
                            return echec_ecriture(id, "name", &body.name, e);
                        }
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

/// Marque et modèle **annoncés par l'appareil** qui vient d'être assigné.
///
/// Volontairement la détection brute, et non l'override utilisateur
/// (`zone_{id}_brand`) : à la création d'une zone, cet override n'existe pas
/// encore — il ne naît que d'une correction ultérieure. Une zone dont
/// l'appareil n'annonce rien n'est pas préconfigurable, et c'est très bien :
/// la reconnaissance exige un nom de modèle exact.
async fn identite_detectee(
    state: &AppState,
    output_device_id: Option<&str>,
) -> (Option<String>, Option<String>) {
    let Some(device_id) = output_device_id else {
        return (None, None);
    };
    let devices = state.scanner.devices().await;
    match devices.iter().find(|d| d.id == device_id) {
        Some(d) => (d.manufacturer.clone(), d.model.clone()),
        None => (None, None),
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

/// `POST /zones/{doublon}/fusionner-dans/{cible}` — DUP-1, phase 1.
///
/// La phase 0 (`zones_doublons` dans `/system/diagnostics`) nomme les zones
/// qui désignent probablement le même appareil ; ici l'utilisateur tranche,
/// et le serveur vérifie avant d'agir : les deux zones doivent avoir la même
/// clé d'appareil (`cle_appareil`, la règle de la phase 0), et aucune ne doit
/// être en lecture. Rien n'est automatique : le ré-ancrage silencieux a déjà
/// coûté une zone Apple TV devenue Sonos (13/08).
///
/// Réponses : `200` avec le bilan de [`ZoneRepo::fusionner`] ; `404` zone
/// inconnue ; `400` même zone des deux côtés ; `409 zones_distinctes` ou
/// `409 zone_en_lecture`.
pub(super) async fn fusionner_zones(
    State(state): State<AppState>,
    Path((doublon, cible)): Path<(i64, i64)>,
) -> impl IntoResponse {
    use crate::routes::system::diagnostics::{cle_appareil, zone_vue};
    if doublon == cible {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "meme_zone", "message": "une zone ne se fusionne pas dans elle-même"})),
        )
            .into_response();
    }
    let repo = ZoneRepo::with_backend(state.backend.clone());
    let (Ok(Some(zone_doublon)), Ok(Some(zone_cible))) = (repo.get(doublon), repo.get(cible))
    else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "zone_inconnue"})),
        )
            .into_response();
    };
    let appareils = state.scanner.devices().await;
    let cle = |z: &Zone| zone_vue(z).and_then(|vue| cle_appareil(&vue, &appareils));
    let (cle_doublon, cle_cible) = (cle(&zone_doublon), cle(&zone_cible));
    if cle_doublon.is_none() || cle_doublon != cle_cible {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "zones_distinctes",
                "message": "les deux zones ne désignent pas le même appareil : la fusion est refusée",
                "cles": [cle_doublon, cle_cible],
            })),
        )
            .into_response();
    }
    for id in [doublon, cible] {
        if matches!(state.playback.get_state(id).await.state, PlayState::Playing) {
            return (
                StatusCode::CONFLICT,
                Json(json!({"error": "zone_en_lecture", "zone_id": id})),
            )
                .into_response();
        }
    }
    match repo.fusionner(doublon, cible) {
        Ok(rapport) => {
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::ZoneDeleted,
                json!({"id": doublon}),
            );
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::ZoneUpdated,
                json!({"id": cible}),
            );
            info!(doublon, cible, "zones_fusionnees");
            (StatusCode::OK, Json(json!(rapport))).into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": "fusion_echouee", "message": e})),
        )
            .into_response(),
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
