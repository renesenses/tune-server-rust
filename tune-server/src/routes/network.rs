use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::error::AppError;
use crate::smb;
use crate::state::AppState;

#[derive(Deserialize)]
struct CreateMount {
    mount_type: Option<String>,
    server: String,
    share: String,
    mount_path: String,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Deserialize)]
struct ScanHostQuery {
    host: String,
    protocol: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

#[derive(Deserialize)]
struct MountRequest {
    host: String,
    share_name: String,
    username: Option<String>,
    password: Option<String>,
    mount_path: Option<String>,
    #[serde(default)]
    dry_run: bool,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/mounts", get(list_mounts).post(create_mount))
        .route("/mounts/{id}", axum::routing::delete(delete_mount))
        .route("/media-servers", get(list_media_servers))
        .route(
            "/library-sources",
            get(super::synchronisation_upnp::list).post(super::synchronisation_upnp::act),
        )
        .route(
            "/media-servers/{id}/library-source",
            post(super::synchronisation_upnp::subscribe),
        )
        .route("/shares", get(list_shares))
        .route("/scan-host", get(scan_host))
        .route("/smb/discover", get(list_smb_shares).post(trigger_smb_scan))
        .route("/smb/mounts", get(list_smb_mounts))
        // Fil 2145 : « Oublier ce partage » DEMONTE puis supprime la ligne.
        // `DELETE /mounts/{id}` supprime la ligne sans demonter : le partage
        // restait monte, invisible, jusqu'au prochain redemarrage.
        .route(
            "/smb/mounts/{id}",
            axum::routing::delete(oublier_un_partage),
        )
        .route("/smb/mount", post(mount_smb_share))
        .route("/media-servers/{id}/browse", get(browse_media_server))
        // Phase 2 du chantier `unifier-serveurs-upnp-et-bibliotheque` :
        // indexer UNE source, choisie à la main. Voir
        // `routes/indexation_upnp.rs` pour la clé d'identité retenue et la
        // mesure qui a écarté l'`ObjectID`.
        .route(
            "/media-servers/{id}/indexer",
            post(crate::routes::indexation_upnp::indexer_une_source),
        )
        // #4624 : le chemin de SORTIE. Indexer existait, retirer n'existait
        // pas — et le seul retrait ecrit (`confirm`) exige un Browse complet,
        // donc un serveur ALLUME. Ces deux routes ne sortent pas sur le
        // reseau : elles fonctionnent serveur eteint, qui est le cas nominal.
        .route(
            "/media-servers/{id}/bibliotheque",
            get(crate::routes::retrait_upnp::apercu_du_retrait)
                .delete(crate::routes::retrait_upnp::retrait_de_la_bibliotheque),
        )
        .route("/media-servers/{id}/search", get(search_media_server))
        .route(
            "/media-servers/{id}/item/{item_id}/stream-url",
            get(media_server_stream_url),
        )
        .route(
            "/media-servers/{id}/item/{item_id}/play/{zone_id}",
            post(play_media_server_item),
        )
        .route("/mounts/test", post(test_mount))
        .route("/shares/{id}", get(get_share_detail))
}

async fn list_mounts(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = state.backend.query_many(
        "SELECT id, mount_type, server, share, mount_path, username, active FROM network_mounts ORDER BY id", &[],
    ).map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "mount_type": r.get(1).and_then(|v| v.as_string()),
                "server": r.get(2).and_then(|v| v.as_string()),
                "share": r.get(3).and_then(|v| v.as_string()),
                "mount_path": r.get(4).and_then(|v| v.as_string()),
                "username": r.get(5).and_then(|v| v.as_string()),
                "active": r.get(6).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

/// L'identite d'un montage : le quadruplet que l'index unique protege
/// (migration 83). Rend l'id de la ligne existante, s'il y en a une.
///
/// GgB (fil 1562, #2453) : sans ce controle, une seconde validation du meme
/// formulaire ajoutait une ligne jumelle que l'ecran Emplacements affichait
/// indefiniment. Depuis l'index unique, elle echouerait a la place — une 500
/// pour un geste anodin. On rend donc la ligne deja la.
fn montage_existant(
    backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>,
    mount_type: &str,
    server: &str,
    share: &str,
    mount_path: &str,
) -> Option<i64> {
    use tune_core::db::backend::ToSqlValue;
    backend
        .query_one(
            "SELECT id FROM network_mounts \
             WHERE mount_type = ? AND server = ? AND share = ? AND mount_path = ?",
            &[
                &mount_type as &dyn ToSqlValue,
                &server as &dyn ToSqlValue,
                &share as &dyn ToSqlValue,
                &mount_path as &dyn ToSqlValue,
            ],
        )
        .ok()
        .flatten()
        .and_then(|row| row.first().and_then(|v| v.as_i64()))
}

async fn create_mount(
    State(state): State<AppState>,
    Json(body): Json<CreateMount>,
) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let mount_type = body.mount_type.unwrap_or_else(|| "smb".into());
    if let Some(id) = montage_existant(
        &state.backend,
        &mount_type,
        &body.server,
        &body.share,
        &body.mount_path,
    ) {
        tracing::info!(id, server = %body.server, share = %body.share, "montage_reseau_deja_enregistre");
        return (StatusCode::OK, Json(json!({ "id": id, "existant": true }))).into_response();
    }
    match state.backend.execute_returning_id(
        "INSERT INTO network_mounts (mount_type, server, share, mount_path, username, password) VALUES (?, ?, ?, ?, ?, ?)",
        &[&mount_type as &dyn ToSqlValue, &body.server as &dyn ToSqlValue, &body.share as &dyn ToSqlValue, &body.mount_path as &dyn ToSqlValue, &body.username as &dyn ToSqlValue, &body.password as &dyn ToSqlValue],
    ) {
        Ok(id) => {
            (StatusCode::CREATED, Json(json!({ "id": id }))).into_response()
        }
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn delete_mount(State(state): State<AppState>, Path(id): Path<i64>) -> impl IntoResponse {
    use tune_core::db::backend::ToSqlValue;
    let p1 = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1".to_string()
    } else {
        "?".to_string()
    };
    state
        .backend
        .execute(
            &format!("DELETE FROM network_mounts WHERE id = {p1}"),
            &[&id as &dyn ToSqlValue],
        )
        .ok();
    StatusCode::NO_CONTENT
}

#[derive(Deserialize, Default)]
struct OublierQuery {
    /// L'utilisateur a confirme : des racines de la bibliotheque dependent du
    /// partage.
    #[serde(default)]
    confirmer: bool,
    /// Retirer aussi ces racines de la bibliotheque, par le chemin existant
    /// de retrait de dossier (decision de Bertrand, 05/10).
    #[serde(default)]
    retirer_racines: bool,
    /// Le nombre de pistes montre a l'utilisateur et accepte : meme contrat
    /// que `confirm_purge` de `POST /system/music-dirs/remove` (#1943).
    #[serde(default)]
    confirmer_purge: Option<u64>,
}

/// Les racines de la bibliotheque qui vivent sous `mount_path` (le point
/// lui-meme, ou un dossier en dessous). Comparaison sur chemins normalises,
/// au separateur pres : `/mnt/nas_Music2` ne depend pas de `/mnt/nas_Music`.
pub(crate) fn racines_dependantes(music_dirs: &[String], mount_path: &str) -> Vec<String> {
    use tune_core::scanner::walker::normalize_path;
    let point = normalize_path(mount_path);
    let point = point.trim_end_matches(['/', '\\']);
    if point.is_empty() {
        return Vec::new();
    }
    music_dirs
        .iter()
        .filter(|d| {
            let d = normalize_path(d);
            let d = d.trim_end_matches(['/', '\\']);
            d == point
                || d.strip_prefix(point)
                    .is_some_and(|reste| reste.starts_with(['/', '\\']))
        })
        .cloned()
        .collect()
}

/// `DELETE /network/smb/mounts/{id}` — « Oublier ce partage » (fil 2145).
///
/// Dans cet ordre : refuser si une racine de la bibliotheque en depend et que
/// l'utilisateur n'a pas confirme (409, avec la liste) ; demonter si le point
/// est monte ; supprimer la ligne ; retirer le point de montage s'il est vide.
///
/// Un demontage qui echoue garde la ligne : supprimer d'abord laisserait un
/// partage monte que plus rien ne nomme, ni l'ecran ni le demarrage.
///
/// Les racines dependantes : `?retirer_racines=true` les retire de la
/// bibliotheque par le chemin de `POST /system/music-dirs/remove`
/// (`retirer_un_dossier`), avec la purge de leurs pistes si
/// `confirmer_purge` couvre le nombre montre dans le 409 (`pistes`). Sans
/// l'option, elles restent declarees ; une racine absente est protegee de la
/// purge par le scan (`verdict_purge`, #1652).
async fn oublier_un_partage(
    _admin: crate::auth::RequireAdmin,
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(q): Query<OublierQuery>,
) -> axum::response::Response {
    use tune_core::db::backend::ToSqlValue;
    let ligne = state
        .backend
        .query_one(
            "SELECT mount_path FROM network_mounts WHERE id = ? AND mount_type = 'smb'",
            &[&id as &dyn ToSqlValue],
        )
        .ok()
        .flatten();
    let Some(mount_path) = ligne.and_then(|r| r.first().and_then(|v| v.as_string())) else {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "partage_inconnu", "message": "Ce partage n'est plus enregistré." })),
        )
            .into_response();
    };

    let racines = racines_dependantes(
        &crate::routes::system::get_music_dirs_list(&state.backend),
        &mount_path,
    );
    if !racines.is_empty() && !q.confirmer {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": "racines_dependantes",
                "message": format!(
                    "{} dossier(s) de la bibliothèque se trouvent sur ce partage.",
                    racines.len()
                ),
                "racines": racines,
                // Ce que la purge emporterait si l'utilisateur retire aussi
                // ces dossiers : il doit le voir avant d'accepter.
                "pistes": crate::routes::system::pistes_qui_partiraient(&state, &racines),
            })),
        )
            .into_response();
    }

    let chemin = std::path::Path::new(&mount_path);
    let mut demonte = false;
    if smb::est_un_point_de_montage(chemin) {
        let res = tokio::time::timeout(
            Duration::from_secs(15),
            Command::new("umount").arg(&mount_path).output(),
        )
        .await;
        let echec = match res {
            Ok(Ok(out)) if out.status.success() => None,
            Ok(Ok(out)) => Some(String::from_utf8_lossy(&out.stderr).trim().to_string()),
            Ok(Err(e)) => Some(e.to_string()),
            Err(_) => Some("délai dépassé".to_string()),
        };
        if let Some(cause) = echec.filter(|_| smb::est_un_point_de_montage(chemin)) {
            warn!(id, path = %mount_path, error = %cause, "smb_oubli_demontage_echoue");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({
                    "error": "demontage_impossible",
                    "message": format!(
                        "Impossible de démonter {mount_path} : {cause}. Le partage est conservé."
                    ),
                })),
            )
                .into_response();
        }
        demonte = true;
    }

    if let Err(e) = state.backend.execute(
        "DELETE FROM network_mounts WHERE id = ?",
        &[&id as &dyn ToSqlValue],
    ) {
        warn!(id, error = %e, "smb_oubli_suppression_echouee");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": "suppression_impossible", "message": e.to_string() })),
        )
            .into_response();
    }
    // `remove_dir` ne retire qu'un dossier VIDE : jamais de la musique.
    if !smb::est_un_point_de_montage(chemin) {
        let _ = std::fs::remove_dir(chemin);
    }
    // Les racines ne partent qu'APRES le demontage et la suppression reussis :
    // un oubli refuse ne doit rien avoir retire de la bibliotheque.
    let mut racines_retirees = Vec::new();
    let mut pistes_retirees = 0u64;
    let mut purge_refusee = false;
    if q.retirer_racines {
        for r in &racines {
            match crate::routes::system::retirer_un_dossier(&state, r, q.confirmer_purge) {
                Ok(v) => {
                    pistes_retirees += v["purged"].as_u64().unwrap_or(0);
                    purge_refusee |= v["purge_refused"].as_bool().unwrap_or(false);
                    racines_retirees.push(r.clone());
                }
                Err(e) => {
                    warn!(id, racine = %r, error = %e.message, "smb_oubli_retrait_racine_echoue")
                }
            }
        }
    }
    info!(
        id, path = %mount_path, demonte, racines = racines.len(),
        racines_retirees = racines_retirees.len(), pistes_retirees, "smb_partage_oublie"
    );
    (
        StatusCode::OK,
        Json(json!({
            "oublie": true,
            "demonte": demonte,
            "racines": racines,
            "racines_retirees": racines_retirees,
            "pistes_retirees": pistes_retirees,
            "purge_refusee": purge_refusee,
        })),
    )
        .into_response()
}

/// Verser dans le registre DURABLE ce que la découverte tient en mémoire.
///
/// Le registre en mémoire (`state.media_servers`) est la vue de la couche
/// SSDP ; la table `media_servers` est la vue qui SURVIT. Les tenir d'accord
/// ici, plutôt que dans `discovery_setup`, a une raison mesurée : la couche
/// SSDP n'émet un évènement qu'à la PREMIÈRE découverte
/// (`SsdpEvent::MediaServerDiscovered`) — un serveur déjà connu qui se
/// réannonce voit sa fraîcheur remise à zéro EN PLACE, dans la carte du
/// SCANNER, sans que rien ne soit publié. Un branchement sur le seul évènement
/// daterait donc chaque serveur de sa première apparition et jamais de la
/// dernière — c'est-à-dire exactement le défaut qu'on corrige.
///
/// ⚠️ Ce raisonnement portait juste sur la couche SSDP et lisait la MAUVAISE
/// carte. `state.media_servers` n'EST PAS la carte du scanner : c'en est une
/// copie, dont le seul écrivain est ce même évènement
/// (`discovery_setup.rs`, `media_servers.lock().await.insert(...)`). Elle était
/// donc gelée à la première découverte, son `Instant` figé pour toujours, et
/// `horodatage_il_y_a(ms.age())` y rendait `maintenant - (maintenant - t0)`,
/// soit `t0`, constant. Mesure du 14/09/2026 sur le `.18` : quatre serveurs,
/// et `first_seen_at == last_seen_at` à la seconde près sur les quatre, à
/// 56 s d'intervalle entre deux relevés (#4125).
///
/// On reprend donc d'abord la fraîcheur de la carte du scanner — celle qui,
/// elle, est tenue à jour — avant de verser quoi que ce soit en base.
///
/// L'observation est datée `maintenant - âge` : on écrit ce que la découverte
/// SAIT, jamais « vu à l'instant ». C'est la contre-épreuve de la phase 1 —
/// un serveur éteint ne doit pas ressusciter parce qu'on a relu la liste.
///
/// Idempotent, quelques lignes au plus, et sans effet de bord visible : une
/// observation ne réécrit ni `first_seen_at`, ni `active`.
pub(super) async fn synchroniser_le_registre(state: &AppState) {
    use tune_core::db::media_server_repo::{
        MediaServerRepo, ObservationServeurRecue, horodatage_il_y_a,
    };

    // La carte du scanner est la seule tenue à jour. La reprise n'insère ni ne
    // retire rien : voir `reprendre_la_fraicheur`, et le rideau de #3688.
    {
        let vue_du_balayage = state.scanner.media_servers().await;
        let mut registre = state.media_servers.lock().await;
        let reprises = tune_core::discovery::presence_serveur::reprendre_la_fraicheur(
            &mut registre,
            vue_du_balayage,
        );
        debug!(reprises, "media_server_fraicheur_reprise_du_balayage");
    }

    // Le verrou est relâché AVANT d'écrire en base : une écriture SQLite sous
    // le mutex du registre ferait attendre la découverte SSDP, qui le prend à
    // chaque annonce reçue.
    let observations: Vec<(ObservationServeurRecue, String)> = {
        let servers = state.media_servers.lock().await;
        servers
            .values()
            .map(|ms| {
                let vu_le = horodatage_il_y_a(ms.age().as_secs() as i64);
                (
                    ObservationServeurRecue {
                        udn: ms.id.clone(),
                        name: ms.name.clone(),
                        manufacturer: Some(ms.manufacturer.clone()).filter(|s| !s.is_empty()),
                        model: Some(ms.model.clone()).filter(|s| !s.is_empty()),
                        device_type: "upnp_media_server".into(),
                        location: ms.location.clone(),
                        content_directory_url: Some(ms.content_directory_url.clone())
                            .filter(|s| !s.is_empty()),
                        host: Some(ms.host.clone()).filter(|s| !s.is_empty()),
                        port: Some(i64::from(ms.port)),
                        max_age_secs: None,
                    },
                    vu_le,
                )
            })
            .collect()
    };

    let repo = MediaServerRepo::with_backend(state.backend.clone());
    for (obs, vu_le) in &observations {
        if let Err(e) = repo.enregistrer_observation_a(obs, vu_le) {
            warn!(udn = %obs.udn, error = %e, "media_server_registre_ecriture_echouee");
        }
    }
}

