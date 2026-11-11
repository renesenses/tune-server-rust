//! #6155 (Pierre, fil 2220) — une zone groupée joue ce que joue la zone qui
//! mène le groupe.
//!
//! Le client web forme un groupe depuis la fiche d'une zone : `POST
//! /zones/group {leader_id, zone_ids}`, la zone de la fiche en tête. Le serveur
//! rangeait le groupe dans le réglage `zone_groups`, répondait `201`, et
//! s'arrêtait là : aucun module de lecture ne lisait ce réglage. Lancer un
//! album sur le Devialet Phantom (meneuse) le faisait jouer seul ; la KEF
//! LS50 II (suiveuse) restait muette. Le groupe « existait » sans rien faire.
//!
//! Un groupe générique est, par décision (#2215), un **groupe de commandes
//! non synchronisé** : les renderers DLNA n'ont pas d'horloge commune et le
//! contrat `synchronization.supported = false` le dit déjà. Ce module ne
//! promet donc aucune synchronisation à la milliseconde ; il fait ce que le
//! groupe de commandes annonce :
//!
//! - la meneuse lance une piste (Lire, suivant, précédent, saut dans la file,
//!   enchaînement) → chaque suiveuse reçoit la file et la piste de la meneuse,
//!   à sa position, par le même chemin que le transfert de zone
//!   ([`crate::routes::playback::do_transfer`], source NON arrêtée) ;
//! - la meneuse se met en pause → les suiveuses aussi ; elle reprend → elles
//!   reprennent ;
//! - la meneuse s'arrête → les suiveuses s'arrêtent.
//!
//! La décision se prend sur l'ÉTAT des deux zones, relu au moment d'agir, et
//! jamais sur le seul nom de l'événement : une suiveuse déjà sur la bonne
//! piste n'est pas relancée (elle enchaîne seule grâce à la file copiée), et
//! un arrêt fugace de la meneuse entre deux pistes ne coupe pas les
//! suiveuses ([`DELAI_AVANT_D_ARRETER`]).
//!
//! Seuls les gestes de la MENEUSE sont relayés : une pause, une reprise, un
//! arrêt, un suivant ou un précédent donnés sur une suiveuse ne valent que
//! pour elle.
//!
//! #7717 — une exception : une NOUVELLE lecture lancée sur une zone du groupe,
//! depuis n'importe quel client, fait de cette zone la meneuse
//! ([`lancer_en_tete_du_groupe`]). Sont des lancements : `POST
//! /zones/{id}/play` avec un corps (le corps vide, Lecture après Arrêt, est une
//! reprise et n'y compte pas), le saut dans la file, la radio (« Mes radios »
//! et l'annuaire, par `jouer_un_flux`), l'épisode de podcast, le préréglage
//! épinglé, la radio d'artiste et la lecture aléatoire de la bibliothèque —
//! et, depuis #8021, le `Play` d'un point de contrôle UPnP externe sur la
//! façade MediaRenderer d'une zone, et la lecture lancée par un greffon. Un
//! lancement refusé (réponse d'échec, radio qui ne part pas) rend la tête à la
//! meneuse d'avant. Le groupe
//! formé depuis la fiche de la Chromecast suivait la Chromecast seule : une
//! lecture lancée ensuite sur l'autre zone partait sur elle seule, et la
//! Chromecast gardait son ancien titre. La meneuse change dans la route, avant
//! le lancement, jamais sur l'événement `started` : la suiveuse qui rejoint
//! émet elle aussi `started`, elle deviendrait meneuse à son tour et les deux
//! zones se renverraient la lecture.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::{info, warn};

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::playback::{PlayState, ZoneState};

use crate::state::AppState;

/// Un arrêt de la meneuse n'est relayé que s'il dure : entre deux pistes,
/// l'orchestrateur peut repasser par « arrêté » le temps d'ouvrir le flux
/// suivant. Couper les suiveuses à ce moment-là puis les relancer ferait un
/// trou audible sur chacune.
pub(crate) const DELAI_AVANT_D_ARRETER: Duration = Duration::from_millis(1500);

/// Ce qu'une suiveuse doit faire pour retrouver la meneuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Geste {
    /// Prendre la file et la piste de la meneuse, à sa position, et jouer.
    Rejoindre,
    Pause,
    Reprise,
    Arret,
}

/// L'élément joué, tel qu'on le compare d'une zone à l'autre.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Piste {
    pub(crate) source: String,
    pub(crate) track_id: Option<i64>,
    pub(crate) source_id: Option<String>,
}

