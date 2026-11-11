use super::*;

#[derive(Deserialize)]
pub(super) struct CreateGroup {
    /// Optional. The web client groups a *selection* of zones and has no name
    /// to offer, so it never sent this field; when it was mandatory serde
    /// rejected the whole body and axum answered a bare `422 Unprocessable
    /// Entity` with no text at all — the unexplained code the tester saw
    /// (#1702). Absent or blank, the group is named after its zones.
    #[serde(default)]
    pub(super) name: Option<String>,
    pub(super) zone_ids: Vec<i64>,
    /// The zone the others follow. Sent by the web client; defaults to the
    /// first zone of the selection.
    #[serde(default)]
    pub(super) leader_id: Option<i64>,
}

/// Why a set of zones cannot form a group.
///
/// Kept separate from the HTTP layer so the rules can be unit-tested without a
/// database, and so every refusal is forced to carry the words explaining it.
#[derive(Debug, PartialEq)]
pub(crate) enum GroupRefusal {
    /// Fewer than two *distinct* zones.
    NotEnoughZones,
    /// A zone id that no longer exists (stale list on the client's side).
    UnknownZone(i64),
    /// Two zones bound to the same output. Tune creates one zone per
    /// discovered output and duplicates do happen ("PC" and "Haut parleurs"
    /// on one sound card, #1702): grouping them would send the same stream
    /// twice to a single device. Both names travel with the refusal so the
    /// message can say *which* zones clash.
    SameOutput(String, String),
    /// #6132 — deux zones locales de la machine, sous ASIO : le pilote ne
    /// tient qu'un flux par machine, « une zone à la fois ». Depuis #7759,
    /// création et extension refusent avant ([`GroupRefusal::AsioUnsupported`]) ;
    /// ne reste que le recalcul de la sortie double.
    AsioOneZoneAtATime(String, String),
    /// #7759 — décision de Bertrand du 11/11 : la lecture groupée n'est
    /// possible qu'en WASAPI. Un membre dont la sortie s'ouvre en ASIO (nommé
    /// ici) suffit à refuser le groupe, quelle que soit la forme du groupe :
    /// en ASIO le groupe perdait le gapless et coupait quelques secondes
    /// (Didier, fil 2215), et le testeur préfère un refus à un mode dégradé.
    AsioUnsupported(String),
}

/// #6132 — les sorties suiveuses d'un groupe : le meneur est une zone locale,
/// et CHAQUE autre membre local de la machine (sortie distincte) reçoit le
/// même flux, corrigé de sa propre dérive. Rend `(zone suiveuse, sortie)`,
/// dans l'ordre du groupe.
pub(super) fn choisir_les_sorties_doubles(
    leader_id: i64,
    zone_ids: &[i64],
    zones: &[Zone],
) -> Vec<(i64, String)> {
    let local = |id: i64| {
        zones
            .iter()
            .find(|z| z.id == Some(id))
            .and_then(|z| z.output_device_id.clone())
            .filter(|d| d.starts_with("local:") && d.len() > "local:".len())
    };
    let Some(sortie_meneur) = local(leader_id) else {
        return Vec::new();
    };
    let mut choisies: Vec<(i64, String)> = Vec::new();
    for id in zone_ids.iter().copied().filter(|id| *id != leader_id) {
        if let Some(d) = local(id)
            && d != sortie_meneur
            && !choisies.iter().any(|(_, c)| *c == d)
        {
            choisies.push((id, d));
        }
    }
    choisies
}

/// #6132 — refuser la sortie double sous ASIO. `backend_meneur` et
/// `backend_suiveur` sont les backends ouvrables des deux sorties
/// ([`crate::routes::zones::backend_de_la_sortie_de_la_zone`]).
pub(super) fn refus_asio(
    backend_meneur: Option<&str>,
    backend_suiveur: Option<&str>,
    nom_meneur: &str,
    nom_suiveur: &str,
) -> Option<GroupRefusal> {
    let asio = |b: Option<&str>| b.is_some_and(|b| b.eq_ignore_ascii_case("asio"));
    (asio(backend_meneur) || asio(backend_suiveur))
        .then(|| GroupRefusal::AsioOneZoneAtATime(nom_meneur.to_string(), nom_suiveur.to_string()))
}