async fn list_media_servers(State(state): State<AppState>) -> Json<Value> {
    use tune_core::discovery::presence_serveur::{
        ObservationServeur, PART_MAX_ABSENCE_SIMULTANEE, PLANCHER_PLAFOND_ABSENCE,
        SERVEUR_ABSENT_APRES, qualifier_le_registre,
    };

    synchroniser_le_registre(&state).await;

    let repo =
        tune_core::db::media_server_repo::MediaServerRepo::with_backend(state.backend.clone());
    let enregistres = match repo.lister() {
        Ok(v) => v,
        Err(e) => {
            // On ne rend PAS une liste vide sur une erreur de base : une liste
            // vide se lit « aucun serveur », et c'est un mensonge de plus. Le
            // registre en mémoire prend le relais, dégradé mais honnête.
            warn!(error = %e, "media_server_registre_lecture_echouee");
            Vec::new()
        }
    };

    // Le calcul de fraîcheur porte sur la LISTE, jamais sur la ligne : le
    // plafond de bascule en masse ne peut se juger que sur l'ensemble — même
    // raison que `verdict_purge` (`routes/system/scan.rs:454-481`).
    let ages: Vec<Option<i64>> = enregistres.iter().map(|s| s.age_secs()).collect();
    let observations: Vec<ObservationServeur<'_>> = enregistres
        .iter()
        .zip(&ages)
        .map(|(s, age)| ObservationServeur {
            udn: &s.udn,
            age_secs: *age,
            disparition_confirmee: s.absence_reason.as_deref() == Some("disparition_confirmee"),
        })
        .collect();
    let verdict = qualifier_le_registre(&observations);

    // Le constat écrit suit le calcul : la table doit pouvoir se relire seule,
    // sans rejouer la qualification (c'est `network_mounts.mount_state`).
    for (udn, presence) in &verdict.presences {
        let ecriture = match presence.raison() {
            Some(raison) => repo.marquer_absent(udn, raison.code()),
            None => Ok(()),
        };
        if let Err(e) = ecriture {
            warn!(udn = %udn, error = %e, "media_server_constat_ecriture_echouee");
        }
    }

    let items: Vec<Value> = enregistres
        .iter()
        .zip(&ages)
        .map(|(s, age)| {
            let presence = verdict
                .pour(&s.udn)
                .unwrap_or(tune_core::discovery::presence_serveur::PresenceServeur::Present);
            json!({
                "id": s.udn,
                "name": s.name,
                "manufacturer": s.manufacturer.clone().unwrap_or_default(),
                "model": s.model.clone().unwrap_or_default(),
                "host": s.host.clone().unwrap_or_default(),
                "port": s.port.unwrap_or(0),
                "location": s.location,
                // Le marquage demandé par Bertrand dans le fil forum 1425 :
                // l'interface grise un serveur qui ne répond plus au lieu de
                // le faire clignoter en le retirant puis le remettant. Champs
                // AJOUTÉS — aucun client existant ne casse (#2139).
                //
                // Ce que `reachable` MESURE, puisque son nom laissait le doute
                // (#4125) : « le balayage SSDP l'a revu il y a moins de
                // `MEDIA_SERVER_STALE_AFTER` (900 s) ». Ce n'est PAS une sonde
                // HTTP, ce n'est PAS un `Browse` réussi — le registre ne sonde
                // personne au moment de rendre la liste, et il ne doit pas :
                // la route serait alors aussi lente que le plus lent des
                // serveurs du réseau.
                //
                // Il se calcule désormais sur le MÊME âge que `presence`, celui
                // du registre durable. Il lisait jusqu'ici un second `Instant`,
                // celui de la copie en mémoire : deux horloges pour une seule
                // question, et l'une d'elles était gelée. Trois serveurs vivants
                // et qui se réannonçaient correctement portaient `reachable:
                // false` sur le `.18` le 14/09/2026, quinze minutes après leur
                // découverte et pour toujours.
                //
                // Les deux seuils restent distincts et cette hiérarchie est
                // voulue : 900 s marque « plus revu depuis un moment » sans
                // aucune conséquence, 5 400 s (`SERVEUR_ABSENT_APRES`) retire
                // des propositions. Un serveur peut donc être `reachable:
                // false` et `proposable: true` — c'est la zone grise, et c'est
                // exactement ce que le fil 1425 demandait de montrer.
                "reachable": age.is_some_and(|a| tune_core::discovery::ssdp::media_server_reachable(
                    Duration::from_secs(a.max(0) as u64)
                )),
                "last_seen_secs": age.unwrap_or(0),
                // #2219 phase 1 — ce que la liste ne savait pas dire.
                //
                // `last_seen_at` est la réparation la plus concrète : la date
                // était un `Instant` `#[serde(skip)]` (`ssdp.rs:146`), donc
                // rien d'absolu n'était exposable, et le client ne pouvait que
                // relire un âge relatif à un instant qu'il ignorait.
                "presence": presence.code(),
                "proposable": presence.proposable(),
                "absence_reason": presence.raison().map(|r| r.code()),
                "first_seen_at": s.first_seen_at,
                "last_seen_at": s.last_seen_at,
                // L'INTENTION, distincte du constat (`network_mounts`, #1916).
                "active": s.active,
            })
        })
        .collect();

    let total = items.len();
    let proposables = items
        .iter()
        .filter(|i| i.get("proposable").and_then(Value::as_bool) == Some(true))
        .count();
    let mut sortie = json!({
        "items": items,
        "total": total,
        // Combien sont réellement utilisables. `total` seul laissait croire
        // que trois serveurs vus il y a 23 h étaient trois serveurs.
        "proposables": proposables,
        "absent_apres_secs": SERVEUR_ABSENT_APRES.as_secs(),
    });
    // Le refus du plafond est PUBLIÉ, avec ses nombres — comme le refus de
    // purge de `scan.rs:531-536`. Un refus muet serait indébogable.
    if let Some(refus) = verdict.bascule_refusee {
        sortie["bascule_en_masse_refusee"] = json!({
            "candidats": refus.candidats,
            "total": refus.total,
            "plafond": refus.plafond,
            "part_max": PART_MAX_ABSENCE_SIMULTANEE,
            "plancher": PLANCHER_PLAFOND_ABSENCE,
            "confirmation_apres_secs": refus.confirmation_apres_secs,
            "motif": "une bascule de cette ampleur est bien plus souvent notre propre lien réseau \
                      qui tombe qu'une extinction simultanée. Les serveurs restent proposés ; \
                      si le silence dure une seconde fenêtre, l'absence sera actée.",
        });
    }
    Json(sortie)
}

// ---------------------------------------------------------------------------
// SMB discovery and mount management
// ---------------------------------------------------------------------------

/// L'adresse a retenir parmi celles qu'un service mDNS annonce.
///
/// `get_addresses()` est un `HashSet` : `first()` y prenait une adresse au
/// hasard, et un Synology qui publie aussi son IPv6 etait propose tantot sous
/// l'une, tantot sous l'autre. Daniel Levy (fil 2145) s'est retrouve avec le
/// meme partage enregistre deux fois. On prefere l'IPv4, puis une IPv6 hors
/// lien local (une `fe80::` sans zone ne se monte pas), et on trie pour que le
/// choix soit le meme d'une decouverte a l'autre.
pub(crate) fn adresse_preferee(addrs: &[std::net::IpAddr]) -> Option<std::net::IpAddr> {
    let rang = |a: &std::net::IpAddr| match a {
        std::net::IpAddr::V4(_) => 0,
        std::net::IpAddr::V6(v6) if (v6.segments()[0] & 0xffc0) != 0xfe80 => 1,
        std::net::IpAddr::V6(_) => 2,
    };
    addrs.iter().copied().min_by_key(|a| (rang(a), *a))
}

/// Discover network shares via mDNS service browsing (_smb._tcp).
async fn list_shares() -> Json<Value> {
    let result = tokio::task::spawn_blocking(|| {
        let daemon = mdns_sd::ServiceDaemon::new().ok()?;
        let receiver = daemon.browse("_smb._tcp.local.").ok()?;
        let mut shares = Vec::new();
        let mut seen = std::collections::HashSet::new();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                    let host = info.get_hostname().trim_end_matches('.').to_string();
                    let addrs: Vec<std::net::IpAddr> = info
                        .get_addresses()
                        .iter()
                        .map(|a| a.to_ip_addr())
                        .collect();
                    let ip = adresse_preferee(&addrs)
                        .map(|a| a.to_string())
                        .unwrap_or_default();
                    let name = info
                        .get_fullname()
                        .split("._smb._tcp")
                        .next()
                        .unwrap_or(&host)
                        .to_string();
                    let key = format!("{}:{}", ip, info.get_port());
                    if seen.contains(&key) {
                        continue;
                    }
                    seen.insert(key);
                    shares.push(json!({
                        "id": format!("smb://{}", ip),
                        "name": name,
                        "host": if ip.is_empty() { host.clone() } else { ip },
                        "hostname": host,
                        "port": info.get_port(),
                        "protocol": "smb",
                        "available": true,
                    }));
                }
                Ok(_) => {}
                Err(_) => {}
            }
        }
        daemon.shutdown().ok();
        Some(shares)
    })
    .await;

    match result {
        Ok(Some(shares)) => Json(json!(shares)),
        _ => Json(json!([])),
    }
}

/// Scan a specific host for SMB or NFS shares.
async fn scan_host(
    headers: axum::http::HeaderMap,
    Query(q): Query<ScanHostQuery>,
) -> impl IntoResponse {
    let lang = crate::i18n::lang_from_header(&headers);
    let host = &q.host;
    let protocol = q.protocol.as_deref().unwrap_or("smb");

    let raw_output = if protocol == "smb" {
        // Platform-specific SMB share enumeration
        let mut output = String::new();
        let mut success = false;
        let mut last_error = String::new();

        // Windows: net view \\host
        if !success {
            if let Ok(Ok(out)) = tokio::time::timeout(
                Duration::from_secs(10),
                Command::new("net")
                    .args(["view", &format!("\\\\{host}")])
                    .output(),
            )
            .await
            {
                if out.status.success() {
                    output = String::from_utf8_lossy(&out.stdout).to_string();
                    success = true;
                } else {
                    last_error = String::from_utf8_lossy(&out.stderr).to_string();
                }
            }
        }

        // macOS: smbutil view
        if !success {
            let smb_user = q.username.as_deref().unwrap_or("guest");
            let smb_url = if let Some(ref pw) = q.password {
                if !pw.is_empty() {
                    format!("//{}:{}@{}", smb_user, pw, host)
                } else {
                    format!("//{}@{}", smb_user, host)
                }
            } else {
                format!("//{}@{}", smb_user, host)
            };
            if let Ok(Ok(out)) = tokio::time::timeout(
                Duration::from_secs(10),
                Command::new("smbutil").args(["view", &smb_url]).output(),
            )
            .await
            {
                if out.status.success() {
                    output = String::from_utf8_lossy(&out.stdout).to_string();
                    success = true;
                } else {
                    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
                    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
                    if !stdout.trim().is_empty() {
                        output = stdout;
                        success = true;
                    } else {
                        last_error = stderr;
                    }
                }
            }
        }

        // Linux: smbclient -N -L
        if !success {
            let mut smb_args = vec!["-L".to_string(), format!("//{host}")];
            if let Some(ref user) = q.username {
                smb_args.push("-U".to_string());
                if let Some(ref pw) = q.password {
                    if !pw.is_empty() {
                        smb_args.push(format!("{}%{}", user, pw));
                    } else {
                        smb_args.push(user.clone());
                        smb_args.push("-N".to_string());
                    }
                } else {
                    smb_args.push(user.clone());
                    smb_args.push("-N".to_string());
                }
            } else {
                smb_args.push("-N".to_string());
            }
            match tokio::time::timeout(
                Duration::from_secs(10),
                Command::new("smbclient").args(&smb_args).output(),
            )
            .await
            {
                Ok(Ok(out)) => {
                    output = String::from_utf8_lossy(&out.stdout).to_string();
                }
                Ok(Err(e)) => {
                    // smbclient not available — use last_error from previous tools
                    tracing::warn!(host = %host, error = %e, "network_smb_smbclient_spawn_failed (smbclient not installed?)");
                }
                Err(_) => {
                    tracing::warn!(host = %host, "network_smb_scan_timed_out (smbclient -L)");
                    return (
                        StatusCode::GATEWAY_TIMEOUT,
                        Json(json!({ "error": "scan timed out" })),
                    )
                        .into_response();
                }
            }
        }

        if output.trim().is_empty() && !last_error.is_empty() {
            tracing::warn!(host = %host, error = %last_error.trim(), "network_smb_scan_failed");
            let msg = if last_error.contains("Authentication")
                || last_error.contains("auth")
                || last_error.contains("STATUS_ACCESS_DENIED")
            {
                crate::i18n::t(&lang, "net.smbAccessDenied").replace("{error}", &last_error)
            } else {
                crate::i18n::t(&lang, "net.smbScanFailed")
                    .replace("{host}", host)
                    .replace("{error}", &last_error)
            };
            return (StatusCode::OK, Json(json!({ "shares": [], "error": msg }))).into_response();
        }

        output
    } else {
        // NFS: showmount -e host
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            Command::new("showmount").args(["-e", host]).output(),
        )
        .await;
        match result {
            Ok(Ok(out)) => String::from_utf8_lossy(&out.stdout).to_string(),
            Ok(Err(e)) => {
                tracing::warn!(host = %host, error = %e, "network_nfs_showmount_spawn_failed (showmount not installed?)");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("scan failed: {e}") })),
                )
                    .into_response();
            }
            Err(_) => {
                tracing::warn!(host = %host, "network_nfs_scan_timed_out (showmount -e)");
                return (
                    StatusCode::GATEWAY_TIMEOUT,
                    Json(json!({ "error": "scan timed out" })),
                )
                    .into_response();
            }
        }
    };

    // Parse share names from command output.
    let shares: Vec<Value> = if protocol == "smb" {
        // smbclient -L / smbutil view / net view all print a "Sharename Type
        // Comment" table where column 2 is the share TYPE (Disk / Printer /
        // IPC). Keying on that type is far more robust than a prefix filter:
        // the previous filter tested "Sharing" (typo — the header word is
        // "Sharename") so the header was never dropped, and every non-empty
        // line — the header, the `----` rule, client-side Kerberos warnings,
        // `mkdir failed on /var/lib/samba/lock`, `SMB1 disabled…`, the second
        // Server/Workgroup table — was emitted as a bogus "share" (Dominique,
        // Fedora). Only rows whose 2nd column is a real file/printer share type
        // survive; IPC$ (admin share, never a music source) is dropped too.
        raw_output
            .lines()
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() < 2 {
                    return None;
                }
                let name = parts[0];
                let stype = parts[1];
                let is_share_type = stype.eq_ignore_ascii_case("Disk")
                    || stype.eq_ignore_ascii_case("Printer")
                    || stype.eq_ignore_ascii_case("Print");
                // Skip admin/hidden shares ($-suffixed: IPC$, ADMIN$, C$, …).
                if !is_share_type || name.ends_with('$') {
                    return None;
                }
                Some(json!({
                    "name": name,
                    "type": stype,
                    "host": host,
                    "protocol": protocol,
                    "path": format!("//{host}/{name}"),
                }))
            })
            .collect()
    } else {
        // NFS `showmount -e host`: "Export list for host:" header then
        // "/export/path  clients" rows — the export path is column 1.
        raw_output
            .lines()
            .filter(|line| {
                let t = line.trim();
                !t.is_empty() && !t.starts_with("Export") && !t.starts_with("---")
            })
            .filter_map(|line| {
                let parts: Vec<&str> = line.split_whitespace().collect();
                let name = *parts.first()?;
                Some(json!({
                    "name": name,
                    "type": "NFS",
                    "host": host,
                    "protocol": protocol,
                    "path": format!("{host}:{name}"),
                }))
            })
            .collect()
    };

    tracing::info!(
        host = %host,
        protocol,
        shares = shares.len(),
        "network_scan_host_complete"
    );
    Json(json!(shares)).into_response()
}

/// Return cached SMB shares (stub — future mDNS integration).
async fn list_smb_shares() -> Json<Value> {
    Json(json!({
        "items": [],
        "total": 0,
        "message": "SMB share discovery pending",
    }))
}