/// Ce que le relais lit d'une zone : son état et sa piste.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Instantane {
    pub(crate) etat: PlayState,
    pub(crate) piste: Option<Piste>,
}

impl Instantane {
    pub(crate) fn de(zone: &ZoneState) -> Self {
        Self {
            etat: zone.state,
            piste: zone.now_playing.as_ref().map(|np| Piste {
                source: np.source.clone(),
                track_id: np.track_id,
                source_id: np.source_id.clone(),
            }),
        }
    }
}

/// Les zones qui suivent `meneuse`, dans l'ordre des groupes puis de leurs
/// `zone_ids`, sans doublon et sans la meneuse elle-même.
///
/// La meneuse d'un groupe est son `leader_id` (ce qu'envoie le client web),
/// sinon son `leader_zone_id` (forme de `/zone-manager/groups`), sinon la
/// première de ses zones — la règle même de `create_group`.
pub(crate) fn zones_suiveuses(groupes: &[Value], meneuse: i64) -> Vec<i64> {
    let mut suiveuses = Vec::new();
    for groupe in groupes {
        let zones = zones_du_groupe(groupe);
        if meneuse_du_groupe(groupe) != Some(meneuse) {
            continue;
        }
        for id in zones {
            if id != meneuse && !suiveuses.contains(&id) {
                suiveuses.push(id);
            }
        }
    }
    suiveuses
}

fn zones_du_groupe(groupe: &Value) -> Vec<i64> {
    groupe["zone_ids"]
        .as_array()
        .map(|ids| ids.iter().filter_map(Value::as_i64).collect())
        .unwrap_or_default()
}

fn meneuse_du_groupe(groupe: &Value) -> Option<i64> {
    groupe["leader_id"]
        .as_i64()
        .or_else(|| groupe["leader_zone_id"].as_i64())
        .or_else(|| zones_du_groupe(groupe).first().copied())
}

/// Le geste qui ramène `suiveuse` sur `meneuse`, ou `None` si elle y est déjà.
pub(crate) fn geste_pour_la_suiveuse(meneuse: &Instantane, suiveuse: &Instantane) -> Option<Geste> {
    let meme_piste = meneuse.piste.is_some() && meneuse.piste == suiveuse.piste;
    match meneuse.etat {
        PlayState::Playing => match suiveuse.etat {
            PlayState::Playing if meme_piste => None,
            PlayState::Paused if meme_piste => Some(Geste::Reprise),
            _ if meneuse.piste.is_none() => None,
            _ => Some(Geste::Rejoindre),
        },
        PlayState::Paused => (suiveuse.etat == PlayState::Playing).then_some(Geste::Pause),
        PlayState::Stopped => (suiveuse.etat != PlayState::Stopped).then_some(Geste::Arret),
    }
}

fn groupes_enregistres(state: &AppState) -> Vec<Value> {
    groupes_de(&state.backend)
}

fn groupes_de(backend: &Arc<dyn DbBackend>) -> Vec<Value> {
    SettingsRepo::with_backend(backend.clone())
        .get("zone_groups")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// #7717 — la meneuse qu'un groupe avait avant qu'une lecture lancée sur une
/// autre de ses zones ne la remplace : de quoi la lui rendre si ce lancement
/// est refusé.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MeneuseRemplacee {
    /// Les zones du groupe, qui le reconnaissent au moment de rendre la tête
    /// (son `id` peut se répéter, voir `create_group`).
    zones: Vec<i64>,
    meneuse: i64,
}

/// #7717 — fait de `zone` la meneuse de chaque groupe qui la compte et qu'elle
/// ne mène pas déjà. Rend les meneuses remplacées (vide : rien n'a changé).
///
/// `leader_id` est la clé que lit [`zones_suiveuses`] en premier ;
/// `leader_zone_id` (forme de `/zone-manager/groups`) suit quand il existe,
/// pour que les deux lectures du groupe disent la même meneuse.
pub(crate) fn mener_les_groupes_de(groupes: &mut [Value], zone: i64) -> Vec<MeneuseRemplacee> {
    let mut remplacees = Vec::new();
    for groupe in groupes.iter_mut() {
        let zones = zones_du_groupe(groupe);
        let Some(meneuse) = meneuse_du_groupe(groupe) else {
            continue;
        };
        if !zones.contains(&zone) || meneuse == zone {
            continue;
        }
        if nommer_la_meneuse(groupe, zone) {
            remplacees.push(MeneuseRemplacee { zones, meneuse });
        }
    }
    remplacees
}