/// #7759 — le premier membre du groupe dont la sortie s'ouvre en ASIO, ou
/// `None`. `backends` donne, dans l'ordre du groupe, `(nom de la zone,
/// backend ouvrable de sa sortie)` ; une sortie réseau n'a pas de backend
/// local (`None`) et n'est jamais refusée ici.
pub(super) fn membre_asio(backends: &[(String, Option<String>)]) -> Option<GroupRefusal> {
    backends
        .iter()
        .find(|(_, b)| {
            b.as_deref()
                .is_some_and(|b| b.trim().eq_ignore_ascii_case("asio"))
        })
        .map(|(nom, _)| GroupRefusal::AsioUnsupported(nom.clone()))
}

/// #7759 — refuser un groupe dont un membre sort par ASIO. Le backend est
/// celui de [`backend_de_la_sortie`] : la sortie enregistrée d'abord, le
/// backend réglé pour la machine pour une sortie locale pas encore vue.
pub(crate) async fn refus_du_groupe_asio(
    state: &AppState,
    zone_ids: &[i64],
    zones: &[Zone],
) -> Option<GroupRefusal> {
    let mut backends = Vec::with_capacity(zone_ids.len());
    for id in zone_ids {
        let sortie = zones
            .iter()
            .find(|z| z.id == Some(*id))
            .and_then(|z| z.output_device_id.clone());
        backends.push((
            nom_de_zone(zones, *id),
            backend_de_la_sortie(state, sortie.as_deref()).await,
        ));
    }
    membre_asio(&backends)
}

/// #7759 — parmi `groups`, ceux qu'il faut défaire : un membre dont la sortie
/// ENREGISTRÉE s'ouvre en ASIO. Rend `(index du groupe, zone ASIO)`.
///
/// `backend_enregistre` ne connaît que les sorties présentes au registre, sans
/// repli sur le réglage de la machine : défaire est irréversible, et une
/// sortie absente (DAC débranché, « Speakers » WASAPI enregistré plus tard
/// sur une machine réglée en ASIO, réglage `asio` hérité hors Windows) ne
/// doit jamais coûter un groupe valide. Le repli reste bon pour REFUSER une
/// création, que l'utilisateur peut corriger sur-le-champ.
pub(super) fn groupes_a_defaire(
    groups: &[Value],
    zones: &[Zone],
    backend_enregistre: &std::collections::HashMap<i64, String>,
) -> Vec<(usize, String)> {
    groups
        .iter()
        .enumerate()
        .filter_map(|(i, g)| {
            let backends: Vec<(String, Option<String>)> = g["zone_ids"]
                .as_array()
                .map(|a| a.iter().filter_map(Value::as_i64).collect::<Vec<_>>())
                .unwrap_or_default()
                .into_iter()
                .map(|id| (nom_de_zone(zones, id), backend_enregistre.get(&id).cloned()))
                .collect();
            match membre_asio(&backends) {
                Some(GroupRefusal::AsioUnsupported(zone)) => Some((i, zone)),
                _ => None,
            }
        })
        .collect()
}