/// Trigger an SMB network scan using mDNS service discovery.
async fn trigger_smb_scan() -> impl IntoResponse {
    let result = tokio::task::spawn_blocking(|| {
        let daemon = mdns_sd::ServiceDaemon::new().ok()?;
        let receiver = daemon.browse("_smb._tcp.local.").ok()?;
        let mut shares = Vec::new();

        // Collect discoveries for 3 seconds
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            match receiver.recv_timeout(Duration::from_millis(500)) {
                Ok(mdns_sd::ServiceEvent::ServiceResolved(info)) => {
                    shares.push(json!({
                        "name": info.get_fullname(),
                        "host": info.get_hostname(),
                        "port": info.get_port(),
                        "addresses": info.get_addresses()
                            .iter()
                            .map(|a| a.to_ip_addr().to_string())
                            .collect::<Vec<_>>(),
                        "properties": info.get_properties()
                            .iter()
                            .map(|p| (p.key().to_string(), p.val_str().to_string()))
                            .collect::<std::collections::HashMap<_, _>>(),
                    }));
                }
                Ok(_) => {}  // other events (SearchStarted, ServiceFound, etc.)
                Err(_) => {} // recv timeout, continue until deadline
            }
        }
        daemon.shutdown().ok();
        Some(shares)
    })
    .await;

    match result {
        Ok(Some(shares)) => {
            let count = shares.len();
            Json(json!({
                "status": "scan_complete",
                "shares": shares,
                "count": count,
            }))
            .into_response()
        }
        _ => Json(json!({
            "status": "scan_failed",
            "shares": [],
        }))
        .into_response(),
    }
}

/// List all stored SMB mounts from the network_mounts table.
///
/// La liste ne rendait que `active` — l'INTENTION de l'utilisateur. Un partage
/// dont le remontage au demarrage avait echoue s'affichait donc exactement
/// comme un partage monte, et l'echec ne se voyait qu'a la lecture, sous la
/// forme d'une erreur reseau generique qui ne le nommait pas (#1916, Eric
/// `ricouxxx`). Trois champs portent desormais le CONSTAT :
///
/// - `mounted` : verifie a l'instant, sur le systeme de fichiers ;
/// - `mount_state` / `last_mount_error` : ce qu'a donne le dernier essai ;
/// - `smb_version` : le dialecte retenu, que l'interface doit afficher quand
///   il vaut `1.0` — retomber sur un protocole obsolete et non chiffre n'est
///   pas neutre, et se fait aujourd'hui en silence (#1834).
async fn list_smb_mounts(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let rows = state
        .backend
        .query_many(
            "SELECT id, server, share, mount_path, username, active, \
             smb_version, mount_state, last_mount_error \
             FROM network_mounts WHERE mount_type = 'smb' ORDER BY id",
            &[],
        )
        .map_err(|e| AppError::internal(e))?;
    let items: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let mount_path = r.get(3).and_then(|v| v.as_string());
            // Le constat de l'instant prime sur celui du dernier essai : un NAS
            // rallume et remonte a la main doit apparaitre monte, meme si le
            // demarrage s'etait solde par un echec.
            let monte = mount_path
                .as_deref()
                .is_some_and(|p| smb::est_un_point_de_montage(std::path::Path::new(p)));
            json!({
                "id": r.get(0).and_then(|v| v.as_i64()),
                "server": r.get(1).and_then(|v| v.as_string()),
                "share": r.get(2).and_then(|v| v.as_string()),
                "mount_path": mount_path,
                "username": r.get(4).and_then(|v| v.as_string()),
                "active": r.get(5).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
                "smb_version": r.get(6).and_then(|v| v.as_string()),
                "mount_state": r.get(7).and_then(|v| v.as_string()),
                "last_mount_error": r.get(8).and_then(|v| v.as_string()),
                "mounted": monte,
            })
        })
        .collect();
    Ok(Json(json!(items)))
}

/// Mount an SMB share: execute the OS mount command, then persist in the database.

/// Traduire l'échec de création du point de montage en un obstacle NOMMÉ.
///
/// Le message rendu était `failed to create mount dir: Permission denied
/// (os error 13)`. Exact, et inutile : il ne dit pas ce qui manque, et surtout
/// pas que **le montage lui-même** demandera le même privilège juste après —
/// de sorte que créer le dossier à la main ne débloquerait rien.
///
/// Vécu le 2026-08-21 par Dominique Comet, dont le serveur tourne depuis son
/// répertoire personnel et non sous `root` : trois échecs identiques dans ses
/// journaux, un 500 à l'écran, et la conclusion naturelle — mais fausse — que
/// son partage SMB ou son NAS étaient en cause. La découverte avait pourtant
/// réussi juste avant (`shares=1`).
///
/// On sépare donc le refus de privilège du reste : c'est le seul cas où
/// l'utilisateur peut agir, et l'action n'est pas celle qu'il croit.
fn obstacle_de_montage(e: &std::io::Error, chemin: &str) -> (&'static str, String) {
    match e.kind() {
        std::io::ErrorKind::PermissionDenied => (
            "privileges_insuffisants",
            format!(
                "Le serveur n'a pas les droits de créer le point de montage {chemin}. \
                 Monter un partage SMB demande des privilèges système (root, ou la \
                 capacité CAP_SYS_ADMIN) : créer ce dossier à la main ne suffira pas, \
                 car le montage lui-même les redemandera. Deux issues : donner ces \
                 privilèges au service, ou monter le partage par le système \
                 (/etc/fstab) et déclarer le dossier obtenu dans les dossiers de musique."
            ),
        ),
        std::io::ErrorKind::NotFound => (
            "chemin_parent_absent",
            format!("Le dossier parent de {chemin} n'existe pas."),
        ),
        _ => (
            "creation_impossible",
            format!("Impossible de créer le point de montage {chemin} : {e}"),
        ),
    }
}

async fn mount_smb_share(
    State(state): State<AppState>,
    Json(body): Json<MountRequest>,
) -> impl IntoResponse {
    let share_safe = body.share_name.replace(['/', '\\', ' '], "_");
    let chemin_impose = body.mount_path.is_some();
    let mut mount_path = body
        .mount_path
        .clone()
        .unwrap_or_else(|| format!("/mnt/{}_{}", body.host, share_safe));

    // Dry run: just test reachability without mounting
    if body.dry_run {
        let reachable = tokio::net::TcpStream::connect(format!("{}:445", body.host))
            .await
            .is_ok();
        // Le message disait « Host reachable on SMB port 445 », que l'interface
        // affichait en vert comme une validation. Il ne teste QUE l'ouverture du
        // port : ni les identifiants, ni l'existence du partage, ni la
        // possibilite de monter. Chez Philippe Landes il etait au vert et
        // l'etape suivante rendait 500 — un voyant vert juste avant l'etape qui
        // echoue est pire qu'aucun voyant, il envoie chercher la panne du cote
        // du reseau, precisement la seule chose qui ait ete verifiee (#1847).
        return Json(json!({
            "ok": reachable,
            "host": body.host,
            "share_name": body.share_name,
            "message": if reachable {
                "Serveur joignable (port 445) — identifiants et partage non vérifiés"
            } else {
                "Serveur injoignable sur le port 445"
            },
        }))
        .into_response();
    }

    // Fil 2145 (Daniel Levy) : le meme NAS, enregistre une fois sous son IPv6
    // (decouverte) et une fois sous son IPv4 (saisie), donnait deux lignes et
    // deux montages du meme dossier — `montage_existant` compare des textes.
    // On demande donc au serveur son identite SMB2 et on la compare a celle des
    // partages du meme nom deja enregistres sous une AUTRE adresse. Seulement
    // quand le chemin n'est pas impose : un chemin choisi a la main reste
    // celui de l'utilisateur.
    let mut ligne_cible = None;
    let mut serveur_du_jumeau = None;
    if !chemin_impose {
        let lignes = lignes_smb(&state.backend);
        if let Some(j) = jumeau_parmi(&body.host, &body.share_name, lignes, |h| async move {
            smb::guid_du_serveur(&h, 445).await
        })
        .await
        {
            if smb::est_un_point_de_montage(std::path::Path::new(&j.mount_path)) {
                info!(
                    id = j.id, host = %body.host, jumeau = %j.server, share = %body.share_name,
                    "smb_meme_serveur_deja_monte"
                );
                return (
                    StatusCode::OK,
                    Json(json!({
                        "id": j.id,
                        "mounted": true,
                        "mount_path": j.mount_path,
                        "existant": true,
                        "deja_monte": true,
                        "meme_serveur_que": j.server,
                    })),
                )
                    .into_response();
            }
            // Le jumeau n'est pas monte : on monte par la nouvelle adresse, au
            // point du jumeau, et sa ligne passe a cette adresse. Le chemin ne
            // change pas — une racine de bibliotheque peut en dependre.
            info!(
                id = j.id, host = %body.host, jumeau = %j.server, share = %body.share_name,
                "smb_meme_serveur_ligne_reprise"
            );
            mount_path = j.mount_path.clone();
            ligne_cible = Some(j.id);
            serveur_du_jumeau = Some(j.server);
        }
    }

    // Create mount directory
    if let Err(e) = tokio::fs::create_dir_all(&mount_path).await {
        // Journalise AUSSI, et pas seulement dans la reponse HTTP : le client
        // web n'affichait que le statut, donc la cause n'existait nulle part
        // (#1847).
        warn!(host = %body.host, path = %mount_path, error = %e, "smb_mount_dir_failed");
        let (motif, message) = obstacle_de_montage(&e, &mount_path);
        // `message` porte le TEXTE, `error` porte le CODE — et cet ordre n'est
        // pas decoratif : `apiError()` du client lit `detail` ou `message` pour
        // ce qu'il affiche, et range `error` dans un code machine. La reponse
        // d'avant mettait sa phrase dans `error` : elle n'etait donc affichee
        // NULLE PART, et l'utilisateur ne voyait que « 500 Internal Server
        // Error ». C'est ce qui a laisse Dominique Comet sans autre indice que
        // ses journaux.
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "message": message, "error": motif })),
        )
            .into_response();
    }

    // Fil 2145 : le point est DEJA monte. Lancer `mount.cifs` par-dessus
    // rendait `mount error(16)` (EBUSY), puis l'echelle descendait jusqu'a
    // SMB 1.0 et l'utilisateur lisait « Operation not supported ». Le
    // remontage au demarrage faisait ce test (`startup.rs`,
    // `monter_un_partage`) ; la route interactive ne l'avait jamais fait.
    if smb::est_un_point_de_montage(std::path::Path::new(&mount_path)) {
        let source = smb::source_du_montage(std::path::Path::new(&mount_path));
        let admis: Vec<&str> = std::iter::once(body.host.as_str())
            .chain(serveur_du_jumeau.as_deref())
            .collect();
        match source.as_deref() {
            Some(src)
                if !admis
                    .iter()
                    .any(|h| smb::meme_source(src, h, &body.share_name)) =>
            {
                warn!(host = %body.host, path = %mount_path, source = %src, "smb_mount_point_occupe");
                return point_occupe(&mount_path, Some(src));
            }
            _ => {
                // Meme source, ou source illisible (hors Linux) : le partage
                // est la, on l'enregistre et on rend son chemin.
                info!(host = %body.host, share = %body.share_name, path = %mount_path, "smb_mount_deja_monte");
                return persister_le_montage(&state, ligne_cible, &body, &mount_path, None, true);
            }
        }
    }

    // Dialecte qui a effectivement monte le partage, a persister pour que le
    // remontage au demarrage reparte du bon (#1834). Reste NUL sur macOS :
    // `mount_smbfs` negocie seul, des deux cotes, il n'y a rien a retenir.
    let mut dialecte_retenu: Option<String> = None;

    // Build the mount command depending on the platform
    let mount_result = if cfg!(target_os = "macos") {
        let credentials = match (&body.username, &body.password) {
            (Some(u), Some(p)) => format!("{u}:{p}@"),
            (Some(u), None) => format!("{u}@"),
            _ => "guest@".to_string(),
        };
        let unc = format!("//{credentials}{}/{}", body.host, body.share_name);
        tokio::time::timeout(
            Duration::from_secs(15),
            Command::new("mount_smbfs")
                .args([&unc, &mount_path])
                .output(),
        )
        .await
    } else {
        // Linux: mount.cifs, en NEGOCIANT le dialecte au lieu de l'imposer.
        //
        // `vers=3.0` etait code en dur, sans repli ni choix. Or le module CIFS
        // du noyau ne negocie de lui-meme qu'entre 2.1, 3.0 et 3.1.1 : il ne
        // descend jamais plus bas et refuse par `mount error(22): Invalid
        // argument`, un message qui ne dit rien de la cause.
        //
        // Philippe Landes l'a paye cher : `smbclient -L` listait parfaitement
        // le partage ROSEDISK de son NAS Rose, avec les memes identifiants,
        // pendant que le montage echouait. L'asymetrie tient a ce que
        // `smbclient` est du Samba en espace utilisateur — il descend plus bas
        // que le noyau. Le materiel audio embarque souvent un Samba ancien ;
        // tout ce parc etait donc inaccessible, sans que rien ne l'explique.
        //
        // On essaie donc, dans l'ordre : negociation libre (le noyau prend le
        // meilleur dialecte moderne), puis 2.0, puis 1.0. Le premier qui monte
        // gagne.
        //
        // L'echelle vit desormais dans `crate::smb` : le remontage au demarrage
        // doit imperativement essayer les MEMES dialectes, dans le MEME ordre.
        // Il ne le faisait pas — il imposait toujours `vers=3.0` — et le partage
        // que cette route venait de monter en SMB 1.0 se perdait au premier
        // redemarrage (#1834).
        let user = body.username.as_deref().unwrap_or("guest");
        let pass = body.password.as_deref().unwrap_or("");
        let unc = format!("//{}/{}", body.host, body.share_name);

        let mut dernier = None;
        for dialecte in smb::DIALECTES {
            let opts = smb::options_de_montage(user, pass, dialecte);
            // JAMAIS `opts` dans une trace : il porte le mot de passe.
            info!(
                host = %body.host,
                share = %body.share_name,
                dialect = smb::etiquette(dialecte),
                "smb_mount_attempt"
            );
            let res = tokio::time::timeout(
                smb::ESSAI_TIMEOUT,
                Command::new("mount.cifs")
                    .args([&unc, &mount_path, "-o", &opts])
                    .output(),
            )
            .await;

            let arreter = match &res {
                Ok(Ok(out)) if out.status.success() => {
                    info!(
                        host = %body.host,
                        share = %body.share_name,
                        dialect = smb::etiquette(dialecte),
                        "smb_mount_ok"
                    );
                    // Le dialecte qui a gagne doit survivre a la reponse HTTP :
                    // c'est lui que le remontage au demarrage rejouera.
                    dialecte_retenu = Some(smb::etiquette(dialecte).to_string());
                    true
                }
                Ok(Ok(out)) => {
                    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                    // Sans cette trace, un echec de montage ne laissait AUCUNE
                    // marque nulle part : ni a l'ecran (le client jetait le
                    // corps de la reponse), ni au journal. Le diagnostic
                    // existait deux fois et disparaissait deux fois.
                    warn!(
                        host = %body.host,
                        share = %body.share_name,
                        dialect = smb::etiquette(dialecte),
                        error = %stderr,
                        "smb_mount_failed"
                    );
                    // Un refus d'authentification ne se repare pas en changeant
                    // de dialecte : inutile de faire patienter l'utilisateur
                    // vingt secondes de plus pour la meme reponse. Un point
                    // deja occupe (EBUSY) non plus (fil 2145).
                    smb::arrete_l_echelle(&stderr)
                }
                Ok(Err(e)) => {
                    // mount.cifs absent ou non executable : reessayer avec un
                    // autre dialecte ne changera rien.
                    warn!(host = %body.host, error = %e, "smb_mount_command_failed");
                    true
                }
                Err(_) => {
                    warn!(
                        host = %body.host,
                        dialect = smb::etiquette(dialecte),
                        "smb_mount_timeout"
                    );
                    false
                }
            };
            dernier = Some(res);
            if arreter {
                break;
            }
        }
        dernier.expect("DIALECTES n'est jamais vide")
    };

    match mount_result {
        Ok(Ok(out)) if out.status.success() => {}
        Ok(Ok(out)) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            // La vraie cause, et non l'erreur du dernier dialecte essaye.
            if smb::est_deja_monte(&stderr) {
                return point_occupe(&mount_path, None);
            }
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("mount failed: {stderr}") })),
            )
                .into_response();
        }
        Ok(Err(e)) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("mount command failed: {e}") })),
            )
                .into_response();
        }
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({ "error": "mount timed out" })),
            )
                .into_response();
        }
    };

    persister_le_montage(
        &state,
        ligne_cible,
        &body,
        &mount_path,
        dialecte_retenu,
        false,
    )
}

/// 409 : le point de montage est occupe par autre chose que ce partage.
fn point_occupe(mount_path: &str, source: Option<&str>) -> axum::response::Response {
    let message = match source {
        Some(src) => format!(
            "Le point de montage {mount_path} est déjà occupé par un autre montage ({src}). \
             Démontez-le, ou choisissez un autre point de montage."
        ),
        None => format!(
            "Le point de montage {mount_path} est déjà occupé (le système répond « Device or \
             resource busy »). Le partage y est peut-être déjà monté."
        ),
    };
    (
        StatusCode::CONFLICT,
        Json(json!({ "message": message, "error": "point_de_montage_occupe" })),
    )
        .into_response()
}