/// #7717 — le lancement sur `zone` a été refusé : chaque groupe qu'elle mène
/// encore et dont elle avait pris la tête retrouve sa meneuse d'avant. Un
/// groupe changé entre-temps (autre meneuse, autres zones) n'est pas touché.
/// Rend `true` si un groupe a changé.
pub(crate) fn rendre_les_groupes(
    groupes: &mut [Value],
    zone: i64,
    remplacees: &[MeneuseRemplacee],
) -> bool {
    let mut change = false;
    for groupe in groupes.iter_mut() {
        if meneuse_du_groupe(groupe) != Some(zone) {
            continue;
        }
        let zones = zones_du_groupe(groupe);
        if let Some(avant) = remplacees.iter().find(|r| r.zones == zones) {
            change |= nommer_la_meneuse(groupe, avant.meneuse);
        }
    }
    change
}

fn nommer_la_meneuse(groupe: &mut Value, zone: i64) -> bool {
    let Some(objet) = groupe.as_object_mut() else {
        return false;
    };
    objet.insert("leader_id".into(), Value::from(zone));
    if objet.contains_key("leader_zone_id") {
        objet.insert("leader_zone_id".into(), Value::from(zone));
    }
    true
}

/// Sérialise les relectures-réécritures de `zone_groups` faites par les
/// lancements : deux lectures simultanées sur deux zones ne s'écrasent pas.
/// Les routes de `groupes.rs` et `zone_manager.rs` écrivent sans lui.
static MENEUSES_EN_ECRITURE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Relit les groupes, applique `changer`, et réécrit s'il a changé quelque
/// chose, sous [`MENEUSES_EN_ECRITURE`].
fn changer_les_groupes<R>(
    backend: &Arc<dyn DbBackend>,
    changer: impl FnOnce(&mut [Value]) -> R,
    a_change: impl Fn(&R) -> bool,
) -> Option<R> {
    let _garde = MENEUSES_EN_ECRITURE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut groupes = groupes_de(backend);
    let rendu = changer(&mut groupes);
    if !a_change(&rendu) {
        return Some(rendu);
    }
    let texte = serde_json::to_string(&groupes).ok()?;
    match SettingsRepo::with_backend(backend.clone()).set("zone_groups", &texte) {
        Ok(()) => Some(rendu),
        Err(error) => {
            warn!(%error, "zone_group_leader_change_failed");
            None
        }
    }
}

/// #7717 — une lecture lancée par un utilisateur sur `zone` : si `zone` est
/// dans un groupe qu'elle ne mène pas, elle en devient la meneuse, et le
/// relais y fera suivre les autres zones au `started` qui vient.
///
/// Appelé par les routes de lancement AVANT de lancer (le relais réagit au
/// `started`), jamais par le relais. Voir [`lancer_en_tete_du_groupe`].
pub(crate) fn prendre_la_tete_du_groupe(state: &AppState, zone: i64) -> Vec<MeneuseRemplacee> {
    prendre_la_tete_du_groupe_sur(&state.backend, zone)
}

/// #8021 — [`prendre_la_tete_du_groupe`] pour qui n'a que la base : l'hôte des
/// greffons ne tient pas d'`AppState` (voir `plugins_host::AppStateHost`).
pub(crate) fn prendre_la_tete_du_groupe_sur(
    backend: &Arc<dyn DbBackend>,
    zone: i64,
) -> Vec<MeneuseRemplacee> {
    let remplacees = changer_les_groupes(
        backend,
        |groupes| mener_les_groupes_de(groupes, zone),
        |r| !r.is_empty(),
    )
    .unwrap_or_default();
    if !remplacees.is_empty() {
        info!(
            zone,
            groupes = remplacees.len(),
            "zone_group_leader_changed"
        );
    }
    remplacees
}

/// #7717 — le lancement sur `zone` a échoué : rendre la tête aux meneuses
/// qu'il avait remplacées.
pub(crate) fn rendre_la_tete_du_groupe(
    state: &AppState,
    zone: i64,
    remplacees: &[MeneuseRemplacee],
) {
    rendre_la_tete_du_groupe_sur(&state.backend, zone, remplacees);
}