/// #7759 — défaire chaque groupe enregistré dont un membre sort désormais par
/// ASIO. Rend les numéros des groupes défaits.
///
/// Pourquoi défaire, et pourquoi ici : le choix ASIO/WASAPI est un réglage de
/// la machine, et les sorties locales ne s'ouvrent sous le nouveau backend
/// qu'au démarrage suivant (ou à la bascule à chaud d'un repli WASAPI vers
/// ASIO, #5353). C'est donc au démarrage, et après cette bascule, que la
/// zone « passe en ASIO » pour de bon. Garder le groupe en le signalant
/// laisserait le mode dégradé que le testeur refuse ; le refuser ne se peut
/// plus, il existe déjà. On le défait, comme le ferait « Dégrouper » : les
/// zones rejouent chacune de leur côté, rien d'autre n'est touché. Seules
/// les sorties enregistrées comptent ([`groupes_a_defaire`]).
pub async fn defaire_les_groupes_asio(state: &AppState) -> Vec<i64> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if groups.is_empty() {
        return Vec::new();
    }
    let zones = ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap_or_default();
    let mut backend_enregistre = std::collections::HashMap::new();
    for z in &zones {
        if let Some(id) = z.id
            && let Some(b) = crate::routes::zones::backend_de_la_sortie_de_la_zone(
                state,
                z.output_device_id.as_deref(),
            )
            .await
        {
            backend_enregistre.insert(id, b);
        }
    }
    defaire_les_groupes_selon(state, &settings, &mut groups, &zones, &backend_enregistre)
}