/// Une ligne SMB enregistree : `(id, server, share, mount_path)`.
type LigneSmb = (i64, String, String, String);

fn lignes_smb(backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>) -> Vec<LigneSmb> {
    backend
        .query_many(
            "SELECT id, server, share, mount_path FROM network_mounts \
             WHERE mount_type = 'smb' ORDER BY id",
            &[],
        )
        .unwrap_or_default()
        .into_iter()
        .filter_map(|r| {
            Some((
                r.first()?.as_i64()?,
                r.get(1)?.as_string()?,
                r.get(2)?.as_string()?,
                r.get(3)?.as_string()?,
            ))
        })
        .collect()
}

/// Une ligne qui designe le MEME partage du MEME serveur, sous une autre
/// adresse.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Jumeau {
    pub id: i64,
    pub server: String,
    pub mount_path: String,
}

/// Chercher, parmi `lignes`, le meme partage du meme serveur enregistre sous
/// une autre adresse que `hote` (fil 2145 : IPv6 et IPv4 d'un Synology).
///
/// `sonde` rend l'identite SMB2 (`ServerGuid`) d'une adresse ; elle est
/// injectee pour l'epreuve. Si `hote` ne repond pas ou n'a pas d'identite, on
/// ne conclut rien : mieux vaut une ligne en trop que deux NAS confondus.
pub(crate) async fn jumeau_parmi<S, F>(
    hote: &str,
    partage: &str,
    lignes: Vec<LigneSmb>,
    mut sonde: S,
) -> Option<Jumeau>
where
    S: FnMut(String) -> F,
    F: std::future::Future<Output = Option<smb::GuidServeur>>,
{
    let hote_nu = hote.trim_start_matches('[').trim_end_matches(']');
    let candidats: Vec<LigneSmb> = lignes
        .into_iter()
        .filter(|(_, server, share, _)| {
            share.eq_ignore_ascii_case(partage)
                && !server
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .eq_ignore_ascii_case(hote_nu)
        })
        .collect();
    if candidats.is_empty() {
        return None;
    }
    let identite = sonde(hote.to_string()).await?;
    for (id, server, _, mount_path) in candidats {
        if sonde(server.clone()).await == Some(identite) {
            return Some(Jumeau {
                id,
                server,
                mount_path,
            });
        }
    }
    None
}

/// Enregistrer le montage qui vient d'etre etabli (ou constate, `deja_monte`).
///
/// `ligne_cible` : la ligne d'un jumeau (meme serveur, autre adresse) a
/// reprendre plutot que d'en ajouter une.
fn persister_le_montage(
    state: &AppState,
    ligne_cible: Option<i64>,
    body: &MountRequest,
    mount_path: &str,
    dialecte_retenu: Option<String>,
    deja_monte: bool,
) -> axum::response::Response {
    use tune_core::db::backend::ToSqlValue;
    let mount_path = mount_path.to_string();
    // Remonter un partage deja enregistre passe souvent par cet ecran plutot
    // que par le bouton de remontage : sans ce controle on ajoutait une ligne
    // jumelle (#2453), et depuis l'index unique on echouerait. On rafraichit
    // la ligne existante — le dialecte retenu et le constat de montage sont
    // justement ce qui vient d'etre etabli.
    let existante = ligne_cible.or_else(|| {
        montage_existant(
            &state.backend,
            "smb",
            &body.host,
            &body.share_name,
            &mount_path,
        )
    });
    if let Some(id) = existante {
        let res = if deja_monte {
            // Rien n'a ete monte : ni les identifiants saisis ni un dialecte
            // n'ont ete eprouves. On ne note que le constat — ecraser les
            // identifiants qui montent ce partage au demarrage par ceux d'un
            // formulaire peut-etre vide le perdrait au prochain redemarrage.
            state.backend.execute(
                "UPDATE network_mounts SET mount_state = ?, active = 1 WHERE id = ?",
                &[&"mounted" as &dyn ToSqlValue, &id as &dyn ToSqlValue],
            )
        } else {
            state.backend.execute(
                "UPDATE network_mounts SET server = ?, mount_path = ?, username = ?, \
                 password = ?, smb_version = COALESCE(?, smb_version), \
                 mount_state = ?, active = 1 WHERE id = ?",
                &[
                    &body.host as &dyn ToSqlValue,
                    &mount_path as &dyn ToSqlValue,
                    &body.username as &dyn ToSqlValue,
                    &body.password as &dyn ToSqlValue,
                    &dialecte_retenu as &dyn ToSqlValue,
                    &"mounted" as &dyn ToSqlValue,
                    &id as &dyn ToSqlValue,
                ],
            )
        };
        if let Err(e) = res {
            warn!(id, error = %e, "montage_reseau_rafraichissement_echoue");
        }
        tracing::info!(id, host = %body.host, share = %body.share_name, deja_monte, "montage_reseau_rafraichi");
        return (
            StatusCode::OK,
            Json(json!({
                "id": id,
                "mounted": true,
                "mount_path": mount_path,
                "smb_version": dialecte_retenu,
                "existant": true,
                "deja_monte": deja_monte,
            })),
        )
            .into_response();
    }
    match state.backend.execute_returning_id(
        // Le mot de passe est persiste AVEC le reste. Sans lui, le partage
        // enregistre est inexploitable au redemarrage : Tune connait l'adresse
        // et l'identifiant, pas le secret, donc il ne peut pas remonter — et
        // l'utilisateur doit re-saisir son partage ET ses identifiants a chaque
        // fois (Dominique Comet, #1692 : « il faut que je relance Ajouter un
        // partage reseau SMB, que je rechoisisse le disque avec les identifiants
        // adequats »).
        //
        // Ce n'est pas une nouvelle exposition : la route de montage generique
        // (`create_mount`) enregistre deja le mot de passe dans cette meme
        // colonne, et la meme base porte les jetons de streaming. Le chiffrer
        // ici seul donnerait l'illusion d'une protection sans en apporter —
        // `secret_envelope` exige une passphrase utilisateur, incompatible avec
        // un remontage sans personne devant la machine.
        //
        // `smb_version` retient le dialecte qui a gagne. Sans lui, le remontage
        // au demarrage repartait de `vers=3.0` en dur : le partage SMB 1.0 de
        // Philippe Landes montait ici, puis disparaissait au premier
        // redemarrage (#1834). `mount_state` porte le CONSTAT, la ou `active`
        // n'exprime qu'une intention (#1916) — on n'arrive ici qu'apres un
        // montage reussi, d'ou 'mounted'.
        "INSERT INTO network_mounts (mount_type, server, share, mount_path, username, password, smb_version, mount_state) VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[&"smb" as &dyn ToSqlValue, &body.host as &dyn ToSqlValue, &body.share_name as &dyn ToSqlValue, &mount_path as &dyn ToSqlValue, &body.username as &dyn ToSqlValue, &body.password as &dyn ToSqlValue, &dialecte_retenu as &dyn ToSqlValue, &"mounted" as &dyn ToSqlValue],
    ) {
        Ok(id) => {
            (
                StatusCode::CREATED,
                Json(json!({
                    "id": id,
                    "mounted": true,
                    "deja_monte": deja_monte,
                    "mount_path": mount_path,
                    "smb_version": dialecte_retenu,
                })),
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("db error: {e}") })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Media Server browsing / streaming
// ---------------------------------------------------------------------------

async fn browse_media_server(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<BrowseQuery>,
) -> Result<Json<Value>, AppError> {
    let object_id = q.object_id.as_deref().unwrap_or("0");
    let servers = state.media_servers.lock().await;
    // Un serveur que la carte mémoire ne connaît pas (démarrage avant la
    // première découverte, serveur oublié) sortait par la même porte qu'un
    // dossier vide : 200, listes vides, pas un mot. L'écran affichait un
    // dossier vide — lu « pas d'ouverture des dossiers » (Yacine, #4134).
    let ms = servers
        .get(&id)
        .cloned()
        .ok_or_else(|| EchecParcours::ServeurInconnu(id.clone()).en_erreur_http("?"))?;
    drop(servers);

    let (containers, items, total_matches, incomplet) =
        parcourir_les_enfants(&ms.content_directory_url, &ms.name, object_id)
            .await
            .map_err(|e| e.en_erreur_http(&ms.name))?;
    let fetched = containers.len() + items.len();
    let total = (total_matches as usize).max(fetched);
    // #4895 : un parcours arrêté en route (page vide au milieu, DIDL illisible
    // sur une page suivante, catalogue qui change…) servait ce qu'il avait lu
    // en 200, sans un mot — l'écran le prenait pour la liste entière. Ce qui a
    // été lu reste servi, mais la réponse le DIT, et le journal aussi.
    if let Some(raison) = &incomplet {
        warn!(
            serveur = %ms.name,
            object_id,
            lus = fetched,
            annonces = total,
            error = %raison,
            "browse_media_server_incomplete"
        );
    }
    Ok(Json(json!({
        "object_id": object_id,
        "containers": containers,
        "items": items,
        "total_matches": total,
        "number_returned": fetched,
        "complet": incomplet.is_none(),
        "incomplet": incomplet,
    })))
}

/// Pourquoi un `Browse` n'a rien rendu — pour que l'écran puisse le dire au
/// lieu d'afficher un dossier vide (#4134).
///
/// Jusqu'ici tout échec sortait par la porte du dossier vide : serveur absent
/// de la carte, délai ou erreur de transport, réponse HTTP 500/401 ou SOAP
/// Fault dont le corps partait dans l'analyseur DIDL, qui rendait `(0, 0)`.
/// Un 200 à listes vides ne rejette pas côté client : `folderNoAnswer` ne
/// s'affichait jamais.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EchecParcours {
    /// L'identifiant n'est pas dans la carte mémoire des serveurs.
    ServeurInconnu(String),
    /// Le POST SOAP n'est pas parti ou n'est pas revenu (délai de 10 s compris).
    Transport(String),
    /// Le serveur a répondu, mais hors 2xx.
    Statut(u16),
    /// 2xx sans élément `<Result>` — un SOAP Fault, ou autre chose qu'un
    /// ContentDirectory.
    SansResultat,
    /// #4895 — la réponse n'a pas de `NumberReturned` lisible : on ne sait pas
    /// ce que la page était censée contenir.
    SansCompteur,
    /// #4895 — la page n'annonce pas de `TotalMatches` lisible.
    SansTotal,
    /// #4895 — Tune n'a su lire que `lus` des `annonces` éléments que la page
    /// déclare (`NumberReturned`) : un DIDL que l'analyseur ne comprend pas.
    DidlIllisible { lus: u32, annonces: u32 },
    /// #4895 — une page vide alors que `total` éléments sont annoncés et que
    /// seuls `lus` ont été lus : « 0/12 » sur la première page.
    PaginationInterrompue { lus: u32, total: u32 },
}

impl EchecParcours {
    /// La réponse HTTP de `GET /media-servers/{id}/browse` : 404 pour
    /// l'inconnu, 504 pour le transport, 502 pour un serveur qui répond mal.
    /// Le message nomme le serveur et la cause ; le code reste stable pour
    /// les clients qui voudront le lire.
    fn en_erreur_http(&self, nom_du_serveur: &str) -> AppError {
        self.en_erreur_http_de(nom_du_serveur, "Browse")
    }

    /// La même réponse pour une autre action ContentDirectory — `Search`
    /// (#4895) : même statut, même message à l'action près, mais son propre
    /// code stable et sa propre ligne de journal, pour qu'on ne confonde pas
    /// une recherche en échec avec un dossier qui ne s'ouvre pas.
    fn en_erreur_http_de(&self, nom_du_serveur: &str, action: &str) -> AppError {
        let (status, message) = self.statut_et_message(nom_du_serveur, action);
        let code = if action == "Search" {
            warn!(serveur = nom_du_serveur, error = %message, "search_media_server_failed");
            "media_server_search_failed"
        } else {
            warn!(serveur = nom_du_serveur, error = %message, "browse_media_server_failed");
            "media_server_browse_failed"
        };
        AppError {
            status,
            message,
            code: Some(code.into()),
        }
    }

    fn statut_et_message(&self, nom_du_serveur: &str, action: &str) -> (StatusCode, String) {
        match self {
            EchecParcours::ServeurInconnu(id) => (
                StatusCode::NOT_FOUND,
                format!(
                    "serveur multimédia {id} inconnu du registre en mémoire — \
                     pas encore découvert, ou oublié"
                ),
            ),
            EchecParcours::Transport(e) => (
                StatusCode::GATEWAY_TIMEOUT,
                format!("{nom_du_serveur} n'a pas répondu au {action} : {e}"),
            ),
            EchecParcours::Statut(code) => (
                StatusCode::BAD_GATEWAY,
                format!("{nom_du_serveur} a répondu HTTP {code} au {action}"),
            ),
            EchecParcours::SansResultat => (
                StatusCode::BAD_GATEWAY,
                format!(
                    "{nom_du_serveur} a répondu sans élément <Result> au {action} \
                     (SOAP Fault ?)"
                ),
            ),
            EchecParcours::SansCompteur => (
                StatusCode::BAD_GATEWAY,
                format!(
                    "{nom_du_serveur} a répondu au {action} sans compteur \
                     NumberReturned lisible"
                ),
            ),
            EchecParcours::SansTotal => (
                StatusCode::BAD_GATEWAY,
                format!(
                    "{nom_du_serveur} a répondu au {action} sans compteur \
                     TotalMatches lisible"
                ),
            ),
            EchecParcours::DidlIllisible { lus, annonces } => (
                StatusCode::BAD_GATEWAY,
                format!(
                    "{nom_du_serveur} a rendu au {action} un DIDL que Tune ne sait \
                     pas lire : {lus} des {annonces} éléments annoncés lus"
                ),
            ),
            EchecParcours::PaginationInterrompue { lus, total } => (
                StatusCode::BAD_GATEWAY,
                format!(
                    "{nom_du_serveur} a rendu une page vide au {action} après \
                     {lus} des {total} éléments annoncés"
                ),
            ),
        }
    }
}

/// Parcourt TOUTES les pages d'un conteneur d'un serveur ContentDirectory.
///
/// Extrait tel quel du corps de [`browse_media_server`], qui l'appelle
/// toujours — l'indexation de la phase 2 du chantier
/// `unifier-serveurs-upnp-et-bibliotheque` a besoin du MEME parcours, page par
/// page, et le recopier aurait fait diverger deux lecteurs du même protocole.
///
/// Rend `(conteneurs, items, total_matches, incomplet)`. Un `total_matches` de
/// 0 avec des items rendus signifie seulement que le serveur ne l'annonce pas.
/// `incomplet` porte la raison d'un parcours arrêté APRÈS avoir lu quelque
/// chose (#4895) : ce qui a été lu est servi, mais ce n'est pas tout.
// UPnP Browse returns results in PAGES. The old code issued a single
// Browse with RequestedCount=200 and returned only that page, so a server
// with thousands of albums showed just its first page (~100 on MinimServer /
// Twonky / Asset, which cap a single response) — "le résumé est juste mais la
// liste est très incomplète (~100 sur x xxx)" (Pierre M). Loop over
// StartingIndex, accumulating children until NumberReturned==0 or
// StartingIndex>=TotalMatches, with a safety bound.
/// Navigation keeps the successfully read pages; indexing also inspects `erreur`.
pub(crate) async fn parcourir_les_enfants(
    content_directory_url: &str,
    nom_du_serveur: &str,
    object_id: &str,
) -> Result<(Vec<Value>, Vec<Value>, u32, Option<String>), EchecParcours> {
    let p = parcourir_les_enfants_verifie(content_directory_url, nom_du_serveur, object_id).await;
    // #4134 : l'échec de la PREMIÈRE page est celui du dossier — rien n'a été
    // lu, la route doit le dire au lieu de rendre un dossier vide muet. Un
    // échec survenu plus loin laisse ce qui a été lu, comme avant.
    if p.conteneurs.is_empty() && p.items.is_empty() {
        if let Some(cause) = p.cause {
            return Err(cause);
        }
    }
    Ok((p.conteneurs, p.items, p.total, p.erreur))
}

pub(crate) struct ParcoursEnfants {
    pub conteneurs: Vec<Value>,
    pub items: Vec<Value>,
    pub total: u32,
    pub erreur: Option<String>,
    /// La même défaillance, typée, pour la route `browse` (#4134) : elle en
    /// tire 404 / 502 / 504 au lieu d'un dossier vide muet. Renseignée à TOUS
    /// les échecs qui peuvent frapper la PREMIÈRE page — #4895 : un compteur
    /// absent, un DIDL illisible ou une page vide sur N annoncés le peuvent
    /// aussi, et sans cause ils rendaient un 200 vide. Les incidents qui ne
    /// surviennent qu'après une page lue (catalogue changé, plafond…) n'en
    /// ont pas besoin : la route les signale par `incomplet`.
    pub cause: Option<EchecParcours>,
}

/// Reject incomplete XML even when its counters happen to describe zero items.
fn xml_complet(xml: &str, root: &[u8]) -> bool {
    use quick_xml::{Reader, events::Event};
    let mut reader = Reader::from_str(xml);
    let mut depth = 0usize;
    let mut seen = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                if depth == 0 {
                    if seen || e.local_name().as_ref() != root {
                        return false;
                    }
                    seen = true;
                }
                depth += 1;
            }
            Ok(Event::Empty(e)) if depth == 0 => {
                if seen || e.local_name().as_ref() != root {
                    return false;
                }
                seen = true;
            }
            Ok(Event::End(_)) => {
                if depth == 0 {
                    return false;
                }
                depth -= 1;
            }
            Ok(Event::Text(e)) if depth == 0 && !e.iter().all(u8::is_ascii_whitespace) => {
                return false;
            }
            Ok(Event::Eof) => return seen && depth == 0,
            Err(_) => return false,
            _ => {}
        }
    }
}