/// #8021 — [`rendre_la_tete_du_groupe`] pour qui n'a que la base.
pub(crate) fn rendre_la_tete_du_groupe_sur(
    backend: &Arc<dyn DbBackend>,
    zone: i64,
    remplacees: &[MeneuseRemplacee],
) {
    if remplacees.is_empty() {
        return;
    }
    if changer_les_groupes(backend, |g| rendre_les_groupes(g, zone, remplacees), |c| *c)
        == Some(true)
    {
        info!(zone, "zone_group_leader_restored");
    }
}

/// #7717 — enveloppe d'une route de lancement : `zone` prend la tête de son
/// groupe, puis la route lance ; une réponse d'échec (4xx, 5xx) rend la tête à
/// la meneuse d'avant, puisqu'aucune lecture n'est partie.
pub(crate) async fn lancer_en_tete_du_groupe<R: axum::response::IntoResponse>(
    state: &AppState,
    zone: i64,
    lancement: impl std::future::Future<Output = R>,
) -> axum::response::Response {
    let remplacees = prendre_la_tete_du_groupe(state, zone);
    let reponse = lancement.await.into_response();
    if !reponse.status().is_success() {
        rendre_la_tete_du_groupe(state, zone, &remplacees);
    }
    reponse
}

/// Ramène chaque suiveuse de `meneuse` sur ce que joue la meneuse, maintenant.
///
/// Appelé par le relais à chaque geste de la meneuse, et par `create_group`
/// quand le groupe se forme autour d'une zone qui joue déjà.
pub(crate) async fn aligner_les_suiveuses(state: &AppState, meneuse: i64) {
    let suiveuses = zones_suiveuses(&groupes_enregistres(state), meneuse);
    if suiveuses.is_empty() {
        return;
    }
    let tete = Instantane::de(&state.playback.get_state(meneuse).await);
    for suiveuse in suiveuses {
        let etat = Instantane::de(&state.playback.get_state(suiveuse).await);
        let Some(geste) = geste_pour_la_suiveuse(&tete, &etat) else {
            continue;
        };
        info!(meneuse, suiveuse, geste = ?geste, "zone_group_follower_aligned");
        let device_id = ZoneRepo::with_backend(state.backend.clone())
            .get(suiveuse)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);
        let resultat = match geste {
            Geste::Rejoindre => {
                let reponse =
                    crate::routes::playback::do_transfer(state, meneuse, suiveuse, false).await;
                if reponse.status().is_success() {
                    Ok(())
                } else {
                    Err(format!("transfer status {}", reponse.status()))
                }
            }
            Geste::Pause => state
                .orchestrator
                .pause(suiveuse, device_id.as_deref())
                .await
                .map_err(|e| format!("{e:?}")),
            Geste::Reprise => state
                .orchestrator
                .resume(suiveuse, device_id.as_deref())
                .await
                .map_err(|e| format!("{e:?}")),
            Geste::Arret => {
                state
                    .orchestrator
                    .stop(suiveuse, device_id.as_deref())
                    .await;
                Ok(())
            }
        };
        if let Err(error) = resultat {
            warn!(meneuse, suiveuse, geste = ?geste, %error, "zone_group_follower_failed");
        }
    }
}

/// #7759 (relecture de #8254) — les déplacements relayés d'une meneuse, par
/// numéro de génération.
///
/// Chaque `seek` reçu sur une meneuse prend un numéro, dans l'ordre d'arrivée
/// à la route ; le relais aux suiveuses part détaché. En glissant le curseur,
/// plusieurs relais sont en vol à la fois, et un relais plus ancien pouvait
/// finir le dernier : la suiveuse restait à une position intermédiaire. Les
/// relais d'une même meneuse passent donc un à un, et seul celui de la
/// dernière demande s'applique ; un relais dépassé ne touche plus aucune
/// suiveuse.
#[derive(Default)]
pub struct DeplacementsDeGroupe {
    par_meneuse: std::sync::Mutex<std::collections::HashMap<i64, Arc<FileDeDeplacements>>>,
}

#[derive(Default)]
struct FileDeDeplacements {
    derniere: std::sync::atomic::AtomicU64,
    un_a_la_fois: tokio::sync::Mutex<()>,
}

/// Une demande de déplacement d'une meneuse, numérotée à son arrivée.
pub struct DemandeDeDeplacement {
    file: Arc<FileDeDeplacements>,
    generation: u64,
}