/// Le corps de [`defaire_les_groupes_asio`], les backends donnés.
pub(super) fn defaire_les_groupes_selon(
    state: &AppState,
    settings: &tune_core::db::settings_repo::SettingsRepo,
    groups: &mut Vec<Value>,
    zones: &[Zone],
    backend_enregistre: &std::collections::HashMap<i64, String>,
) -> Vec<i64> {
    let a_defaire = groupes_a_defaire(groups, zones, backend_enregistre);
    if a_defaire.is_empty() {
        return Vec::new();
    }
    let mut defaits = Vec::new();
    for (i, zone) in a_defaire.iter().rev() {
        let mut group = groups.remove(*i);
        if let Some(meneur) = group["leader_id"].as_i64().or_else(|| {
            group["zone_ids"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(Value::as_i64)
        }) {
            poser_la_sortie_double(settings, &mut group, meneur, Vec::new());
        }
        let gid = group.get("id").and_then(Value::as_i64);
        warn!(
            group_id = ?gid,
            zone = %zone,
            "groupe_asio_defait — la lecture groupée n'est possible qu'en WASAPI (#7759)"
        );
        if let Some(gid) = gid {
            defaits.push(gid);
        }
    }
    if let Ok(texte) = serde_json::to_string(&*groups) {
        settings.set("zone_groups", &texte).ok();
    }
    for id in &defaits {
        state.event_bus.emit_typed(
            tune_core::event_types::EventType::GroupDeleted,
            json!({ "id": id, "reason": "group_asio_unsupported" }),
        );
    }
    defaits
}

/// Le nom d'une zone, pour un message.
fn nom_de_zone(zones: &[Zone], id: i64) -> String {
    zones
        .iter()
        .find(|z| z.id == Some(id))
        .map(|z| z.name.clone())
        .unwrap_or_else(|| format!("Zone {id}"))
}

/// Le backend ouvrable d'une sortie locale, et à défaut (sortie pas encore
/// enregistrée, au démarrage) le backend local réglé pour la machine.
async fn backend_de_la_sortie(state: &AppState, sortie: Option<&str>) -> Option<String> {
    if let Some(b) = crate::routes::zones::backend_de_la_sortie_de_la_zone(state, sortie).await {
        return Some(b);
    }
    sortie.filter(|s| s.starts_with("local:"))?;
    tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
        .get("local_audio_backend")
        .ok()
        .flatten()
}

/// #6132 — choisir les sorties suiveuses du groupe et vérifier qu'ASIO les
/// permet : une seule zone locale sous ASIO, ni meneuse ni suiveuse.
async fn sorties_doubles_du_groupe(
    state: &AppState,
    leader_id: i64,
    zone_ids: &[i64],
    zones: &[Zone],
) -> Result<Vec<(i64, String)>, GroupRefusal> {
    let choisies = choisir_les_sorties_doubles(leader_id, zone_ids, zones);
    if choisies.is_empty() {
        return Ok(choisies);
    }
    let sortie_meneur = zones
        .iter()
        .find(|z| z.id == Some(leader_id))
        .and_then(|z| z.output_device_id.clone());
    let backend_meneur = backend_de_la_sortie(state, sortie_meneur.as_deref()).await;
    for (suiveur, sortie) in &choisies {
        let backend_suiveur = backend_de_la_sortie(state, Some(sortie)).await;
        if let Some(refus) = refus_asio(
            backend_meneur.as_deref(),
            backend_suiveur.as_deref(),
            &nom_de_zone(zones, leader_id),
            &nom_de_zone(zones, *suiveur),
        ) {
            return Err(refus);
        }
    }
    Ok(choisies)
}

/// #6132 — poser (ou retirer) le réglage interne que la lecture du meneur
/// lit, et le dire dans le groupe enregistré.
fn poser_la_sortie_double(
    settings: &tune_core::db::settings_repo::SettingsRepo,
    group: &mut Value,
    leader_id: i64,
    choisies: Vec<(i64, String)>,
) {
    let cle = tune_core::outputs::sortie_double::cle_de_zone(leader_id);
    if choisies.is_empty() {
        settings.delete(&cle).ok();
        if let Some(obj) = group.as_object_mut() {
            obj.remove("sortie_double");
        }
        return;
    }
    let sorties: Vec<String> = choisies.iter().map(|(_, s)| s.clone()).collect();
    settings
        .set(
            &cle,
            &tune_core::outputs::sortie_double::ecrire_la_valeur(&sorties),
        )
        .ok();
    group["sortie_double"] = json!({
        "suiveurs": choisies
            .iter()
            .map(|(id, sortie)| json!({ "zone_id": id, "sortie": sortie }))
            .collect::<Vec<_>>(),
    });
}

/// #6132 — au démarrage : recalculer la sortie double de chaque groupe
/// enregistré, pour que les groupes faits avant cette version (ou avant
/// qu'une zone ne change de sortie) jouent ensemble sans geste de
/// l'utilisateur. Un groupe qu'ASIO interdit perd sa sortie double, sans
/// être supprimé : il reste un groupe de volume, comme avant.
pub async fn recalculer_les_sorties_doubles(state: &AppState) {
    // #7759 — un groupe dont un membre sort désormais par ASIO est défait
    // avant tout recalcul.
    defaire_les_groupes_asio(state).await;
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    if groups.is_empty() {
        return;
    }
    let zones = ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap_or_default();
    let mut poses = 0usize;
    for group in groups.iter_mut() {
        let zone_ids: Vec<i64> = group["zone_ids"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
            .unwrap_or_default();
        let Some(leader_id) = group["leader_id"]
            .as_i64()
            .or_else(|| zone_ids.first().copied())
        else {
            continue;
        };
        let choisies = match sorties_doubles_du_groupe(state, leader_id, &zone_ids, &zones).await {
            Ok(choisies) => choisies,
            Err(refus) => {
                warn!(?refus, leader_id, "sortie_double_recalcul_refusee");
                Vec::new()
            }
        };
        poses += usize::from(!choisies.is_empty());
        poser_la_sortie_double(&settings, group, leader_id, choisies);
    }
    if let Ok(texte) = serde_json::to_string(&groups) {
        settings.set("zone_groups", &texte).ok();
    }
    info!(
        groupes = groups.len(),
        avec_sortie_double = poses,
        "sortie_double_recalculee_au_demarrage"
    );
}

/// Check a grouping request against the zones that actually exist.
///
/// Returns the de-duplicated zone ids, in request order, when the group is
/// legitimate.
pub(super) fn validate_group(zone_ids: &[i64], zones: &[Zone]) -> Result<Vec<i64>, GroupRefusal> {
    let mut unique: Vec<i64> = Vec::new();
    for &id in zone_ids {
        if !unique.contains(&id) {
            unique.push(id);
        }
    }
    if unique.len() < 2 {
        return Err(GroupRefusal::NotEnoughZones);
    }

    // (output device, name of the zone that claimed it first)
    let mut claimed: Vec<(&str, &str)> = Vec::new();
    for &id in &unique {
        let zone = zones
            .iter()
            .find(|z| z.id == Some(id))
            .ok_or(GroupRefusal::UnknownZone(id))?;
        // A zone with no output device is an orphan, not a duplicate: several
        // of them share "nothing", which is not the same output.
        let Some(device) = zone.output_device_id.as_deref().filter(|d| !d.is_empty()) else {
            continue;
        };
        if let Some((_, first)) = claimed.iter().find(|(d, _)| *d == device) {
            return Err(GroupRefusal::SameOutput(
                (*first).to_string(),
                zone.name.clone(),
            ));
        }
        claimed.push((device, zone.name.as_str()));
    }
    Ok(unique)
}

/// Turn a refusal into the error envelope the web client understands:
/// `error` is the machine code, `message` the sentence it displays.
pub(crate) fn group_refusal_response(
    refusal: &GroupRefusal,
    lang: &str,
) -> axum::response::Response {
    // `message_key` et `params` (#7077) : le client web n'affiche la phrase du
    // serveur que si elle se dit traduite ; sans eux, il montrait « le serveur
    // n'a pas pu traiter la demande » à la place de la raison du refus.
    let (status, code, cle, params): (StatusCode, &str, &str, Vec<(&str, String)>) = match refusal {
        GroupRefusal::NotEnoughZones => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "group_needs_two_zones",
            "zonegroup.needsTwoZones",
            Vec::new(),
        ),
        GroupRefusal::UnknownZone(id) => (
            StatusCode::NOT_FOUND,
            "group_unknown_zone",
            "zonegroup.unknownZone",
            vec![("id", id.to_string())],
        ),
        GroupRefusal::SameOutput(a, b) => (
            StatusCode::CONFLICT,
            "group_same_output",
            "zonegroup.sameOutput",
            vec![("a", a.clone()), ("b", b.clone())],
        ),
        GroupRefusal::AsioOneZoneAtATime(a, b) => (
            StatusCode::CONFLICT,
            "group_asio_one_zone",
            "zonegroup.asioOneZone",
            vec![("a", a.clone()), ("b", b.clone())],
        ),
        GroupRefusal::AsioUnsupported(zone) => (
            StatusCode::BAD_REQUEST,
            "group_asio_unsupported",
            "zonegroup.asioUnsupported",
            vec![("zone", zone.clone())],
        ),
    };
    let corps = crate::message_api::avec_message_en(json!({ "error": code }), lang, cle, &params);
    warn!(code, message = %corps["message"].as_str().unwrap_or_default(), "zone_group_refused");
    (status, Json(corps)).into_response()
}

pub(super) async fn list_groups(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    // Groups stored before #1702 have no `group_id`. The web client keys its
    // "dissolve" button on that field and was putting `undefined` in the URL,
    // so an existing group could not be undone. Derive it from `id` on read.
    for group in &mut groups {
        if group.get("group_id").is_none()
            && let Some(id) = group.get("id").and_then(|v| v.as_i64())
        {
            group["group_id"] = json!(id.to_string());
        }
    }
    let groups = groups
        .into_iter()
        .map(crate::routes::zone_manager::generic_group_view)
        .collect::<Vec<_>>();
    Json(json!(groups))
}

pub(super) async fn create_group(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<CreateGroup>,
) -> Result<impl IntoResponse, AppError> {
    // Premium gate: Multiroom sync requires Premium
    if let Err(resp) = crate::premium_guard::require_premium(
        &state.license,
        tune_core::license::Feature::MultiroomSync,
    )
    .await
    {
        return Ok(resp);
    }

    let lang = crate::i18n::lang_from_header(&headers);
    let zones = ZoneRepo::with_backend(state.backend.clone())
        .list()
        .unwrap_or_default();
    let zone_ids = match validate_group(&body.zone_ids, &zones) {
        Ok(ids) => ids,
        Err(refusal) => return Ok(group_refusal_response(&refusal, &lang)),
    };
    // #7759 — la lecture groupée n'est possible qu'en WASAPI.
    if let Some(refusal) = refus_du_groupe_asio(&state, &zone_ids, &zones).await {
        return Ok(group_refusal_response(&refusal, &lang));
    }

    let name = body
        .name
        .as_deref()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            zone_ids
                .iter()
                .filter_map(|id| zones.iter().find(|z| z.id == Some(*id)))
                .map(|z| z.name.as_str())
                .collect::<Vec<_>>()
                .join(" + ")
        });
    let leader_id = body
        .leader_id
        .filter(|id| zone_ids.contains(id))
        .unwrap_or(zone_ids[0]);
    let sortie_double = match sorties_doubles_du_groupe(&state, leader_id, &zone_ids, &zones).await
    {
        Ok(choix) => choix,
        Err(refusal) => return Ok(group_refusal_response(&refusal, &lang)),
    };

    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // #7937 — une zone n'appartient qu'à UN groupe : celles du nouveau
    // quittent les précédents. Sans cela l'ancien groupe survivait au
    // nouveau (le client web regroupe sans dégrouper d'abord), et défaire
    // l'un laissait l'autre — le groupe « revenait ».
    use tune_core::zones::groupes_enregistres::{prochain_numero, retirer_les_zones};
    // #6132 — un groupe touché perd d'abord sa sortie double : celle de sa
    // meneuse ne doit pas survivre à un groupe défait. Celui qui reste
    // (encore deux zones ou plus) la retrouve, recalculée sur sa nouvelle
    // composition, juste après.
    let touches: Vec<i64> = groups
        .iter_mut()
        .filter(|g| {
            g["zone_ids"].as_array().is_some_and(|a| {
                a.iter()
                    .filter_map(Value::as_i64)
                    .any(|z| zone_ids.contains(&z))
            })
        })
        .filter_map(|g| {
            let gid = g.get("id").and_then(Value::as_i64);
            if g.get("sortie_double").is_some()
                && let Some(ancien_meneur) = g["leader_id"].as_i64()
            {
                poser_la_sortie_double(&settings, g, ancien_meneur, Vec::new());
            }
            gid
        })
        .collect();
    retirer_les_zones(&mut groups, &zone_ids);
    for g in groups.iter_mut().filter(|g| {
        g.get("id")
            .and_then(Value::as_i64)
            .is_some_and(|gid| touches.contains(&gid))
    }) {
        let restants: Vec<i64> = g["zone_ids"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        let Some(meneur) = g["leader_id"]
            .as_i64()
            .or_else(|| restants.first().copied())
        else {
            continue;
        };
        let choix = sorties_doubles_du_groupe(&state, meneur, &restants, &zones)
            .await
            .unwrap_or_default();
        poser_la_sortie_double(&settings, g, meneur, choix);
    }
    let id = prochain_numero(&groups);
    let mut group = json!({
        "id": id,
        "group_id": id.to_string(),
        "name": name,
        "leader_id": leader_id,
        "zone_ids": zone_ids,
    });
    poser_la_sortie_double(&settings, &mut group, leader_id, sortie_double);
    groups.push(group.clone());

    settings
        .set("zone_groups", &serde_json::to_string(&groups)?)
        .ok();
    state.event_bus.emit_typed(
        tune_core::event_types::EventType::GroupCreated,
        json!({ "id": id, "name": name, "zone_ids": zone_ids }),
    );
    // #6155 — un groupe formé autour d'une zone qui joue déjà : les autres la
    // rejoignent tout de suite, sans attendre son prochain geste. Hors de la
    // réponse : prévenir un renderer DLNA peut prendre plusieurs secondes.
    {
        let state = state.clone();
        tokio::spawn(async move {
            super::relais_de_groupe_6155::aligner_les_suiveuses(&state, leader_id).await;
        });
    }
    Ok((
        StatusCode::CREATED,
        Json(crate::routes::zone_manager::generic_group_view(group)),
    )
        .into_response())
}