/// A failed or truncated Browse must never authorize reconciliation.
pub(crate) async fn parcourir_les_enfants_verifie(
    content_directory_url: &str,
    nom: &str,
    object_id: &str,
) -> ParcoursEnfants {
    let mut p = ParcoursEnfants {
        conteneurs: vec![],
        items: vec![],
        total: 0,
        erreur: None,
        cause: None,
    };
    let mut start = 0u32;
    let mut update_id: Option<String> = None;
    let mut total_annonce: Option<u32> = None;
    let mut termine = false;
    let object_id = object_id
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    for _ in 0..500 {
        let body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
<u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
<ObjectID>{object_id}</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter>
<StartingIndex>{start}</StartingIndex><RequestedCount>200</RequestedCount><SortCriteria></SortCriteria>
</u:Browse></s:Body></s:Envelope>"#
        );
        let response = tune_core::http::client::shared()
            .post(content_directory_url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header(
                "SOAPAction",
                "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"",
            )
            .body(body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await;
        let response = match response.and_then(reqwest::Response::error_for_status) {
            Ok(r) => r,
            Err(e) => {
                p.erreur = Some(format!("{nom} : échec de lecture à l’index {start} : {e}"));
                // `error_for_status` replie le hors-2xx dans la même erreur que
                // le transport : `status()` les départage, et c'est ce qui
                // sépare un 502 d'un 504 pour l'appelant (#4134).
                p.cause = Some(match e.status() {
                    Some(code) => EchecParcours::Statut(code.as_u16()),
                    None => EchecParcours::Transport(e.to_string()),
                });
                break;
            }
        };
        let body = match response.text().await {
            Ok(b) => b,
            Err(e) => {
                p.erreur = Some(format!("{nom} : réponse interrompue : {e}"));
                p.cause = Some(EchecParcours::Transport(e.to_string()));
                break;
            }
        };
        let returned =
            extract_xml_tag(&body, "NumberReturned").and_then(|v| v.trim().parse::<u32>().ok());
        let total =
            extract_xml_tag(&body, "TotalMatches").and_then(|v| v.trim().parse::<u32>().ok());
        let valid_result = extract_xml_tag(&body, "Result").is_some_and(|raw| {
            if raw.trim().is_empty() {
                return returned == Some(0) && total == Some(0);
            }
            if raw.trim_start().starts_with('<') {
                return xml_complet(&raw, b"DIDL-Lite");
            }
            quick_xml::escape::unescape(&raw)
                .is_ok_and(|decoded| xml_complet(&decoded, b"DIDL-Lite"))
        });
        if !xml_complet(&body, b"Envelope") || !valid_result {
            p.erreur = Some(format!("{nom} : réponse XML incomplète ou invalide"));
            // Un SOAP Fault n'a pas d'élément `<Result>` ; un DIDL tronqué en a
            // un mais mal formé. Les deux disent la même chose à l'appelant :
            // le serveur a répondu, mais pas un catalogue lisible — 502.
            p.cause = Some(EchecParcours::SansResultat);
            break;
        }
        let update = extract_xml_tag(&body, "UpdateID");
        let (mut containers, mut items) = parse_didl_browse_response(&body);
        let parsed = (containers.len() + items.len()) as u32;
        // Missing counters, malformed XML/HTML and SOAP faults are not empty libraries.
        let Some(returned) = returned else {
            p.erreur = Some(format!("{nom} : réponse Browse sans compteur valide"));
            p.cause = Some(EchecParcours::SansCompteur);
            break;
        };
        if returned != parsed {
            p.erreur = Some(format!(
                "{nom} : réponse Browse incomplète ({parsed}/{returned})"
            ));
            p.cause = Some(EchecParcours::DidlIllisible {
                lus: parsed,
                annonces: returned,
            });
            break;
        }
        let Some(total) = total else {
            p.erreur = Some(format!(
                "{nom} : réponse Browse incomplète ({parsed}/{returned}, sans TotalMatches)"
            ));
            p.cause = Some(EchecParcours::SansTotal);
            break;
        };
        if total_annonce.is_some_and(|t| t != total) || (start > 0 && update != update_id) {
            p.erreur = Some(format!("{nom} : le catalogue a changé pendant le parcours"));
            break;
        }
        total_annonce = Some(total);
        update_id = update;
        p.total = total;
        p.conteneurs.append(&mut containers);
        p.items.append(&mut items);
        if returned == 0 {
            if total != 0 && start < total {
                p.erreur = Some(format!("{nom} : pagination interrompue ({start}/{total})"));
                // #4895 : sur la PREMIÈRE page (« 0/12 »), c'est l'échec du
                // dossier entier ; plus loin, `incomplet` le dit à la route.
                p.cause = Some(EchecParcours::PaginationInterrompue { lus: start, total });
            } else {
                termine = true;
            }
            break;
        }
        let Some(next) = start.checked_add(returned) else {
            p.erreur = Some(format!("{nom} : débordement de pagination"));
            break;
        };
        start = next;
        if total > 0 && start >= total {
            if start == total {
                termine = true;
            } else {
                p.erreur = Some(format!("{nom} : pagination incohérente ({start}/{total})"));
            }
            break;
        }
    }
    if !termine && p.erreur.is_none() {
        p.erreur = Some(format!("{nom} : plafond de 500 pages atteint"));
    }
    if let Some(e) = &p.erreur {
        tracing::warn!("{e}");
    }
    p
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    /// Le texte cherché.
    q: String,
    /// Le conteneur où chercher. Absent = tout le serveur (`0`).
    container: Option<String>,
}

/// Cherche DANS un serveur de médias, par son action ContentDirectory `Search`.
///
/// Pourquoi ce n'est pas un simple `Browse` filtré : parcourir une
/// arborescence de plusieurs milliers d'entrées côté client pour y chercher un
/// titre est intenable, et c'est précisément ce que `Search` évite — le
/// serveur cherche dans SON index.
///
/// La règle du chantier, symétrique de celle qu'on s'applique à nous-mêmes
/// (#2312) : **ne demander que ce que le serveur distant annonce**. On lit donc
/// d'abord ses `SearchCapabilities`. S'il n'annonce pas `dc:title`, on ne lui
/// envoie pas de critère qu'il ne sait pas évaluer — beaucoup répondent alors
/// par toute la bibliothèque, ce qui ressemble à un résultat et n'en est pas.
/// La réponse porte `supported: false` et le client se rabat sur un filtrage
/// du dossier courant, en le disant à l'écran.
async fn search_media_server(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<SearchQuery>,
) -> Result<Json<Value>, AppError> {
    let container = q.container.as_deref().unwrap_or("0");
    let vide = |supported: bool, raison: &str| {
        Json(json!({
            "container": container,
            "query": q.q,
            "supported": supported,
            "reason": raison,
            "containers": [],
            "items": [],
            "total_matches": 0,
            "number_returned": 0,
        }))
    };

    let servers = state.media_servers.lock().await;
    let ms = match servers.get(&id) {
        Some(ms) => ms.clone(),
        None => return Ok(vide(false, "serveur inconnu")),
    };
    drop(servers);

    if q.q.trim().is_empty() {
        return Ok(vide(true, ""));
    }

    let caps = capacites_de_recherche(&ms.content_directory_url).await;
    let criteria = match critere_de_recherche(&caps, &q.q) {
        Some(c) => c,
        None => {
            return Ok(vide(
                false,
                "ce serveur n'annonce pas la recherche par titre",
            ));
        }
    };

    const PAGE_SIZE: u32 = 200;
    const MAX_PAGES: u32 = 50;
    let client = tune_core::http::client::shared();
    let mut containers: Vec<Value> = Vec::new();
    let mut items: Vec<Value> = Vec::new();
    let mut starting_index: u32 = 0;
    let mut total_matches: u32 = 0;
    let mut cause: Option<EchecParcours> = None;
    let mut termine = false;

    for _page in 0..MAX_PAGES {
        let soap_body = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:Search xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
<ContainerID>{container}</ContainerID>
<SearchCriteria>{criteria}</SearchCriteria>
<Filter>*</Filter>
<StartingIndex>{starting_index}</StartingIndex>
<RequestedCount>{PAGE_SIZE}</RequestedCount>
<SortCriteria></SortCriteria>
</u:Search>
</s:Body>
</s:Envelope>"#,
            criteria = xml_escape(&criteria),
        );

        let resp = match client
            .post(&ms.content_directory_url)
            .header("Content-Type", "text/xml; charset=utf-8")
            .header(
                "SOAPAction",
                "\"urn:schemas-upnp-org:service:ContentDirectory:1#Search\"",
            )
            .body(soap_body)
            .timeout(std::time::Duration::from_secs(10))
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    "search_media_server soap_error server={} start={starting_index} err={e}",
                    ms.name
                );
                cause = Some(EchecParcours::Transport(e.to_string()));
                break;
            }
        };

        let statut = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // Un 708 (« critère non supporté ») n'est pas une panne : c'est un
        // serveur qui annonce plus qu'il n'évalue. On le dit, plutôt que de
        // rendre une liste vide qui se lirait « aucun résultat ».
        if body.contains("<errorCode>") {
            let code = extract_xml_tag(&body, "errorCode").unwrap_or_default();
            tracing::info!(
                "search_media_server refus server={} code={code} criteria={criteria}",
                ms.name
            );
            return Ok(vide(false, "ce serveur a refusé le critère de recherche"));
        }
        if !statut.is_success() {
            cause = Some(EchecParcours::Statut(statut.as_u16()));
            break;
        }

        let (mut page_containers, mut page_items) = parse_didl_browse_response(&body);
        let parsed = (page_containers.len() + page_items.len()) as u32;
        let number_returned: u32 = extract_xml_tag(&body, "NumberReturned")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(parsed);
        if let Some(tm) = extract_xml_tag(&body, "TotalMatches").and_then(|s| s.trim().parse().ok())
        {
            total_matches = tm;
        }

        containers.append(&mut page_containers);
        items.append(&mut page_items);

        // #4895 — la boucle s'arrêtait EN SILENCE sur toute page vide : une
        // page vide AU MILIEU (« 8/13 ») servait 8 résultats comme s'ils
        // étaient tous, et une première page vide sur 12 annoncés se lisait
        // « aucun résultat ». Même règle que le Browse : on dit pourquoi.
        if parsed < number_returned {
            cause = Some(EchecParcours::DidlIllisible {
                lus: parsed,
                annonces: number_returned,
            });
            break;
        }
        if number_returned == 0 {
            if total_matches > starting_index {
                cause = Some(EchecParcours::PaginationInterrompue {
                    lus: starting_index,
                    total: total_matches,
                });
            } else {
                termine = true;
            }
            break;
        }
        starting_index += number_returned.max(parsed);
        if total_matches != 0 && starting_index >= total_matches {
            termine = true;
            break;
        }
    }

    let fetched = containers.len() + items.len();
    // Rien lu : l'échec est celui de la recherche entière — 502/504 typé, et
    // la ligne `search_media_server_failed`, plutôt qu'un « aucun résultat ».
    if fetched == 0
        && let Some(cause) = &cause
    {
        return Err(cause.en_erreur_http_de(&ms.name, "Search"));
    }
    let incomplet = match &cause {
        Some(cause) => Some(cause.statut_et_message(&ms.name, "Search").1),
        // `total_matches == 0` : le serveur n'annonce pas de total ; la
        // borne de pages atteinte n'y prouve rien de manquant.
        None if !termine && total_matches > starting_index => Some(format!(
            "{} : plafond de {MAX_PAGES} pages atteint au Search ({starting_index}/{total_matches})",
            ms.name
        )),
        None => None,
    };
    if let Some(raison) = &incomplet {
        warn!(
            serveur = %ms.name,
            container,
            lus = fetched,
            annonces = total_matches,
            error = %raison,
            "search_media_server_incomplete"
        );
    }
    Ok(Json(json!({
        "container": container,
        "query": q.q,
        "supported": true,
        "reason": "",
        "containers": containers,
        "items": items,
        "total_matches": (total_matches as usize).max(fetched),
        "number_returned": fetched,
        "complet": incomplet.is_none(),
        "incomplet": incomplet,
    })))
}

/// Ce que le serveur distant DIT savoir chercher.
///
/// Mis en cache dix minutes : une zone de recherche interroge à chaque frappe,
/// et cette capacité ne change pas d'une seconde à l'autre. Une panne réseau
/// n'est pas mise en cache — on réessaiera.
async fn capacites_de_recherche(content_directory_url: &str) -> String {
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    type Cache = std::sync::Mutex<std::collections::HashMap<String, (Instant, String)>>;
    static CACHE: OnceLock<Cache> = OnceLock::new();
    const TTL: Duration = Duration::from_secs(600);

    let cache = CACHE.get_or_init(Default::default);
    if let Ok(map) = cache.lock() {
        if let Some((pose, caps)) = map.get(content_directory_url) {
            if pose.elapsed() < TTL {
                return caps.clone();
            }
        }
    }

    let soap = r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body><u:GetSearchCapabilities xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"/></s:Body>
</s:Envelope>"#;
    let caps = match tune_core::http::client::shared()
        .post(content_directory_url)
        .header("Content-Type", "text/xml; charset=utf-8")
        .header(
            "SOAPAction",
            "\"urn:schemas-upnp-org:service:ContentDirectory:1#GetSearchCapabilities\"",
        )
        .body(soap)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
    {
        Ok(r) => {
            extract_xml_tag(&r.text().await.unwrap_or_default(), "SearchCaps").unwrap_or_default()
        }
        Err(e) => {
            tracing::debug!("get_search_capabilities err={e}");
            return String::new();
        }
    };

    if let Ok(mut map) = cache.lock() {
        map.insert(
            content_directory_url.to_string(),
            (Instant::now(), caps.clone()),
        );
    }
    caps
}

/// Le critère à envoyer, construit UNIQUEMENT avec les champs annoncés.
///
/// `*` est la façon dont beaucoup de serveurs disent « tout m'est
/// interrogeable ». Sans `dc:title` — ni `*` —, on rend `None` : mieux vaut
/// dire au client qu'on ne sait pas chercher que lui rendre la bibliothèque
/// entière sous le nom de « résultats ».
///
/// La restriction de classe n'est ajoutée que si `upnp:class` est annoncé :
/// c'est un champ de plus à évaluer, et un serveur qui ne le connaît pas
/// refuserait tout le critère.
fn critere_de_recherche(caps: &str, texte: &str) -> Option<String> {
    let annonce = |champ: &str| {
        caps.split(',')
            .any(|c| c.trim() == "*" || c.trim().eq_ignore_ascii_case(champ))
    };
    if !annonce("dc:title") {
        return None;
    }
    let valeur = echapper_valeur_critere(texte);
    let titre = format!("dc:title contains \"{valeur}\"");
    Some(if annonce("upnp:class") {
        format!("upnp:class derivedfrom \"object.item.audioItem\" and {titre}")
    } else {
        titre
    })
}