impl DeplacementsDeGroupe {
    /// Numérote une nouvelle demande pour `meneuse` : elle dépasse toutes les
    /// précédentes. À appeler dans la route, AVANT de détacher le relais.
    pub fn nouvelle_demande(&self, meneuse: i64) -> DemandeDeDeplacement {
        let file = self
            .par_meneuse
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(meneuse)
            .or_default()
            .clone();
        let generation = file
            .derniere
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            + 1;
        DemandeDeDeplacement { file, generation }
    }
}

impl DemandeDeDeplacement {
    fn est_la_derniere(&self) -> bool {
        self.file.derniere.load(std::sync::atomic::Ordering::SeqCst) == self.generation
    }
}

/// #7759 (fil 2215) — un déplacement dans la piste, donné sur la meneuse, est
/// relayé à chaque suiveuse qui joue la même piste.
///
/// Le relais n'écoute que `started`, `paused`, `resumed` et `stopped` : une
/// suiveuse déjà sur la bonne piste n'avait aucune raison de bouger
/// ([`geste_pour_la_suiveuse`] rend `None`). Après un déplacement de la
/// meneuse, elle continuait donc à l'ancienne position : « une seule piste
/// avance ». Journal du testeur (rc10, deux DAC en ASIO) : chaque
/// `seek_local_output_complete` de la meneuse, aucune ligne pour la suiveuse.
///
/// Appelé par la route `seek`, une fois par geste, et non sur l'événement
/// `seek` du gestionnaire de lecture, que l'orchestrateur émet aussi pour ses
/// déplacements internes (garde du sondeur pendant une recréation de flux).
/// Une suiveuse qui ne joue pas la même piste n'est pas touchée : le relais
/// ordinaire l'aligne déjà, à la position de la meneuse.
///
/// `demande` vient de [`DeplacementsDeGroupe::nouvelle_demande`] : les relais
/// d'une meneuse passent un à un, et un relais dépassé par une demande plus
/// récente n'applique rien.
pub async fn deplacer_les_suiveuses(
    state: &AppState,
    meneuse: i64,
    position_ms: u64,
    demande: DemandeDeDeplacement,
) {
    let _un_a_la_fois = demande.file.un_a_la_fois.lock().await;
    if !demande.est_la_derniere() {
        info!(meneuse, position_ms, "zone_group_follower_seek_superseded");
        return;
    }
    let suiveuses = zones_suiveuses(&groupes_enregistres(state), meneuse);
    if suiveuses.is_empty() {
        return;
    }
    let tete = Instantane::de(&state.playback.get_state(meneuse).await);
    if tete.piste.is_none() {
        return;
    }
    for suiveuse in suiveuses {
        if !demande.est_la_derniere() {
            info!(meneuse, position_ms, "zone_group_follower_seek_superseded");
            return;
        }
        let etat = Instantane::de(&state.playback.get_state(suiveuse).await);
        if etat.piste != tete.piste || etat.etat == PlayState::Stopped {
            continue;
        }
        let device_id = ZoneRepo::with_backend(state.backend.clone())
            .get(suiveuse)
            .ok()
            .flatten()
            .and_then(|z| z.output_device_id);
        info!(meneuse, suiveuse, position_ms, "zone_group_follower_seeked");
        if let Err(error) = state
            .orchestrator
            .seek(suiveuse, position_ms, device_id.as_deref())
            .await
        {
            warn!(meneuse, suiveuse, position_ms, error = ?error, "zone_group_follower_seek_failed");
        }
    }
}

/// Lance le relais : une tâche qui écoute le canal de lecture et aligne les
/// suiveuses après chaque geste d'une meneuse. Appelé une fois au démarrage
/// (`background.rs`).
pub fn lancer_le_relais_de_groupe(state: &AppState) {
    let state = state.clone();
    let mut evenements = state.playback.subscribe();
    tokio::spawn(async move {
        loop {
            let evenement = match evenements.recv().await {
                Ok(evenement) => evenement,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(perdus)) => {
                    warn!(perdus, "zone_group_relay_lagged");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            let meneuse = evenement.zone_id;
            match evenement.event.as_str() {
                "started" | "paused" | "resumed" => aligner_les_suiveuses(&state, meneuse).await,
                // Différé hors de la boucle : l'attente ne retarde pas les
                // gestes des autres zones.
                "stopped" if !zones_suiveuses(&groupes_enregistres(&state), meneuse).is_empty() => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(DELAI_AVANT_D_ARRETER).await;
                        aligner_les_suiveuses(&state, meneuse).await;
                    });
                }
                _ => {}
            }
        }
    });
}

#[cfg(test)]
#[path = "relais_de_groupe_6155_tests.rs"]
mod tests;