#[derive(Deserialize)]
pub(super) struct PatchGroup {
    pub(super) name: Option<String>,
    pub(super) zone_ids: Option<Vec<i64>>,
}

#[derive(Deserialize)]
pub(super) struct GroupVolumeRequest {
    pub(super) master_volume: Option<f64>,
    pub(super) offsets: Option<std::collections::HashMap<String, f64>>,
}

pub(super) async fn patch_group(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
    headers: axum::http::HeaderMap,
    Json(body): Json<PatchGroup>,
) -> Result<impl IntoResponse, AppError> {
    let lang = crate::i18n::lang_from_header(&headers);
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let idx = groups
        .iter()
        .position(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match idx {
        Some(i) => {
            if let Some(ref name) = body.name {
                groups[i]["name"] = json!(name);
            }
            if let Some(ref zone_ids) = body.zone_ids {
                // #6132 — la sortie double suit la composition du groupe.
                let zones = ZoneRepo::with_backend(state.backend.clone())
                    .list()
                    .unwrap_or_default();
                // #7759 — étendre un groupe à une zone ASIO est refusé comme
                // le créer.
                if let Some(refusal) = refus_du_groupe_asio(&state, zone_ids, &zones).await {
                    return Ok(group_refusal_response(&refusal, &lang));
                }
                let leader_id = groups[i]["leader_id"]
                    .as_i64()
                    .or_else(|| zone_ids.first().copied())
                    .unwrap_or_default();
                let choix =
                    match sorties_doubles_du_groupe(&state, leader_id, zone_ids, &zones).await {
                        Ok(choix) => choix,
                        Err(refusal) => return Ok(group_refusal_response(&refusal, &lang)),
                    };
                groups[i]["zone_ids"] = json!(zone_ids);
                poser_la_sortie_double(&settings, &mut groups[i], leader_id, choix);
            }
            let result = groups[i].clone();
            settings
                .set("zone_groups", &serde_json::to_string(&groups)?)
                .ok();
            state.event_bus.emit_typed(
                tune_core::event_types::EventType::GroupUpdated,
                json!({ "id": group_id, "group": result }),
            );
            Ok(Json(crate::routes::zone_manager::generic_group_view(result)).into_response())
        }
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

pub(super) async fn group_volume(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
    Json(body): Json<GroupVolumeRequest>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let idx = groups
        .iter()
        .position(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match idx {
        Some(i) => {
            let master = body
                .master_volume
                .unwrap_or(groups[i]["master_volume"].as_f64().unwrap_or(0.5));
            groups[i]["master_volume"] = json!(master);
            if let Some(ref offsets) = body.offsets {
                groups[i]["offsets"] = json!(offsets);
            }
            let zone_ids: Vec<i64> = groups[i]["zone_ids"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                .unwrap_or_default();
            settings
                .set("zone_groups", &serde_json::to_string(&groups)?)
                .ok();

            for zid in &zone_ids {
                let offset = body
                    .offsets
                    .as_ref()
                    .and_then(|o| o.get(&zid.to_string()))
                    .copied()
                    .unwrap_or(0.0);
                let effective = (master + offset).clamp(0.0, 1.0);
                let device_id = ZoneRepo::with_backend(state.backend.clone())
                    .get(*zid)
                    .ok()
                    .flatten()
                    .and_then(|zone| zone.output_device_id);
                if let Err(error) = state
                    .orchestrator
                    .set_volume(*zid, effective, device_id.as_deref())
                    .await
                {
                    return Ok(crate::routes::playback::output_command_error_response(
                        error,
                    ));
                }
            }
            Ok(Json(json!({"group_id": group_id, "master_volume": master})).into_response())
        }
        None => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

pub(super) async fn calibrate_group(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let group = groups
        .iter()
        .find(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match group {
        Some(_) => crate::routes::zone_manager::audio_calibration_unavailable(group_id),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(super) async fn group_health(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let group = groups
        .iter()
        .find(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id));
    match group {
        Some(group) => {
            let zone_ids: Vec<i64> = group["zone_ids"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                .unwrap_or_default();
            let repo = ZoneRepo::with_backend(state.backend.clone());
            let mut zones_health = Vec::new();
            for zid in &zone_ids {
                let ps = state.playback.get_state(*zid).await;
                let zone = repo.get(*zid).ok().flatten();
                let name = zone
                    .map(|z| z.name)
                    .unwrap_or_else(|| format!("Zone {zid}"));
                let online =
                    ps.state != tune_core::playback::PlayState::Stopped || ps.now_playing.is_some();
                zones_health.push(json!({
                    "zone_id": zid,
                    "name": name,
                    "status": if online { "online" } else { "offline" },
                }));
            }
            Json(json!(zones_health)).into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

pub(super) async fn delete_group(
    State(state): State<AppState>,
    Path(group_id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut groups: Vec<Value> = settings
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    // #6132 — dégrouper retire la sortie double du meneur.
    for group in groups
        .iter_mut()
        .filter(|g| g.get("id").and_then(|v| v.as_i64()) == Some(group_id))
    {
        if group.get("sortie_double").is_some()
            && let Some(leader_id) = group["leader_id"].as_i64()
        {
            poser_la_sortie_double(&settings, group, leader_id, Vec::new());
        }
    }
    groups.retain(|g| g.get("id").and_then(|v| v.as_i64()) != Some(group_id));
    settings
        .set("zone_groups", &serde_json::to_string(&groups)?)
        .ok();
    state.event_bus.emit_typed(
        tune_core::event_types::EventType::GroupDeleted,
        json!({ "id": group_id }),
    );
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn list_stereo_pairs(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let pairs: Vec<Value> = settings
        .get("stereo_pairs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Json(json!(pairs))
}

#[derive(Deserialize)]
pub(super) struct CreateStereoPair {
    pub(super) name: String,
    pub(super) left_device_id: String,
    pub(super) right_device_id: String,
}

pub(super) async fn create_stereo_pair(
    State(state): State<AppState>,
    Json(body): Json<CreateStereoPair>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut pairs: Vec<Value> = settings
        .get("stereo_pairs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    let id = pairs.len() as i64 + 1;
    pairs.push(json!({
        "id": id,
        "name": body.name,
        "left_device_id": body.left_device_id,
        "right_device_id": body.right_device_id,
    }));

    settings
        .set("stereo_pairs", &serde_json::to_string(&pairs)?)
        .ok();
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))).into_response())
}

pub(super) async fn delete_stereo_pair(
    State(state): State<AppState>,
    Path(pair_id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut pairs: Vec<Value> = settings
        .get("stereo_pairs")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();

    pairs.retain(|p| p.get("id").and_then(|v| v.as_i64()) != Some(pair_id));
    settings
        .set("stereo_pairs", &serde_json::to_string(&pairs)?)
        .ok();
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn list_group_delays(State(state): State<AppState>) -> Json<Value> {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let raw = settings
        .get("group_delays")
        .unwrap_or(None)
        .unwrap_or_default();
    let delays: Vec<Value> = serde_json::from_str(&raw).unwrap_or_default();
    Json(json!(delays))
}

pub(super) async fn set_group_delay(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let settings = tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone());
    let mut delays: Vec<Value> = settings
        .get("group_delays")
        .unwrap_or(None)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let tech_a = body.get("tech_a").and_then(|v| v.as_str()).unwrap_or("");
    let tech_b = body.get("tech_b").and_then(|v| v.as_str()).unwrap_or("");
    let delay_ms = body.get("delay_ms").and_then(|v| v.as_f64()).unwrap_or(0.0);
    delays.retain(|d| {
        !(d.get("tech_a").and_then(|v| v.as_str()) == Some(tech_a)
            && d.get("tech_b").and_then(|v| v.as_str()) == Some(tech_b))
    });
    delays.push(json!({"tech_a": tech_a, "tech_b": tech_b, "delay_ms": delay_ms}));
    settings
        .set(
            "group_delays",
            &serde_json::to_string(&delays).unwrap_or_default(),
        )
        .ok();
    Json(json!(delays))
}