/// Dans un `SearchCriteria`, une valeur est entre guillemets : la barre
/// oblique inverse et le guillemet doivent y être échappés, sinon un titre
/// contenant `"` casse le critère — ou, pire, en injecte un autre.
fn echapper_valeur_critere(v: &str) -> String {
    v.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Le critère voyage dans du XML : `&`, `<` et les guillemets doivent y être
/// écrits en entités, sinon le SOAP est invalide.
fn xml_escape(v: &str) -> String {
    v.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Position de la prochaine balise ouvrante `<nom>` ou `<nom …>` dans `doc`,
/// à partir de l'octet `depuis`.
///
/// #4895 — l'analyseur cherchait `<item ` et `<container ` avec l'ESPACE : un
/// serveur qui écrit `<item\n id="…">` ou `<container\tid="…">` — du XML
/// parfaitement valide, le blanc après le nom de balise peut être n'importe
/// quel blanc XML — voyait tout son DIDL lu comme vide. Ici, après le nom,
/// seul compte un blanc XML (espace, tabulation, CR, LF) — ou `>` quand
/// `nue_admise` : `<Result>`, `<res>`. `<itemfoo>` ou `<item:x>` ne sont PAS des
/// `<item>` ; un `<item>` ou `<container>` NU non plus : DIDL-Lite leur impose
/// `id`, `parentID` et `restricted`, et #4914 garde qu'un tel DIDL est dit
/// illisible plutôt que lu avec des identifiants vides.
fn balise_ouvrante(doc: &str, depuis: usize, nom: &str, nue_admise: bool) -> Option<usize> {
    let motif = format!("<{nom}");
    let mut pos = depuis;
    while let Some(rel) = doc.get(pos..)?.find(&motif) {
        let debut = pos + rel;
        let suite = debut + motif.len();
        match doc.as_bytes().get(suite) {
            Some(b' ' | b'\t' | b'\n' | b'\r') => return Some(debut),
            Some(b'>') if nue_admise => return Some(debut),
            _ => pos = suite,
        }
    }
    None
}

fn parse_didl_browse_response(xml: &str) -> (Vec<Value>, Vec<Value>) {
    let result_start = balise_ouvrante(xml, 0, "Result", true);
    let result_end = xml.find("</Result>");
    let didl = match (result_start, result_end) {
        (Some(s), Some(e)) => {
            let after = &xml[s..];
            let content_start = after.find('>').map(|i| s + i + 1).unwrap_or(s);
            &xml[content_start..e]
        }
        _ => return (vec![], vec![]),
    };
    let decoded = didl
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&apos;", "'");

    let mut containers = Vec::new();
    let mut items = Vec::new();

    for tag in ["container", "item"] {
        let close = format!("</{tag}>");
        let mut pos = 0;
        // #4895 : tout blanc XML après le nom de balise, pas la seule espace.
        while let Some(abs_start) = balise_ouvrante(&decoded, pos, tag, false) {
            if let Some(end) = decoded[abs_start..].find(&close) {
                let element = &decoded[abs_start..abs_start + end + close.len()];
                let id = extract_attr(element, "id").unwrap_or_default();
                let parent_id = extract_attr(element, "parentID").unwrap_or_default();
                let title = extract_xml_tag(element, "dc:title").unwrap_or_default();
                let album_art_uri = extract_xml_tag(element, "upnp:albumArtURI");
                let artist = extract_xml_tag(element, "upnp:artist")
                    .or_else(|| extract_xml_tag(element, "dc:creator"));

                if tag == "container" {
                    let child_count: u32 = extract_attr(element, "childCount")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    containers.push(json!({
                        "id": id,
                        "parent_id": parent_id,
                        "title": title,
                        // Le serveur envoie dc:creator sur les conteneurs album
                        // depuis toujours — c'est ICI qu'il se perdait : extrait
                        // quatre lignes plus haut, jamais posé dans le JSON.
                        // Une grille d'albums sans artiste n'est pas une
                        // bibliothèque (jeu des sept erreurs, 25/08).
                        "artist": artist,
                        "child_count": child_count,
                        "album_art_uri": album_art_uri,
                    }));
                } else {
                    let album = extract_xml_tag(element, "upnp:album");
                    // A server may announce SEVERAL <res> per item — Lyrion/LMS
                    // lists the original file (download.flc, with duration) plus
                    // on-the-fly transcodes (download.pcm headerless raw PCM with
                    // duration 0:00, download.mp3, …). The old code took the FIRST
                    // <res> blindly, so whenever the raw-PCM transcode came first
                    // the DLNA renderer was handed an unplayable headerless stream
                    // (Yacine: immediate failure, 0:00, replay loop). Pick the best
                    // resource instead; single-res items are untouched.
                    let resources = parse_res_elements(element);
                    let best = select_best_res(&resources);
                    let res_url = best.map(|r| r.url.clone());
                    // Real resolution + codec from the CHOSEN res@ attributes.
                    // Without these the signal path defaulted to "AAC 44kHz/16bit —
                    // Avec perte", mislabelling a hi-res ALAC (audio/mp4) as lossy AAC
                    // (Yves: NAS ALAC shown as AAC while the DartZeel read 24-bit).
                    let duration_ms = best.and_then(|r| r.duration_ms);
                    let sample_rate = best.and_then(|r| r.sample_rate);
                    let bit_depth = best.and_then(|r| r.bit_depth);
                    let channels = best.and_then(|r| r.channels);
                    let protocol_info = best.and_then(|r| r.protocol_info.clone());
                    // `res@size` : jamais rendu jusqu'ici. L'indexation de la
                    // phase 2 en fait une composante de la clé d'identité.
                    let size = best.and_then(|r| r.size);
                    // Numéros de piste et de disque : `upnp:originalTrackNumber`
                    // est la balise normalisée ; `upnp:originalDiscNumber` est
                    // celle des serveurs qui disent le disque. Jamais lus
                    // jusqu'ici : l'import de la bibliothèque unifiée rangeait
                    // 0 sur chaque piste, et la fiche d'album se triait par
                    // titre. Absent, illisible ou 0 : `null`, rien d'inventé.
                    let track_number = extract_xml_tag(element, "upnp:originalTrackNumber")
                        .and_then(|v| numero_didl(&v));
                    let disc_number = extract_xml_tag(element, "upnp:originalDiscNumber")
                        .and_then(|v| numero_didl(&v));
                    items.push(json!({
                        "id": id,
                        "title": title,
                        "artist": artist,
                        "album": album,
                        "res_url": res_url,
                        "album_art_uri": album_art_uri,
                        "duration_ms": duration_ms,
                        "sample_rate": sample_rate,
                        "bit_depth": bit_depth,
                        "channels": channels,
                        "protocol_info": protocol_info,
                        "size": size,
                        "track_number": track_number,
                        "disc_number": disc_number,
                    }));
                }

                pos = abs_start + end + close.len();
            } else {
                break;
            }
        }
    }
    (containers, items)
}

/// One `<res>` element of a DIDL-Lite item.
#[derive(Debug, Clone)]
struct DidlRes {
    url: String,
    protocol_info: Option<String>,
    duration_ms: Option<u64>,
    sample_rate: Option<u32>,
    bit_depth: Option<u16>,
    channels: Option<u16>,
    /// `res@size` — la taille du fichier amont, en octets.
    ///
    /// Le DIDL la publie depuis toujours (mesuré le 14/09 sur Asset UPnP comme
    /// sur un Tune : `size="31911291"`), et le parseur la jetait. Elle entre
    /// ici parce qu'elle est le seul discriminant **stable** qui reste quand
    /// l'`ObjectID` ne l'est pas — voir `cle_d_identite` dans
    /// `routes/indexation_upnp.rs`.
    size: Option<u64>,
}

/// Parse every `<res …>url</res>` of a DIDL item, in document order.
fn parse_res_elements(element: &str) -> Vec<DidlRes> {
    let mut out = Vec::new();
    let mut pos = 0;
    // Only match the actual <res> tag, not e.g. <resType> — and, #4895, with
    // ANY XML whitespace after the name (`<res\n protocolInfo=…>`).
    while let Some(abs) = balise_ouvrante(element, pos, "res", true) {
        let Some(tag_end_rel) = element[abs..].find('>') else {
            break;
        };
        let tag_end = abs + tag_end_rel;
        let res_tag = &element[abs..tag_end];
        let Some(close_rel) = element[tag_end..].find("</res>") else {
            break;
        };
        let url = texte_didl(element[tag_end + 1..tag_end + close_rel].trim());
        if !url.is_empty() {
            out.push(DidlRes {
                url,
                protocol_info: extract_attr(res_tag, "protocolInfo"),
                duration_ms: extract_attr(res_tag, "duration")
                    .and_then(|d| parse_upnp_duration(&d)),
                sample_rate: extract_attr(res_tag, "sampleFrequency")
                    .and_then(|s| s.parse::<u32>().ok()),
                bit_depth: extract_attr(res_tag, "bitsPerSample")
                    .and_then(|s| s.parse::<u16>().ok()),
                channels: extract_attr(res_tag, "nrAudioChannels")
                    .and_then(|s| s.parse::<u16>().ok()),
                size: extract_attr(res_tag, "size").and_then(|s| s.parse::<u64>().ok()),
            });
        }
        pos = tag_end + close_rel + "</res>".len();
    }
    out
}

/// Format-preference rank for a `<res>` — lower is better.
///
/// 0 = original/lossless WITH headers (flac/flc, alac/m4a, wav, aiff)
/// 1 = encapsulated lossy (mp3, aac, ogg/opus, wma)
/// 2 = unknown audio format
/// 3 = raw headerless PCM (audio/L16, audio/L24, LPCM, .pcm) — LMS announces
///     these transcodes with duration 0:00 and DLNA renderers choke on them
/// 4 = non-audio res (cover images some servers attach as extra <res>)
fn res_format_rank(res: &DidlRes) -> u8 {
    let path = res
        .url
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .to_lowercase();
    // protocolInfo = "http-get:*:<mime>:<extra>"
    let mime = res
        .protocol_info
        .as_deref()
        .and_then(|p| p.split(':').nth(2))
        .unwrap_or("")
        .trim()
        .to_lowercase();

    if mime.starts_with("image/") || mime.starts_with("video/") {
        return 4;
    }
    if mime.starts_with("audio/l16")
        || mime.starts_with("audio/l24")
        || mime.contains("lpcm")
        || path.ends_with(".pcm")
    {
        return 3;
    }
    const LOSSLESS_EXT: [&str; 8] = [
        ".flac", ".flc", ".m4a", ".mp4", ".alac", ".wav", ".aif", ".aiff",
    ];
    const LOSSLESS_MIME: [&str; 11] = [
        "audio/flac",
        "audio/x-flac",
        "audio/mp4",
        "audio/m4a",
        "audio/x-m4a",
        "audio/wav",
        "audio/x-wav",
        "audio/wave",
        "audio/aiff",
        "audio/x-aiff",
        "audio/x-aif",
    ];
    if LOSSLESS_EXT.iter().any(|e| path.ends_with(e)) || LOSSLESS_MIME.contains(&mime.as_str()) {
        return 0;
    }
    const LOSSY_EXT: [&str; 6] = [".mp3", ".aac", ".ogg", ".oga", ".opus", ".wma"];
    const LOSSY_MIME: [&str; 9] = [
        "audio/mpeg",
        "audio/mp3",
        "audio/aac",
        "audio/x-aac",
        "audio/ogg",
        "audio/x-ogg",
        "application/ogg",
        "audio/opus",
        "audio/x-ms-wma",
    ];
    if LOSSY_EXT.iter().any(|e| path.ends_with(e)) || LOSSY_MIME.contains(&mime.as_str()) {
        return 1;
    }
    2
}

/// Pick the best `<res>` of an item: by format rank, then prefer a resource
/// with a non-zero `duration` attribute, then keep document order. Items that
/// announce a single res keep it unconditionally (behaviour unchanged for
/// servers that only expose one resource, even raw PCM).
fn select_best_res(resources: &[DidlRes]) -> Option<&DidlRes> {
    if resources.len() <= 1 {
        return resources.first();
    }
    resources
        .iter()
        .enumerate()
        .min_by_key(|(i, r)| {
            (
                res_format_rank(r),
                u8::from(r.duration_ms.unwrap_or(0) == 0),
                *i,
            )
        })
        .map(|(_, r)| r)
}

fn parse_upnp_duration(d: &str) -> Option<u64> {
    let parts: Vec<&str> = d.split(':').collect();
    if parts.len() == 3 {
        let h: f64 = parts[0].parse().ok()?;
        let m: f64 = parts[1].parse().ok()?;
        let s: f64 = parts[2].parse().ok()?;
        Some((h * 3_600_000.0 + m * 60_000.0 + s * 1_000.0) as u64)
    } else if parts.len() == 2 {
        let m: f64 = parts[0].parse().ok()?;
        let s: f64 = parts[1].parse().ok()?;
        Some((m * 60_000.0 + s * 1_000.0) as u64)
    } else {
        None
    }
}

fn extract_attr(element: &str, name: &str) -> Option<String> {
    let pattern = format!("{name}=\"");
    let start = element.find(&pattern)? + pattern.len();
    let end = element[start..].find('"')? + start;
    Some(element[start..end].to_string())
}

/// Le texte d'un nœud DIDL, ses entités rendues à leur caractère.
///
/// 🔴 Le DIDL arrive ÉCHAPPÉ DEUX FOIS, et n'était désechappé qu'une.
/// `parse_didl_browse_response` désechappe le contenu de `<Result>` pour
/// obtenir le document DIDL ; dans CE document, les textes portent encore
/// leur propre échappement. « Polo &amp;amp; Pan » devenait donc
/// « Polo &amp;amp; Pan » → « Polo &amp; Pan », affiché tel quel sous la
/// pochette (Bertrand, .18, 17/09/2026, capture à l'appui).
///
/// Une URL `<res>` souffre du même mal : `…?id=1&amp;type=flac` désigne une
/// autre ressource que `…?id=1&type=flac` — celle-là, le serveur ne la
/// connaît pas.
///
/// `unescape` échoue sur une entité inconnue (`&toto;`) : on rend alors le
/// texte tel quel, plutôt que rien.
fn texte_didl(brut: &str) -> String {
    quick_xml::escape::unescape(brut)
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| brut.to_string())
}

/// Un numéro de piste ou de disque DIDL : les chiffres de tête, strictement
/// positifs. « 3/12 » donne 3 ; « 0 », « » ou « A1 » ne donnent rien — 0 n'est
/// pas un numéro, c'est l'absence de numéro.
fn numero_didl(brut: &str) -> Option<u32> {
    let chiffres: String = brut
        .trim()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    chiffres.parse::<u32>().ok().filter(|n| *n > 0)
}

fn extract_xml_tag(element: &str, tag: &str) -> Option<String> {
    let open_full = format!("<{tag}>");
    let close = format!("</{tag}>");
    // #4895 : `<dc:title\n xml:lang=…>` est une balise avec attributs, au même
    // titre que `<dc:title xml:lang=…>`.
    let content_start = if let Some(s) = element.find(&open_full) {
        s + open_full.len()
    } else if let Some(s) = balise_ouvrante(element, 0, tag, true) {
        let after = &element[s..];
        after.find('>')? + s + 1
    } else {
        return None;
    };
    let content_end = element[content_start..].find(&close)? + content_start;
    Some(texte_didl(&element[content_start..content_end]))
}

#[derive(Deserialize)]
struct BrowseQuery {
    object_id: Option<String>,
}

async fn media_server_stream_url(Path((id, item_id)): Path<(String, String)>) -> Json<Value> {
    Json(json!({
        "server_id": id,
        "item_id": item_id,
        "stream_url": null,
        "message": "UPnP stream URL resolution not yet implemented",
    }))
}

async fn play_media_server_item(
    Path((id, item_id, zone_id)): Path<(String, String, i64)>,
) -> Json<Value> {
    Json(json!({
        "server_id": id,
        "item_id": item_id,
        "zone_id": zone_id,
        "status": "not_implemented",
        "message": "UPnP media server playback not yet implemented",
    }))
}

#[derive(Deserialize)]
struct TestMountRequest {
    path: String,
}

async fn test_mount(Json(body): Json<TestMountRequest>) -> impl IntoResponse {
    let path = std::path::Path::new(&body.path);
    let exists = path.exists();
    let is_dir = path.is_dir();
    let readable = if exists {
        std::fs::read_dir(path).is_ok()
    } else {
        false
    };
    let file_count = if readable {
        std::fs::read_dir(path).map(|rd| rd.count()).unwrap_or(0)
    } else {
        0
    };

    Json(json!({
        "path": body.path,
        "exists": exists,
        "is_directory": is_dir,
        "readable": readable,
        "file_count": file_count,
    }))
}

async fn get_share_detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<impl IntoResponse, AppError> {
    use tune_core::db::backend::ToSqlValue;
    let p1 = if state.backend.engine() == tune_core::db::engine::Engine::Postgres {
        "$1".to_string()
    } else {
        "?".to_string()
    };
    let result = state.backend.query_one(
        &format!(
            "SELECT id, mount_type, server, share, mount_path, username, active \
             FROM network_mounts WHERE id = {p1}"
        ),
        &[&id as &dyn ToSqlValue],
    );
    match result {
        Ok(Some(r)) => Ok(Json(json!({
            "id": r.get(0).and_then(|v| v.as_i64()),
            "mount_type": r.get(1).and_then(|v| v.as_string()),
            "server": r.get(2).and_then(|v| v.as_string()),
            "share": r.get(3).and_then(|v| v.as_string()),
            "mount_path": r.get(4).and_then(|v| v.as_string()),
            "username": r.get(5).and_then(|v| v.as_string()),
            "active": r.get(6).and_then(|v| v.as_i64()).unwrap_or(1) != 0,
        }))
        .into_response()),
        Ok(None) => Ok(StatusCode::NOT_FOUND.into_response()),
        Err(_) => Ok(StatusCode::NOT_FOUND.into_response()),
    }
}

#[cfg(test)]
mod tests {

    /// La regle du chantier : ne demander QUE ce que le serveur annonce.
    ///
    /// Un serveur qui n'annonce pas `dc:title` ne doit pas recevoir de critere
    /// de titre. Beaucoup repondent alors par toute la bibliotheque, ce qui
    /// ressemble a un resultat et n'en est pas.
    #[test]
    fn on_ne_demande_que_ce_que_le_serveur_annonce() {
        assert_eq!(critere_de_recherche("upnp:class", "blue"), None);
        assert_eq!(critere_de_recherche("", "blue"), None);
        assert_eq!(
            critere_de_recherche("upnp:class,dc:title", "blue").as_deref(),
            Some("upnp:class derivedfrom \"object.item.audioItem\" and dc:title contains \"blue\"")
        );
        // Sans `upnp:class` annonce, la restriction de classe est retiree :
        // l'ajouter ferait refuser tout le critere.
        assert_eq!(
            critere_de_recherche("dc:title", "blue").as_deref(),
            Some("dc:title contains \"blue\"")
        );
        // `*` est la facon dont beaucoup de serveurs disent « tout ».
        assert!(critere_de_recherche("*", "blue").is_some());
        // La casse annoncee varie d'un serveur a l'autre.
        assert!(critere_de_recherche("DC:TITLE", "blue").is_some());
    }

    /// Un titre contenant un guillemet ne doit pas pouvoir fermer la valeur du
    /// critere — ni casser le SOAP, ni y injecter un predicat.
    #[test]
    fn un_guillemet_dans_le_texte_cherche_est_echappe() {
        let c = critere_de_recherche("dc:title", r#"say "hello""#).unwrap();
        assert!(c.contains(r#"\"hello\""#), "{c}");
        assert_eq!(echapper_valeur_critere(r#"a\b"c"#), r#"a\\b\"c"#);
        assert_eq!(xml_escape(r#"a&b<c>"d""#), "a&amp;b&lt;c&gt;&quot;d&quot;");
    }

    use super::{
        critere_de_recherche, echapper_valeur_critere, obstacle_de_montage,
        parse_didl_browse_response, parse_res_elements, select_best_res, xml_escape,
    };

    /// Build a SOAP Browse response whose escaped DIDL contains one item with
    /// the given raw `<res>` elements (LMS-style).
    fn soap_with_res(res_elements: &str) -> String {
        let didl = format!(
            r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="t1" parentID="a1" restricted="1"><dc:title>Track</dc:title><upnp:artist>Artist</upnp:artist><upnp:album>Album</upnp:album>{res_elements}</item></DIDL-Lite>"#
        );
        let escaped = didl
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
            .replace('"', "&quot;");
        format!(
            "<s:Envelope><s:Body><u:BrowseResponse><Result>{escaped}</Result><NumberReturned>1</NumberReturned><TotalMatches>1</TotalMatches></u:BrowseResponse></s:Body></s:Envelope>"
        )
    }

    // Yacine's case: LMS announces the headerless raw-PCM transcode FIRST
    // (duration 0:00), then the original FLAC (with duration), then an MP3
    // transcode. The FLAC must win.
    const LMS_MULTI_RES: &str = concat!(
        r#"<res protocolInfo="http-get:*:audio/L16;rate=44100;channels=2:DLNA.ORG_PN=LPCM" duration="0:00:00">http://192.168.1.7:9000/music/123/download.pcm</res>"#,
        r#"<res protocolInfo="http-get:*:audio/x-flac:*" duration="0:04:33.000" sampleFrequency="44100" bitsPerSample="16" nrAudioChannels="2">http://192.168.1.7:9000/music/123/download.flc</res>"#,
        r#"<res protocolInfo="http-get:*:audio/mpeg:*" duration="0:04:33.000">http://192.168.1.7:9000/music/123/download.mp3</res>"#,
    );

    #[test]
    fn multi_res_lms_prefers_flac_over_raw_pcm() {
        let resources = parse_res_elements(LMS_MULTI_RES);
        assert_eq!(resources.len(), 3);
        let best = select_best_res(&resources).expect("a res must be selected");
        assert!(
            best.url.ends_with("download.flc"),
            "expected the FLAC original, got {}",
            best.url
        );
        assert_eq!(best.duration_ms, Some(273_000));
        assert_eq!(best.sample_rate, Some(44100));
        assert_eq!(best.bit_depth, Some(16));
        assert_eq!(
            best.protocol_info.as_deref(),
            Some("http-get:*:audio/x-flac:*")
        );
    }

    /// Jeu des sept erreurs (25/08) : le serveur envoie dc:creator sur les
    /// conteneurs album depuis toujours — extrait par le parseur, jamais posé
    /// dans le JSON. Une grille d'albums sans artiste n'est pas une
    /// bibliothèque. Contre-épreuve faite : fix neutralisé → FAILED.
    #[test]
    fn le_createur_d_un_conteneur_atterrit_dans_le_json() {
        let soap = format!(
            "<Envelope><Body><BrowseResponse><Result>{}</Result></BrowseResponse></Body></Envelope>",
            xml_escape(
                r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><container id="album/18" parentID="albums" restricted="1" childCount="10"><dc:title>18</dc:title><dc:creator>Moby</dc:creator><upnp:class>object.container.album.musicAlbum</upnp:class></container></DIDL-Lite>"#
            )
        );
        let (containers, items) = parse_didl_browse_response(&soap);
        assert!(items.is_empty());
        assert_eq!(containers.len(), 1);
        assert_eq!(containers[0]["title"].as_str(), Some("18"));
        assert_eq!(
            containers[0]["artist"].as_str(),
            Some("Moby"),
            "dc:creator doit survivre jusqu'au JSON du conteneur"
        );
    }

    /// 🔴 Le DIDL est échappé DEUX fois, et n'était désechappé qu'une.
    ///
    /// Bertrand, .18, 17/09/2026, capture à l'appui : « Polo &amp; Pan »
    /// s'affichait sous la pochette de *Canopée*, artiste et barre de lecture
    /// comprises. Le contenu de `<Result>` est désechappé pour obtenir le
    /// document DIDL ; les TEXTES de ce document portent encore le leur.
    ///
    /// L'URL `<res>` souffrait du même mal, et c'est plus grave qu'un
    /// affichage : `…?id=1&amp;amp;type=flac` désigne une ressource que le
    /// serveur ne connaît pas.
    #[test]
    fn les_entites_du_didl_sont_rendues_a_leur_caractere() {
        let soap = format!(
            "<Envelope><Body><BrowseResponse><Result>{}</Result></BrowseResponse></Body></Envelope>",
            xml_escape(
                r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="42" parentID="7" restricted="1"><dc:title>Canop&#233;e</dc:title><upnp:artist>Polo &amp; Pan</upnp:artist><upnp:album>Caravelle</upnp:album><res protocolInfo="http-get:*:audio/flac:*" duration="0:04:36">http://192.168.1.19:9000/stream?id=1&amp;type=flac</res></item></DIDL-Lite>"#
            )
        );
        let (_containers, items) = parse_didl_browse_response(&soap);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0]["artist"].as_str(),
            Some("Polo & Pan"),
            "l'esperluette doit redevenir une esperluette — sans le correctif : « Polo &amp; Pan »"
        );
        assert_eq!(items[0]["title"].as_str(), Some("Canopée"));
        assert_eq!(
            items[0]["res_url"].as_str(),
            Some("http://192.168.1.19:9000/stream?id=1&type=flac"),
            "une URL mal désechappée désigne une ressource qui n'existe pas"
        );
        // Témoin : un texte sans entité traverse inchangé.
        assert_eq!(items[0]["album"].as_str(), Some("Caravelle"));
    }

    /// b209 — le numéro de piste d'un serveur UPnP. Le DIDL ci-dessous a la
    /// forme exacte de celui qu'un serveur Tune rend à `Browse` sur un album
    /// (relevé sur le LAN le 07/10/2026, adresses remplacées) : le numéro est
    /// dans `upnp:originalTrackNumber`, et rien ne le lisait — l'import
    /// rangeait 0 sur chaque piste.
    #[test]
    fn le_numero_de_piste_et_de_disque_sont_lus_et_zero_n_en_est_pas_un() {
        let item = |id: &str, numeros: &str| {
            format!(
                r#"<item id="track/{id}" parentID="album/1" restricted="1"><dc:title>T{id}</dc:title><dc:creator>Artiste</dc:creator><upnp:artist>Artiste</upnp:artist><upnp:class>object.item.audioItem.musicTrack</upnp:class><upnp:album>Album</upnp:album>{numeros}<res protocolInfo="http-get:*:audio/flac:*" duration="0:06:38.493" sampleFrequency="44100" bitsPerSample="16" nrAudioChannels="2" size="31909580">http://serveur.invalid/api/v1/library/tracks/{id}/audio</res></item>"#
            )
        };
        let didl = format!(
            r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">{}{}{}{}</DIDL-Lite>"#,
            item(
                "1",
                "<upnp:originalTrackNumber>7</upnp:originalTrackNumber><upnp:originalDiscNumber>2</upnp:originalDiscNumber>"
            ),
            item(
                "2",
                "<upnp:originalTrackNumber>0</upnp:originalTrackNumber>"
            ),
            item("3", ""),
            item(
                "4",
                "<upnp:originalTrackNumber> 3/12 </upnp:originalTrackNumber>"
            ),
        );
        let soap = format!(
            "<Envelope><Body><BrowseResponse><Result>{}</Result></BrowseResponse></Body></Envelope>",
            xml_escape(&didl)
        );
        let (_c, items) = parse_didl_browse_response(&soap);
        assert_eq!(items.len(), 4);
        assert_eq!(
            items[0]["track_number"].as_u64(),
            Some(7),
            "sans le correctif : absent"
        );
        assert_eq!(items[0]["disc_number"].as_u64(), Some(2));
        assert!(items[1]["track_number"].is_null(), "0 n'est pas un numéro");
        assert!(items[1]["disc_number"].is_null());
        assert!(items[2]["track_number"].is_null(), "rien n'est inventé");
        assert_eq!(items[3]["track_number"].as_u64(), Some(3));
    }

    #[test]
    fn multi_res_end_to_end_didl_parse_picks_flac() {
        let soap = soap_with_res(LMS_MULTI_RES);
        let (containers, items) = parse_didl_browse_response(&soap);
        assert!(containers.is_empty());
        assert_eq!(items.len(), 1);
        let item = &items[0];
        assert_eq!(
            item["res_url"].as_str().unwrap(),
            "http://192.168.1.7:9000/music/123/download.flc"
        );
        // duration/resolution must come from the CHOSEN res, not the first one
        assert_eq!(item["duration_ms"].as_u64(), Some(273_000));
        assert_eq!(item["sample_rate"].as_u64(), Some(44100));
        assert_eq!(
            item["protocol_info"].as_str(),
            Some("http-get:*:audio/x-flac:*")
        );
    }

    #[test]
    fn single_res_pcm_is_kept() {
        // A server that only announces raw PCM must keep working as before.
        let element = r#"<res protocolInfo="http-get:*:audio/L16;rate=44100;channels=2:*">http://10.0.0.2:9000/music/9/download.pcm</res>"#;
        let resources = parse_res_elements(element);
        assert_eq!(resources.len(), 1);
        let best = select_best_res(&resources).unwrap();
        assert!(best.url.ends_with("download.pcm"));
    }

    #[test]
    fn only_lossy_res_picks_mp3() {
        let element = concat!(
            r#"<res protocolInfo="http-get:*:audio/L16:*" duration="0:00:00">http://h:9000/music/5/download.pcm</res>"#,
            r#"<res protocolInfo="http-get:*:audio/mpeg:*" duration="0:03:10.000">http://h:9000/music/5/download.mp3</res>"#,
        );
        let resources = parse_res_elements(element);
        assert_eq!(resources.len(), 2);
        let best = select_best_res(&resources).unwrap();
        assert!(
            best.url.ends_with("download.mp3"),
            "mp3 must beat raw pcm, got {}",
            best.url
        );
        assert_eq!(best.duration_ms, Some(190_000));
    }

    #[test]
    fn equal_format_prefers_res_with_duration() {
        // Same rank (both FLAC): the one with a real duration wins even if
        // listed second.
        let element = concat!(
            r#"<res protocolInfo="http-get:*:audio/flac:*">http://h/1/nodur.flac</res>"#,
            r#"<res protocolInfo="http-get:*:audio/flac:*" duration="0:04:00.000">http://h/1/dur.flac</res>"#,
        );
        let resources = parse_res_elements(element);
        let best = select_best_res(&resources).unwrap();
        assert!(best.url.ends_with("dur.flac"));
    }

    #[test]
    fn equal_format_and_duration_keeps_document_order() {
        let element = concat!(
            r#"<res protocolInfo="http-get:*:audio/flac:*" duration="0:04:00.000">http://h/1/first.flac</res>"#,
            r#"<res protocolInfo="http-get:*:audio/flac:*" duration="0:04:00.000">http://h/1/second.flac</res>"#,
        );
        let resources = parse_res_elements(element);
        let best = select_best_res(&resources).unwrap();
        assert!(best.url.ends_with("first.flac"));
    }

    #[test]
    fn image_res_never_beats_audio() {
        // Some servers attach the cover as an extra <res>.
        let element = concat!(
            r#"<res protocolInfo="http-get:*:image/jpeg:*">http://h/cover.jpg</res>"#,
            r#"<res protocolInfo="http-get:*:audio/mpeg:*" duration="0:03:00.000">http://h/track.mp3</res>"#,
        );
        let resources = parse_res_elements(element);
        let best = select_best_res(&resources).unwrap();
        assert!(best.url.ends_with("track.mp3"));
    }

    // --- Nommer l'obstacle, au lieu de rendre un errno (#1515 voisin) ---

    fn err(kind: std::io::ErrorKind) -> std::io::Error {
        std::io::Error::new(kind, "essai")
    }

    /// Le cas de Dominique Comet : serveur lance depuis son repertoire
    /// personnel, /mnt appartient a root. Le message rendu etait « failed to
    /// create mount dir: Permission denied (os error 13) » — exact, et
    /// inutile : il ne dit pas ce qui manque, et surtout pas que le MONTAGE
    /// redemandera le meme privilege juste apres.
    #[test]
    fn un_refus_de_privilege_est_nomme_comme_tel() {
        let (motif, msg) = obstacle_de_montage(
            &err(std::io::ErrorKind::PermissionDenied),
            "/mnt/192.168.1.146_Music",
        );
        assert_eq!(motif, "privileges_insuffisants");
        assert!(msg.contains("/mnt/192.168.1.146_Music"), "{msg}");
        // Le point qui a coute une soiree a Dominique : creer le dossier a la
        // main ne suffit pas. Le message doit le dire, sinon il essaiera.
        assert!(
            msg.contains("ne suffira pas"),
            "le message doit prevenir que creer le dossier ne debloque rien : {msg}"
        );
        assert!(
            msg.contains("CAP_SYS_ADMIN") || msg.contains("root"),
            "{msg}"
        );
        // Et il doit offrir la sortie, pas seulement le constat.
        assert!(
            msg.contains("fstab"),
            "la solution non privilegiee manque : {msg}"
        );
    }

    #[test]
    fn les_autres_echecs_gardent_leur_cause_exacte() {
        // On ne noie pas tout dans « privileges » : un parent absent est un
        // probleme different, avec une reparation differente.
        let (motif, msg) = obstacle_de_montage(&err(std::io::ErrorKind::NotFound), "/x/y");
        assert_eq!(motif, "chemin_parent_absent");
        assert!(msg.contains("/x/y"), "{msg}");
        assert!(
            !msg.contains("CAP_SYS_ADMIN"),
            "pas de conseil hors sujet : {msg}"
        );

        let (motif, msg) = obstacle_de_montage(&err(std::io::ErrorKind::AlreadyExists), "/x/y");
        assert_eq!(motif, "creation_impossible");
        // Le cas inconnu garde l'erreur systeme : mieux vaut un message brut
        // qu'un message faux.
        assert!(
            msg.contains("essai"),
            "l'erreur d'origine doit survivre : {msg}"
        );
    }

    #[test]
    fn chaque_motif_est_distinct() {
        // Trois motifs, trois codes : le client peut les traduire, et un
        // journal les distingue. Les confondre ramenerait au message unique.
        let m: Vec<&str> = [
            std::io::ErrorKind::PermissionDenied,
            std::io::ErrorKind::NotFound,
            std::io::ErrorKind::AlreadyExists,
        ]
        .iter()
        .map(|k| obstacle_de_montage(&err(*k), "/x").0)
        .collect();
        let uniques: std::collections::HashSet<&&str> = m.iter().collect();
        assert_eq!(uniques.len(), 3, "{m:?}");
    }
}

/// Une réponse de `Browse` porte un élément `<Result>` — vide pour un dossier
/// vide, mais présent. Un SOAP Fault n'en a pas.
fn reponse_porte_un_result(xml: &str) -> bool {
    xml.contains("<Result>") || xml.contains("<Result ")
}

/// #4134 — un `Browse` qui échoue ne rend plus un dossier vide muet : il
/// dit sa cause, et la route la traduit en 404 / 502 / 504.
#[cfg(test)]
mod tests_browse_dit_son_echec_4134 {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Un faux ContentDirectory qui rend toujours la même réponse HTTP.
    async fn faux_serveur(statut: &'static str, corps: &'static str) -> String {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = l.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = [0u8; 8192];
                    let _ = sock.read(&mut buf).await;
                    let reponse = format!(
                        "HTTP/1.1 {statut}\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{corps}",
                        corps.len()
                    );
                    let _ = sock.write_all(reponse.as_bytes()).await;
                });
            }
        });
        format!("http://127.0.0.1:{port}/cd/control")
    }

    const BROWSE_VIDE: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1"><Result>&lt;DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"/&gt;</Result><NumberReturned>0</NumberReturned><TotalMatches>0</TotalMatches><UpdateID>1</UpdateID></u:BrowseResponse></s:Body></s:Envelope>"#;
    const SOAP_FAULT: &str = r#"<?xml version="1.0"?><s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>701</errorCode><errorDescription>No such object</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>"#;

    #[tokio::test]
    async fn un_dossier_reellement_vide_reste_un_succes() {
        let url = faux_serveur("200 OK", BROWSE_VIDE).await;
        let (c, i, total, incomplet) = parcourir_les_enfants(&url, "faux", "0")
            .await
            .expect("vide ≠ échec");
        assert!(c.is_empty() && i.is_empty());
        assert_eq!(total, 0);
        assert_eq!(incomplet, None, "un dossier vide est complet");
    }

    #[tokio::test]
    async fn un_500_est_un_echec_nomme_pas_un_dossier_vide() {
        let url = faux_serveur("500 Internal Server Error", "<html>boom</html>").await;
        let err = parcourir_les_enfants(&url, "faux", "0")
            .await
            .expect_err("500 doit échouer");
        assert_eq!(err, EchecParcours::Statut(500));
        assert_eq!(err.en_erreur_http("Syno").status, StatusCode::BAD_GATEWAY);
    }

    #[tokio::test]
    async fn un_soap_fault_en_200_est_un_echec_nomme() {
        let url = faux_serveur("200 OK", SOAP_FAULT).await;
        let err = parcourir_les_enfants(&url, "faux", "0")
            .await
            .expect_err("un Fault n'est pas un dossier");
        assert_eq!(err, EchecParcours::SansResultat);
        assert_eq!(
            err.en_erreur_http("Freebox").status,
            StatusCode::BAD_GATEWAY
        );
    }

    #[tokio::test]
    async fn un_serveur_qui_ne_repond_pas_est_un_echec_de_transport() {
        // Port libre, personne n'écoute : refus de connexion immédiat.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = l.local_addr().unwrap().port();
        drop(l);
        let url = format!("http://127.0.0.1:{port}/cd/control");
        let err = parcourir_les_enfants(&url, "faux", "0")
            .await
            .expect_err("connexion refusée");
        assert!(matches!(err, EchecParcours::Transport(_)), "{err:?}");
        assert_eq!(
            err.en_erreur_http("DESKTOP").status,
            StatusCode::GATEWAY_TIMEOUT
        );
    }

    #[test]
    fn le_serveur_inconnu_rend_404_avec_son_identifiant() {
        let e = EchecParcours::ServeurInconnu("uuid:abc".into()).en_erreur_http("?");
        assert_eq!(e.status, StatusCode::NOT_FOUND);
        assert!(e.message.contains("uuid:abc"), "{}", e.message);
        assert_eq!(e.code.as_deref(), Some("media_server_browse_failed"));
    }

    #[test]
    fn reponse_porte_un_result_distingue_le_vide_du_fault() {
        assert!(reponse_porte_un_result(BROWSE_VIDE));
        assert!(!reponse_porte_un_result(SOAP_FAULT));
        assert!(reponse_porte_un_result("<Result xmlns=\"x\"></Result>"));
    }
}

/// Fil 2145 (Daniel Levy, « Disparition bibliothèque sur unité NAS ») : un
/// partage deja monte faisait echouer l'assistant avec un message faux, et le
/// meme NAS vu sous deux adresses donnait deux lignes.
#[cfg(test)]
mod tests_montage_2145 {
    use super::{Jumeau, adresse_preferee, jumeau_parmi};
    use std::net::IpAddr;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn la_decouverte_prefere_l_ipv4() {
        let synology = [
            ip("fd12:3456:789a::58d1"),
            ip("192.168.10.69"),
            ip("fe80::1"),
        ];
        assert_eq!(adresse_preferee(&synology), Some(ip("192.168.10.69")));
        // L'ordre de la collection ne change rien.
        let mut inverse = synology;
        inverse.reverse();
        assert_eq!(adresse_preferee(&inverse), Some(ip("192.168.10.69")));
        // Sans IPv4 : l'IPv6 routable plutot que le lien local.
        assert_eq!(
            adresse_preferee(&[ip("fe80::1"), ip("fd12::58d1")]),
            Some(ip("fd12::58d1"))
        );
        assert_eq!(adresse_preferee(&[]), None);
    }

    fn lignes() -> Vec<(i64, String, String, String)> {
        vec![
            (
                1,
                "fd12::58d1".into(),
                "Music".into(),
                "/mnt/fd12::58d1_Music".into(),
            ),
            (
                2,
                "192.168.10.80".into(),
                "Music".into(),
                "/mnt/192.168.10.80_Music".into(),
            ),
            (
                3,
                "fd12::58d1".into(),
                "Video".into(),
                "/mnt/fd12::58d1_Video".into(),
            ),
        ]
    }

    /// L'identite SMB2 de chaque adresse du banc : le Synology de Daniel a
    /// deux adresses et un seul GUID ; un autre NAS en a un autre.
    async fn sonde(h: String) -> Option<crate::smb::GuidServeur> {
        match h.as_str() {
            "192.168.10.69" | "fd12::58d1" | "[fd12::58d1]" => Some(*b"daniel-synology!"),
            "192.168.10.80" => Some(*b"un-autre-serveur"),
            _ => None,
        }
    }

    #[tokio::test]
    async fn le_meme_nas_sous_son_ipv4_retrouve_sa_ligne_ipv6() {
        let j = jumeau_parmi("192.168.10.69", "music", lignes(), sonde).await;
        assert_eq!(
            j,
            Some(Jumeau {
                id: 1,
                server: "fd12::58d1".into(),
                mount_path: "/mnt/fd12::58d1_Music".into(),
            })
        );
    }

    #[tokio::test]
    async fn un_autre_nas_ou_un_autre_partage_n_est_pas_un_jumeau() {
        // Autre serveur (GUID different) : aucune fusion.
        let lignes_autre = vec![lignes()[1].clone()];
        assert_eq!(
            jumeau_parmi("192.168.10.69", "Music", lignes_autre, sonde).await,
            None
        );
        // Meme serveur, autre partage.
        assert_eq!(
            jumeau_parmi("192.168.10.69", "Photo", lignes(), sonde).await,
            None
        );
        // Serveur sans identite (SMB1, injoignable) : on ne conclut rien.
        assert_eq!(
            jumeau_parmi("10.0.0.9", "Music", lignes(), sonde).await,
            None
        );
        // La meme adresse n'est pas un jumeau : `montage_existant` s'en charge.
        let meme = vec![lignes()[0].clone()];
        assert_eq!(
            jumeau_parmi("[fd12::58d1]", "Music", meme, sonde).await,
            None
        );
    }

    /// Un point de montage occupe par AUTRE CHOSE que le partage demande :
    /// `/proc`, toujours monte sur Linux. Avant le correctif, la route lancait
    /// `mount.cifs` par-dessus (EBUSY, ou commande absente) et rendait 500 avec
    /// l'erreur du dernier dialecte ; elle doit rendre 409 et nommer la cause,
    /// sans lancer de montage ni ecrire de ligne.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn un_point_occupe_par_autre_chose_rend_409_et_sa_cause() {
        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let backend = etat.backend.clone();
        let app = crate::routes::router(etat);
        let corps = serde_json::json!({
            "host": "192.168.10.69",
            "share_name": "Music",
            "mount_path": "/proc",
        });
        let rep = app
            .oneshot(
                Request::post("/api/v1/network/smb/mount")
                    .header("content-type", "application/json")
                    .body(Body::from(corps.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let statut = rep.status();
        let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&octets).unwrap_or_default();
        assert_eq!(statut, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"], "point_de_montage_occupe", "{v}");
        let message = v["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("/proc") && message.contains("proc"),
            "{message}"
        );
        let n = backend
            .query_many("SELECT id FROM network_mounts", &[])
            .unwrap()
            .len();
        assert_eq!(n, 0, "aucune ligne ne doit etre ecrite");
    }
}

/// Fil 2145 : « Oublier ce partage » demonte, puis supprime, et demande
/// confirmation si la bibliotheque en depend.
#[cfg(test)]
mod tests_oubli_2145 {
    use super::racines_dependantes;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[test]
    fn une_racine_depend_du_point_ou_d_un_dossier_dessous() {
        let dirs = vec![
            "/mnt/nas_Music".to_string(),
            "/mnt/nas_Music/Jazz/".to_string(),
            "/mnt/nas_Music2".to_string(),
            "/home/daniel/Musique".to_string(),
        ];
        assert_eq!(
            racines_dependantes(&dirs, "/mnt/nas_Music/"),
            vec![
                "/mnt/nas_Music".to_string(),
                "/mnt/nas_Music/Jazz/".to_string()
            ]
        );
        assert!(racines_dependantes(&dirs, "/mnt/autre").is_empty());
        assert!(racines_dependantes(&dirs, "").is_empty());
    }

    async fn appel(app: &axum::Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let rep = app.clone().oneshot(req).await.unwrap();
        let statut = rep.status();
        let octets = axum::body::to_bytes(rep.into_body(), usize::MAX)
            .await
            .unwrap();
        (statut, serde_json::from_slice(&octets).unwrap_or_default())
    }

    fn lignes(backend: &std::sync::Arc<dyn tune_core::db::backend::DbBackend>) -> usize {
        backend
            .query_many("SELECT id FROM network_mounts", &[])
            .unwrap()
            .len()
    }

    /// Un partage non monte, dont une racine de bibliotheque depend : sans
    /// confirmation, 409 et rien ne bouge ; confirme, la ligne disparait, le
    /// point vide aussi, et la racine reste declaree.
    #[tokio::test]
    async fn oublier_un_partage_demande_confirmation_puis_supprime() {
        use tune_core::db::backend::ToSqlValue;
        let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let backend = etat.backend.clone();
        let point = tune_core::test_scratch::scratch_dir("tune_oubli_2145").join("nas_Music");
        std::fs::create_dir_all(&point).unwrap();
        let point_s = point.to_string_lossy().to_string();
        let id = backend
            .execute_returning_id(
                "INSERT INTO network_mounts (mount_type, server, share, mount_path) VALUES ('smb', ?, ?, ?)",
                &[&"192.168.10.69" as &dyn ToSqlValue, &"Music" as &dyn ToSqlValue, &point_s as &dyn ToSqlValue],
            )
            .unwrap();
        tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone())
            .set(
                "music_dirs",
                &serde_json::to_string(&vec![point_s.clone()]).unwrap(),
            )
            .unwrap();
        let app = crate::routes::router(etat);

        let (statut, v) = appel(
            &app,
            Request::delete(format!("/api/v1/network/smb/mounts/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(statut, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["error"], "racines_dependantes");
        assert_eq!(v["racines"][0], point_s.as_str());
        assert_eq!(lignes(&backend), 1, "rien ne doit bouger sans confirmation");

        let (statut, v) = appel(
            &app,
            Request::delete(format!("/api/v1/network/smb/mounts/{id}?confirmer=true"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{v}");
        assert_eq!(v["oublie"], true);
        assert_eq!(v["demonte"], false, "le point n'etait pas monte");
        assert_eq!(lignes(&backend), 0, "la ligne doit etre supprimee");
        assert!(!point.exists(), "le point de montage vide est retire");
        assert_eq!(
            crate::routes::system::get_music_dirs_list(&backend),
            vec![point_s],
            "la racine reste declaree : son sort est le geste « retirer un dossier »"
        );

        let (statut, _) = appel(
            &app,
            Request::delete(format!("/api/v1/network/smb/mounts/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(statut, StatusCode::NOT_FOUND);
    }

    /// Decision de Bertrand (05/10) : la confirmation propose de retirer aussi
    /// les dossiers, avec la purge habituelle de leurs pistes. Le 409 dit
    /// combien de pistes partiraient ; confirme avec ce nombre, les racines
    /// sont retirees par le chemin de `POST /system/music-dirs/remove` et
    /// leurs pistes purgees ; une racine qui ne depend pas du partage reste,
    /// avec ses pistes.
    #[tokio::test]
    async fn oublier_en_retirant_les_racines_purge_leurs_pistes() {
        use tune_core::db::backend::ToSqlValue;
        use tune_core::db::models::Track;
        use tune_core::db::track_repo::TrackRepo;
        let n = tune_core::scanner::walker::normalize_path;
        let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let backend = etat.backend.clone();
        let base = tune_core::test_scratch::scratch_dir("tune_oubli_2145_c");
        let point = base.join("nas_Music");
        let autre = base.join("disque_local");
        std::fs::create_dir_all(&point).unwrap();
        let (point_s, autre_s) = (n(&point.to_string_lossy()), n(&autre.to_string_lossy()));
        let id = backend
            .execute_returning_id(
                "INSERT INTO network_mounts (mount_type, server, share, mount_path) VALUES ('smb', ?, ?, ?)",
                &[&"192.168.10.69" as &dyn ToSqlValue, &"Music" as &dyn ToSqlValue, &point_s as &dyn ToSqlValue],
            )
            .unwrap();
        tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone())
            .set(
                "music_dirs",
                &serde_json::to_string(&vec![point_s.clone(), autre_s.clone()]).unwrap(),
            )
            .unwrap();
        let repo = TrackRepo::with_backend(backend.clone());
        for chemin in [
            format!("{point_s}/a.flac"),
            format!("{point_s}/Jazz/b.flac"),
            format!("{autre_s}/c.flac"),
        ] {
            let mut t = Track::new(format!("piste {chemin}"));
            t.file_path = Some(chemin);
            repo.create(&t).unwrap();
        }
        let app = crate::routes::router(etat);

        let (statut, v) = appel(
            &app,
            Request::delete(format!("/api/v1/network/smb/mounts/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(statut, StatusCode::CONFLICT, "{v}");
        assert_eq!(v["pistes"], 2, "deux pistes vivent sur le partage : {v}");

        let (statut, v) = appel(
            &app,
            Request::delete(format!(
                "/api/v1/network/smb/mounts/{id}?confirmer=true&retirer_racines=true&confirmer_purge=2"
            ))
            .body(Body::empty())
            .unwrap(),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{v}");
        assert_eq!(v["racines_retirees"][0], point_s.as_str(), "{v}");
        assert_eq!(v["pistes_retirees"], 2, "{v}");
        assert_eq!(v["purge_refusee"], false, "{v}");
        assert_eq!(lignes(&backend), 0);
        assert_eq!(
            crate::routes::system::get_music_dirs_list(&backend),
            vec![autre_s.clone()],
            "seule la racine du partage est retiree"
        );
        let restantes = backend
            .query_many("SELECT file_path FROM tracks", &[])
            .unwrap();
        assert_eq!(restantes.len(), 1, "la piste de l'autre racine reste");
    }

    /// Sans racine dependante, l'oubli se fait sans confirmation. Un point qui
    /// porte des fichiers n'est jamais efface.
    #[tokio::test]
    async fn sans_racine_l_oubli_est_direct_et_ne_touche_pas_aux_fichiers() {
        use tune_core::db::backend::ToSqlValue;
        let etat = crate::state::AppState::new(":memory:", 0, Default::default()).unwrap();
        let backend = etat.backend.clone();
        let point = tune_core::test_scratch::scratch_dir("tune_oubli_2145_b").join("nas_Music");
        std::fs::create_dir_all(&point).unwrap();
        std::fs::write(point.join("residu.flac"), b"x").unwrap();
        let point_s = point.to_string_lossy().to_string();
        let id = backend
            .execute_returning_id(
                "INSERT INTO network_mounts (mount_type, server, share, mount_path) VALUES ('smb', ?, ?, ?)",
                &[&"fd12::58d1" as &dyn ToSqlValue, &"Music" as &dyn ToSqlValue, &point_s as &dyn ToSqlValue],
            )
            .unwrap();
        let app = crate::routes::router(etat);
        let (statut, v) = appel(
            &app,
            Request::delete(format!("/api/v1/network/smb/mounts/{id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(statut, StatusCode::OK, "{v}");
        assert_eq!(lignes(&backend), 0);
        assert!(
            point.join("residu.flac").exists(),
            "aucun fichier ne doit etre efface"
        );
    }
}
