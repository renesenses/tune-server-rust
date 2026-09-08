//! Les concerts des artistes de la bibliothèque, en [`TunePlugin`] (#2363).
//!
//! Extrait du cœur toujours-compilé :
//!
//! - `tune-core/src/cloud/concert_alerts.rs` — la tâche de fond qui pousse
//!   toutes les 24 h les artistes de la bibliothèque vers
//!   `mozaiklabs.fr/api/v1/premium/concerts/subscribe`. Elle était démarrée
//!   **sans condition** par `background.rs`, dans tous les serveurs, y compris
//!   ceux dont personne n'a jamais demandé la fonction.
//! - `GET /api/v1/system/concerts` — la route de lecture, remontée ici sur
//!   `/api/v1/ext/concerts/upcoming` (le préfixe vient de `name()` : un plugin
//!   ne choisit jamais le sien).
//!
//! Bertrand a tranché le 29/08 : la fonction sera un plugin. Le cœur nu cesse
//! donc de parler à un service tiers, et la tâche de fond ne tourne plus que
//! chez ceux qui ont installé le plugin.
//!
//! # L'extraction a été rebasée sur #2892, pas sur la version d'avant
//!
//! Ce greffon a d'abord été écrit comme un portage littéral de
//! `concert_alerts.rs` **tel qu'il était le 29/08**. Entre-temps, le 30/08,
//! #2892 a réécrit ce même fichier dans la ligne de release (+275 / −38) :
//! l'abonnement porte désormais sur TOUTE la bibliothèque et non plus sur les
//! seuls artistes identifiés par un MusicBrainz ID.
//!
//! La fusion rendait un conflit `modify/delete` : la PR supprime le fichier,
//! la ligne de release le réécrit. Prendre la suppression — le réflexe, puisque
//! c'est l'intention de la PR — aurait annulé #2892 **sans qu'aucun test ne
//! rougisse**, le greffon compilant parfaitement avec l'ancienne requête. Le
//! comportement de #2892 a donc été reporté ici, et il est gardé par des tests
//! (`tune-server/tests/concerts_plugin.rs`) qui portent sur le fait de base :
//! un artiste sans MBID est abonné comme les autres.
//!
//! # Et l'apport de #2178, porté à la fusion de `rc/v0.9.130`
//!
//! Même piège, une seconde fois, sur le même fichier. Le lot
//! `batch/p2-recentes-1` portait `64e8378f` — « un 429 du nuage dit la limite
//! et le délai, partout » — qui apprenait à `concert_alerts.rs` à rendre un
//! [`CloudError`] plutôt qu'une `String`, et à `GET /system/concerts` à rendre
//! ce refus **sans écraser son statut**. Ce fichier et cette route étant
//! supprimés ici, prendre la suppression aurait perdu le traitement du 429
//! pour les concerts — en silence, une fois de plus : le greffon compile très
//! bien en rendant 200 sur tous les refus.
//!
//! Le comportement a donc été porté ([`reponse_de_refus`]), et gardé par des
//! tests qui portent sur le fait de base : un 429 du nuage arrive au client
//! **en 429, avec son délai**.
//!
//! # Ce que ce plugin ne fait PAS, et pourquoi
//!
//! **Il n'est pas au catalogue** ([`ConcertsPlugin::catalogued`] rend `false`).
//! Aucun écran ne consomme encore ces routes — `git grep -i concert` dans
//! `tune-web-client` ne rend rien de la fonction. Offrir « Installer » sur une
//! fonction que rien n'expose dépense la confiance de l'utilisateur et ne rend
//! rien : il installe, il redémarre comme on le lui demande, et rien
//! n'apparaît (#2090). À rebrancher au catalogue le jour où l'écran existe.
//!
//! **Il ne rendra rien tant que le cloud n'aura pas de source.** La seule
//! source branchée aujourd'hui est MusicBrainz, dont l'entité `event` est une
//! archive : 0 date future sur Coldplay, Taylor Swift et Metallica réunis.
//! Ce n'est pas un défaut de ce plugin — la table `concert_events` est vide
//! côté cloud, et le rester est le sujet du lot 1, ailleurs.
//!
//! **Il ne collecte aucune position.** Le filtre géographique (rayon / pays /
//! partout, arbitré le 29/08) est le lot 2, et il commence côté cloud : la
//! route `upcoming` accepte `city` et `country` aujourd'hui sans rien en
//! faire. Envoyer une position que personne ne lit ne servirait personne.
//!
//! **Il ne pose pas de portillon premium.** L'arbitrage « réservé aux
//! premium » est acté, mais `tune-core/src/license.rs` n'a aucune variante
//! `Feature` pour les concerts, et en ajouter une touche aussi le catalogue
//! côté client et côté cloud. C'est le lot 5, et il doit sortir *en même temps*
//! que l'écran — sinon on pose un refus que personne ne peut voir.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use tune_core::cloud::refusal::CloudError;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::event_bus::TuneEvent;
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

const CONCERTS_API: &str = "https://mozaiklabs.fr/api/v1/premium/concerts";

/// Le nuage n'accepte pas plus de 200 artistes par appel (`artists => max:200`).
///
/// Publique pour que le test de découpage lise la VRAIE borne : un test qui
/// réécrirait `200` à la main resterait vert si le code changeait de taille de
/// lot et se remettait à couper.
pub const LOT: usize = 200;

/// Plafond de sécurité, en artistes. Une bibliothèque ordinaire en compte
/// quelques milliers (1 747 sur le serveur de référence, soit 9 appels) ; ce
/// plafond n'existe que pour qu'une bibliothèque pathologique ne parte pas en
/// centaines de requêtes. Une troncature est TOUJOURS signalée dans le journal :
/// un abonnement silencieusement amputé se lit comme « ce groupe ne joue nulle
/// part » côté utilisateur.
pub const PLAFOND: usize = 5_000;

/// Services de l'hôte remis au plugin à la construction.
///
/// Passés explicitement plutôt que tirés du [`PluginContext`], comme
/// `tune-dj`, `tune-karaoke` et `tune-bandcamp` : la vraie dépendance du
/// plugin — la base — est ainsi visible au point de câblage, dans
/// `tune-server/src/plugins.rs`.
pub struct HostServices {
    pub backend: Arc<dyn DbBackend>,
}

pub struct ConcertsPlugin {
    backend: Arc<dyn DbBackend>,
    /// La tâche d'abonnement périodique, pour l'arrêter au `teardown`.
    ///
    /// Le cœur ne gardait aucune poignée : `tokio::spawn` et plus rien. Une
    /// tâche de plugin doit pouvoir s'arrêter quand le plugin s'arrête, sinon
    /// elle survit à son propriétaire et continue d'appeler le cloud.
    tache: Option<tokio::task::JoinHandle<()>>,
}

impl ConcertsPlugin {
    pub fn new(services: HostServices) -> Self {
        Self {
            backend: services.backend,
            tache: None,
        }
    }
}

#[async_trait]
impl TunePlugin for ConcertsPlugin {
    fn name(&self) -> &str {
        "concerts"
    }
    fn version(&self) -> &str {
        env!("CARGO_PKG_VERSION")
    }
    fn description(&self) -> &str {
        "Concerts à venir des artistes de la bibliothèque"
    }

    /// Opt-in, comme `dj`, `karaoke` et `bandcamp`.
    fn default_enabled(&self) -> bool {
        false
    }

    /// Hors catalogue tant qu'aucun écran ne consomme ces routes — voir l'en-
    /// tête du module. Le plugin reste compilé, testé, et se charge si
    /// `plugin_concerts_installed` est posé à la main.
    fn catalogued(&self) -> bool {
        false
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        ctx.register_router(router(self.backend.clone()));
        self.tache = Some(lancer_synchronisation(self.backend.clone()));
        Ok(())
    }

    async fn teardown(&mut self) -> Result<(), String> {
        if let Some(t) = self.tache.take() {
            t.abort();
        }
        Ok(())
    }

    /// Ce plugin n'observe pas la lecture : il interroge le cloud sur une
    /// horloge. Surcharge explicite en no-op pour ne pas recevoir tout le bus
    /// pour rien.
    async fn on_event(&mut self, _event: &TuneEvent) {}
}

// ---------------------------------------------------------------------------
// Routes — montées par l'hôte sous /api/v1/ext/concerts
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct EtatConcerts {
    backend: Arc<dyn DbBackend>,
}

pub fn router(backend: Arc<dyn DbBackend>) -> Router<()> {
    Router::new()
        .route("/upcoming", get(concerts_a_venir))
        .with_state(EtatConcerts { backend })
}

/// `GET /api/v1/ext/concerts/upcoming` — remplace `GET /system/concerts`.
///
/// Le corps d'erreur de l'ancienne route était une chaîne technique anglaise
/// (`{"concerts": [], "error": "concerts: HTTP 500"}`) qu'une interface
/// traduite en 11 langues aurait affichée telle quelle. On rend désormais un
/// **code stable**, traduisible côté client, et le détail part au journal.
async fn concerts_a_venir(
    axum::extract::State(etat): axum::extract::State<EtatConcerts>,
) -> Response {
    let instance_id = SettingsRepo::with_backend(etat.backend.clone())
        .get("instance_id")
        .ok()
        .flatten()
        .unwrap_or_default();

    if instance_id.is_empty() {
        return Json(json!({"concerts": [], "code": "concerts.no_instance_id"})).into_response();
    }

    let client = match tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "concerts_client_build_failed");
            return Json(json!({"concerts": [], "code": "concerts.unavailable"})).into_response();
        }
    };

    match recuperer_concerts(&client, &instance_id).await {
        Ok(concerts) => Json(json!({"concerts": concerts})).into_response(),
        Err(e) => {
            warn!(error = %e, retry_after = ?e.retry_after(), "concerts_fetch_failed");
            reponse_de_refus(&e)
        }
    }
}

/// Rend un refus du nuage **sans en perdre le motif** — la forme greffon de
/// `routes::cloud_error::reponse` (#2178).
///
/// # Pourquoi ce n'est pas un appel à la fabrique commune
///
/// `tune-server/src/routes/cloud_error.rs` rend ce contrat pour les quinze
/// gestionnaires du cœur. Ce greffon **ne peut pas l'appeler** : il dépend de
/// `tune-core`, jamais de `tune-server` — l'inverse ferait un cycle, puisque
/// c'est `tune-server` qui monte ce routeur. Ce qui est partagé l'est au bon
/// niveau : le **type** du refus, [`CloudError`], et la lecture du délai
/// (`cloud::rate_limit::retry_after_secs`), tous deux dans `tune-core`. Seul le
/// rendu est refait ici, et il l'est sur la forme propre au greffon.
///
/// # Ce qui diffère de la fabrique du cœur, et pourquoi
///
/// La fabrique du cœur pose un `message` **déjà traduit** (`crate::i18n::t`,
/// dix langues). Ce greffon n'en pose pas : `i18n_server.json` vit dans
/// `tune-server`, hors de portée — mais surtout, ne pas traduire ici est la
/// règle que ce greffon s'est donnée en sortant du cœur. L'ancienne route
/// rendait `{"error": "concerts: HTTP 500"}`, une phrase anglaise qu'une
/// interface traduite en onze langues affichait telle quelle ; le greffon rend
/// un **code stable** que le client traduit. Le 429 suit cette règle : il se
/// nomme `concerts.rate_limited`, il ne se raconte pas.
///
/// Le reste du contrat est tenu mot pour mot :
///
/// * le **statut 429 est préservé** — l'ancienne route rendait 200 sur un
///   refus, et c'est précisément ce qui empêchait de le reconnaître ;
/// * `retry_after` en secondes **quand le distant l'annonce**, jamais fabriqué ;
/// * l'en-tête `Retry-After` réémis, forme standard pour qui programme ;
/// * le texte amont conservé sous `upstream_message` ;
/// * l'enveloppe `{"concerts": []}` conservée, pour l'écran qui rend la liste
///   avant de regarder l'erreur.
///
/// Hors 429, **rien ne bouge** : 200 et `concerts.unavailable`, comme avant.
///
/// Publique pour être observable depuis `tune-server/tests/concerts_plugin.rs`,
/// de l'autre côté de la frontière de crate — même raison que
/// [`artistes_de_la_bibliotheque`].
pub fn reponse_de_refus(err: &CloudError) -> Response {
    let CloudError::RateLimited {
        retry_after,
        upstream,
        ..
    } = err
    else {
        return Json(json!({"concerts": [], "code": "concerts.unavailable"})).into_response();
    };

    let mut corps = serde_json::Map::new();
    corps.insert("concerts".into(), json!([]));
    corps.insert("code".into(), json!("concerts.rate_limited"));
    if let Some(secs) = retry_after {
        corps.insert("retry_after".into(), json!(secs));
    }
    if !upstream.is_empty() {
        corps.insert("upstream_message".into(), json!(upstream));
    }

    let mut resp = (StatusCode::TOO_MANY_REQUESTS, Json(Value::Object(corps))).into_response();
    if let Some(secs) = retry_after {
        if let Ok(v) = header::HeaderValue::from_str(&secs.to_string()) {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
    }
    resp
}

// ---------------------------------------------------------------------------
// Le cloud — repris de tune-core/src/cloud/concert_alerts.rs, dans son état
// après #2892 (40f9342c) : l'abonnement porte sur toute la bibliothèque.
// La lecture (`recuperer_concerts`) a depuis reçu l'apport de #2178
// (64e8378f) à la fusion de rc/v0.9.130 : elle rend un `CloudError`, et un
// 429 du nuage arrive au client en 429. Voir l'en-tête et
// [`reponse_de_refus`].
// ---------------------------------------------------------------------------

/// Les artistes de la bibliothèque, prêts à être abonnés.
///
/// ⚠️ LE MBID N'EST PLUS EXIGÉ. Cette requête filtrait `musicbrainz_id IS NOT
/// NULL`, ce qui plafonnait la fonction à la part identifiée de la bibliothèque
/// — quelques pour cent sur une installation ordinaire.
///
/// Mesure du 30/08/2026 contre l'agenda Ticketmaster, sur les 1 747 artistes du
/// serveur de référence : 881 d'entre eux (50,4 %) sont reconnus par leur seul
/// NOM, mais seules 460 des attractions correspondantes portent un lien
/// MusicBrainz. Exiger le MBID écartait donc la moitié des concerts que la
/// source sait rendre, en plus de tous les artistes non identifiés localement.
///
/// Le MBID reste envoyé quand on l'a : c'est la meilleure identité disponible,
/// il a simplement cessé d'être une condition d'entrée.
///
/// `GROUP BY name` parce que la même personne peut apparaître sur plusieurs
/// lignes — l'une identifiée, l'autre non. Le nuage classe désormais par nom
/// replié : envoyer deux fois le même artiste ne ferait que gonfler la charge.
///
/// # Pourquoi cette fonction est publique
///
/// Elle l'est pour être **observable depuis un test**. Dans le cœur, cet apport
/// (#2892) était gardé par un `#[cfg(test)] mod tests` interne au fichier. Un
/// greffon n'a pas ce luxe : ses tests vivent dans `tune-server`
/// (`tests/concerts_plugin.rs`), de l'autre côté de la frontière de crate. Sans
/// ce point d'observation, la seule voie serait le HTTP vers `mozaiklabs.fr`,
/// et le fait de base — « un artiste sans MBID part quand même » — redeviendrait
/// invérifiable, c'est-à-dire effaçable en silence. C'est exactement ce que
/// cette PR a failli faire.
pub fn artistes_de_la_bibliotheque(backend: &Arc<dyn DbBackend>) -> Result<Vec<Value>, String> {
    // `PLAFOND` est injecté plutôt qu'écrit en dur : si le `LIMIT` et le seuil
    // d'alerte divergeaient, la troncature redeviendrait silencieuse — le
    // défaut même que ce code corrige.
    let sql = format!(
        "SELECT name, MAX(musicbrainz_id) FROM artists \
         WHERE name IS NOT NULL AND name != '' \
         GROUP BY name ORDER BY name \
         LIMIT {PLAFOND}"
    );

    let rows = backend
        .query_many(&sql, &[])
        .map_err(|e| format!("query: {e}"))?;

    Ok(rows
        .iter()
        .filter_map(|r| {
            let nom = r.first().and_then(|v| v.as_string())?;
            if nom.is_empty() {
                return None;
            }
            let mbid = r
                .get(1)
                .and_then(|v| v.as_string())
                .filter(|m| !m.is_empty());

            Some(json!({
                "artist_name": nom,
                "musicbrainz_artist_id": mbid,
            }))
        })
        .collect())
}

/// Pousse les artistes de la bibliothèque comme abonnements de concerts.
/// Rend le nombre d'artistes abonnés.
///
/// ⚠️ ORDRE DE DÉPLOIEMENT. Cette fonction envoie des artistes SANS
/// `musicbrainz_artist_id`. Le nuage ne l'accepte que depuis site-mozaiklabs#185
/// (30/08/2026) ; une version antérieure répondait 422 sur la charge entière.
/// Le nuage se déploie en continu et cette version de Tune passe par un train de
/// release, donc l'ordre est acquis en pratique — mais il faut le savoir avant
/// de rejouer ce code sur une instance pointant vers un nuage figé.
pub async fn synchroniser_abonnements(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<usize, String> {
    let artistes = artistes_de_la_bibliotheque(backend)?;
    envoyer_abonnements(CONCERTS_API, http_client, instance_id, &artistes).await
}

/// L'envoi proprement dit : le découpage en lots, la tolérance au lot perdu et
/// le décompte. Séparé de [`synchroniser_abonnements`] pour être **appelable**
/// sans base et sans nuage.
///
/// # Pourquoi la racine de l'API est un argument
///
/// Sans elle, la seule façon d'exercer ce code serait de parler à
/// `mozaiklabs.fr` depuis un essai — c'est-à-dire jamais. Le découpage était de
/// fait le seul apport de ce greffon qu'aucun essai n'atteignait :
/// `tune-server/tests/concerts_plugin.rs` découpe lui-même un vecteur avec
/// `chunks(LOT)` et vérifie sa propre arithmétique. C'est un essai qui **relit**
/// le code au lieu de l'**appeler** : le jour où cette boucle-ci se remettrait à
/// couper à 200, il resterait vert.
///
/// L'unique appelant en production est [`synchroniser_abonnements`] juste
/// au-dessus, et il passe [`CONCERTS_API`].
pub async fn envoyer_abonnements(
    racine: &str,
    http_client: &reqwest::Client,
    instance_id: &str,
    artistes: &[Value],
) -> Result<usize, String> {
    if artistes.is_empty() {
        debug!("concert_alerts_no_artists");
        return Ok(0);
    }

    if artistes.len() >= PLAFOND {
        warn!(
            plafond = PLAFOND,
            "concert_subscriptions_tronquees: bibliotheque au-dela du plafond, \
             les artistes suivants ne seront pas abonnes"
        );
    }

    // Un seul appel ne peut porter que 200 artistes : au-delà, l'ancienne
    // requête coupait à 200 sans le dire. On découpe et on additionne.
    let mut total = 0usize;
    let mut ignores = 0usize;
    let mut lots_en_echec = 0usize;
    let nombre_de_lots = artistes.len().div_ceil(LOT);

    for lot in artistes.chunks(LOT) {
        let body = json!({
            "instance_id": instance_id,
            "artists": lot,
        });

        let resp = http_client
            .post(format!("{racine}/subscribe"))
            .json(&body)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await;

        // Un lot en échec ne condamne pas les autres : mieux vaut abonner
        // 1 500 artistes sur 1 747 que zéro parce que le huitième appel a
        // rencontré une coupure réseau.
        let resp = match resp {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                // Le refus de l'abonnement ne remonte à aucun écran : la tâche
                // est périodique et personne ne l'attend. Le délai annoncé est
                // tout de même lu et journalisé (#2178) — sans lui, « lot
                // refusé » ne dit pas si le nuage demande d'attendre une minute
                // ou une heure, et c'est la seule trace qu'on aura. `None` veut
                // dire « le distant ne l'a pas dit » : jamais fabriqué.
                let retry_after = tune_core::cloud::rate_limit::retry_after_secs(r.headers());
                warn!(statut = %r.status(), ?retry_after, "concert_subscribe_lot_refuse");
                lots_en_echec += 1;
                continue;
            }
            Err(e) => {
                warn!(error = %e, "concert_subscribe_lot_echoue");
                lots_en_echec += 1;
                continue;
            }
        };

        let result: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "concert_subscribe_lot_illisible");
                lots_en_echec += 1;
                continue;
            }
        };

        total += result["subscribed"].as_i64().unwrap_or(0) as usize;
        // Le nuage écarte les noms qui ne désignent aucun artiste
        // (« Various Artists », « Unknown »...). Les compter permet de voir
        // d'un coup d'œil si une bibliothèque est surtout faite de compilations.
        ignores += result["ignored"].as_i64().unwrap_or(0) as usize;
    }

    if lots_en_echec == nombre_de_lots {
        return Err(format!(
            "concert subscribe: {nombre_de_lots} lot(s) en echec"
        ));
    }

    info!(
        count = total,
        ignores,
        lots = nombre_de_lots,
        lots_en_echec,
        "concert_subscriptions_synced"
    );
    Ok(total)
}

/// Récupère les concerts à venir pour les artistes auxquels cette instance
/// s'est abonnée.
///
/// Le refus est rendu en [`CloudError`] et non plus en `String` (#2178) : un
/// 429 y garde son délai (`Retry-After`, à défaut `X-RateLimit-Reset`) et le
/// texte du distant, que [`reponse_de_refus`] fait ensuite ressortir jusqu'au
/// client. Les autres erreurs — réseau, analyse — passent inchangées par
/// `impl From<String>`, et le texte rendu par `Display` reste mot pour mot
/// celui d'avant : les journaux ne bougent pas.
pub async fn recuperer_concerts(
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<Vec<Value>, CloudError> {
    recuperer_concerts_depuis(CONCERTS_API, http_client, instance_id).await
}

/// La lecture, avec la racine de l'API en argument — même raison que
/// [`envoyer_abonnements`] : c'est le seul moyen d'exercer la traduction d'un
/// refus du nuage en [`CloudError`] sans appeler `mozaiklabs.fr`.
///
/// L'unique appelant en production est [`recuperer_concerts`] juste au-dessus.
pub async fn recuperer_concerts_depuis(
    racine: &str,
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<Vec<Value>, CloudError> {
    let resp = http_client
        .get(format!("{racine}/upcoming"))
        .query(&[("instance_id", instance_id)])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("concerts: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        return Err(CloudError::from_response(format!("concerts: HTTP {status}"), resp).await);
    }

    let data: Value = resp.json().await.map_err(|e| format!("parse: {e}"))?;
    let concerts = data["concerts"].as_array().cloned().unwrap_or_default();
    info!(count = concerts.len(), "upcoming_concerts_fetched");
    Ok(concerts)
}

/// La tâche périodique : abonnement toutes les 24 h, 2 min après le démarrage.
///
/// Le double garde-fou de l'original est conservé : le réglage
/// `community_sync_enabled` **et** un `instance_id` non vide. Ce qui change,
/// c'est qu'elle ne démarre plus que si le plugin est installé — avant, elle
/// tournait dans tous les serveurs.
fn lancer_synchronisation(backend: Arc<dyn DbBackend>) -> tokio::task::JoinHandle<()> {
    let client = match tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "concert_alerts_client_build_failed");
            return tokio::spawn(async {});
        }
    };

    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(120)).await;

        loop {
            let settings = SettingsRepo::with_backend(backend.clone());
            let enabled = settings
                .get("community_sync_enabled")
                .ok()
                .flatten()
                .map(|v| v == "true")
                .unwrap_or(false);

            if enabled {
                let instance_id = settings
                    .get("instance_id")
                    .ok()
                    .flatten()
                    .unwrap_or_default();

                if !instance_id.is_empty() {
                    if let Err(e) = synchroniser_abonnements(&backend, &client, &instance_id).await
                    {
                        warn!(error = %e, "concert_subscriptions_sync_failed");
                    }
                } else {
                    debug!("concert_alerts_skipped_no_instance_id");
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(86400)).await;
        }
    })
}

// ---------------------------------------------------------------------------
// Essais (#3640)
// ---------------------------------------------------------------------------
//
// Cette caisse rendait `tune_concerts: 0 passed` dans les deux jobs qui la
// nomment. `tune-server/tests/concerts_plugin.rs` en garde déjà la moitié
// haute — le montage du routeur, le hors-catalogue, l'arrêt de la tâche, la
// requête d'artistes, et le RENDU d'un refus par `reponse_de_refus`.
//
// Ce qui restait sans aucun témoin, c'est tout ce qui parle au nuage :
//
//   * le découpage en lots de `LOT` et sa tolérance au lot perdu. L'essai de
//     `tune-server` découpe LUI-MÊME un vecteur avec `chunks(LOT)` et vérifie
//     sa propre arithmétique — il relit le code au lieu de l'appeler, et
//     resterait vert si la boucle d'envoi se remettait à couper à 200 ;
//   * la LECTURE d'un refus : `reponse_de_refus` est gardée, mais rien ne
//     vérifiait que `recuperer_concerts` construit bien le `CloudError` qu'elle
//     rend. Les deux moitiés du 429 sont désormais tenues.

#[cfg(test)]
mod essais {
    use std::sync::Mutex;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::*;

    /// Une réponse du banc : statut, corps, et le `Retry-After` à annoncer.
    type Reponse = (u16, &'static str, Option<u64>);

    /// Un banc HTTP minimal : il répond dans l'ordre du script et garde ce
    /// qu'il a reçu.
    ///
    /// ⚠️ Il lit la requête **entière** — ligne, en-têtes et corps — avant
    /// d'écrire, puis ferme par un `shutdown` explicite. Un banc qui répond
    /// sans avoir lu fait émettre un RST par le noyau, et le RST détruit la
    /// réponse encore en vol : c'est la vraie cause de l'instabilité cherchée
    /// pendant des jours sur #1358.
    struct Banc {
        racine: String,
        recues: Arc<Mutex<Vec<Value>>>,
        tache: tokio::task::JoinHandle<()>,
    }

    impl Banc {
        /// Ce que le banc a reçu : `{"cible": "…", "corps": …}` par requête.
        fn recues(&self) -> Vec<Value> {
            self.recues.lock().unwrap().clone()
        }
    }

    impl Drop for Banc {
        fn drop(&mut self) {
            self.tache.abort();
        }
    }

    fn position(foin: &[u8], aiguille: &[u8]) -> Option<usize> {
        foin.windows(aiguille.len()).position(|f| f == aiguille)
    }

    /// Le dernier élément du script est réutilisé si les appels le dépassent :
    /// un essai qui veut « tout refuser » n'écrit qu'une réponse.
    async fn banc(script: Vec<Reponse>) -> Banc {
        assert!(
            !script.is_empty(),
            "le script du banc ne peut pas etre vide"
        );
        let ecoute = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let racine = format!("http://{}", ecoute.local_addr().unwrap());
        let recues: Arc<Mutex<Vec<Value>>> = Arc::new(Mutex::new(Vec::new()));
        let journal = recues.clone();

        let tache = tokio::spawn(async move {
            let mut appel = 0usize;
            loop {
                let Ok((mut flux, _)) = ecoute.accept().await else {
                    return;
                };

                // 1. Lire la requête entière AVANT d'écrire quoi que ce soit.
                let mut brut: Vec<u8> = Vec::new();
                let mut tampon = [0u8; 4096];
                let complete = loop {
                    let lu = match flux.read(&mut tampon).await {
                        Ok(0) | Err(_) => break false,
                        Ok(n) => n,
                    };
                    brut.extend_from_slice(&tampon[..lu]);
                    let Some(fin) = position(&brut, b"\r\n\r\n") else {
                        continue;
                    };
                    let entetes = String::from_utf8_lossy(&brut[..fin]).to_lowercase();
                    let taille = entetes
                        .split("content-length:")
                        .nth(1)
                        .and_then(|s| s.split("\r\n").next())
                        .and_then(|s| s.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if brut.len() >= fin + 4 + taille {
                        let cible = String::from_utf8_lossy(&brut[..fin])
                            .lines()
                            .next()
                            .unwrap_or_default()
                            .to_string();
                        let corps = serde_json::from_slice::<Value>(&brut[fin + 4..])
                            .unwrap_or(Value::Null);
                        journal.lock().unwrap().push(json!({
                            "cible": cible,
                            "corps": corps,
                        }));
                        break true;
                    }
                };
                if !complete {
                    continue;
                }

                // 2. Répondre, puis fermer proprement.
                let (statut, charge, retry) = script
                    .get(appel)
                    .copied()
                    .unwrap_or(script[script.len() - 1]);
                appel += 1;
                let mut tete = format!(
                    "HTTP/1.1 {statut} R\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
                    charge.len()
                );
                if let Some(secondes) = retry {
                    tete.push_str(&format!("Retry-After: {secondes}\r\n"));
                }
                tete.push_str("\r\n");
                tete.push_str(charge);
                let _ = flux.write_all(tete.as_bytes()).await;
                let _ = flux.flush().await;
                let _ = flux.shutdown().await;
            }
        });

        Banc {
            racine,
            recues,
            tache,
        }
    }

    fn artistes(n: usize) -> Vec<Value> {
        (0..n)
            .map(|i| json!({"artist_name": format!("Artiste {i:04}"), "musicbrainz_artist_id": null}))
            .collect()
    }

    // -----------------------------------------------------------------------
    // `envoyer_abonnements` — appelée par `synchroniser_abonnements`
    // -----------------------------------------------------------------------

    /// ⭐ Le découpage, exercé pour de vrai. L'ancienne requête coupait à 200
    /// SANS LE DIRE : sur les 1 747 artistes du serveur de référence, 1 547
    /// n'étaient jamais abonnés et personne ne pouvait le savoir. Ce témoin
    /// compte les appels **reçus par le nuage**, pas les tranches d'un vecteur.
    #[tokio::test]
    async fn quatre_cent_cinquante_artistes_partent_en_trois_appels_et_aucun_ne_se_perd() {
        let banc = banc(vec![
            (200, r#"{"subscribed":200,"ignored":0}"#, None),
            (200, r#"{"subscribed":200,"ignored":0}"#, None),
            (200, r#"{"subscribed":50,"ignored":0}"#, None),
        ])
        .await;

        let total = envoyer_abonnements(
            &banc.racine,
            &reqwest::Client::new(),
            "inst-1",
            &artistes(450),
        )
        .await
        .unwrap();
        assert_eq!(total, 450, "le total doit additionner les trois reponses");

        let recues = banc.recues();
        assert_eq!(recues.len(), 3, "450 artistes = 3 appels au nuage");
        let mut noms: Vec<String> = Vec::new();
        for appel in &recues {
            assert_eq!(appel["corps"]["instance_id"], "inst-1");
            let lot = appel["corps"]["artists"].as_array().unwrap();
            assert!(
                lot.len() <= LOT,
                "un lot de {} depasse la borne du nuage ({LOT})",
                lot.len()
            );
            noms.extend(
                lot.iter()
                    .map(|a| a["artist_name"].as_str().unwrap().to_string()),
            );
        }
        assert_eq!(noms.len(), 450, "aucun artiste ne doit rester a quai");
        noms.sort();
        noms.dedup();
        assert_eq!(noms.len(), 450, "aucun artiste ne doit partir deux fois");
    }

    /// « Un lot en échec ne condamne pas les autres » : mieux vaut abonner
    /// 250 artistes que zéro parce que le deuxième appel est tombé.
    #[tokio::test]
    async fn un_lot_refuse_ne_condamne_pas_les_suivants() {
        let banc = banc(vec![
            (200, r#"{"subscribed":200}"#, None),
            (500, r#"{"message":"boum"}"#, None),
            (200, r#"{"subscribed":50}"#, None),
        ])
        .await;

        let total = envoyer_abonnements(
            &banc.racine,
            &reqwest::Client::new(),
            "inst-1",
            &artistes(450),
        )
        .await
        .unwrap();
        assert_eq!(total, 250, "les deux lots passes doivent compter");
        assert_eq!(
            banc.recues().len(),
            3,
            "le troisieme lot doit partir malgre l'echec du deuxieme"
        );
    }

    /// Contre-épreuve de la tolérance : quand TOUT échoue, il faut une erreur.
    /// Un `Ok(0)` paisible se lirait dans le journal comme « bibliothèque
    /// vide », c'est-à-dire comme un fait, alors que le nuage est en panne.
    #[tokio::test]
    async fn tous_les_lots_en_echec_rendent_une_erreur_et_non_un_zero_paisible() {
        let banc = banc(vec![(500, r#"{}"#, None)]).await;

        let resultat = envoyer_abonnements(
            &banc.racine,
            &reqwest::Client::new(),
            "inst-1",
            &artistes(450),
        )
        .await;

        let Err(motif) = resultat else {
            panic!("trois lots refuses doivent rendre une erreur, pas un Ok");
        };
        assert!(
            motif.contains('3'),
            "l'erreur doit dire combien de lots sont tombes : {motif}"
        );
    }

    /// Un 429 est un refus comme un autre pour cette tâche : il est journalisé
    /// avec son délai, et les lots suivants partent quand même. La tâche est
    /// périodique, personne ne l'attend — l'arrêter perdrait les 250 autres.
    #[tokio::test]
    async fn un_429_sur_un_lot_ne_fait_pas_tomber_la_tache() {
        let banc = banc(vec![
            (429, r#"{"message":"Too Many Attempts."}"#, Some(90)),
            (200, r#"{"subscribed":200}"#, None),
            (200, r#"{"subscribed":50}"#, None),
        ])
        .await;

        let total = envoyer_abonnements(
            &banc.racine,
            &reqwest::Client::new(),
            "inst-1",
            &artistes(450),
        )
        .await
        .unwrap();
        assert_eq!(total, 250);
        assert_eq!(banc.recues().len(), 3);
    }

    /// Une bibliothèque vide ne doit produire AUCUN appel : abonner « rien »
    /// ferait tourner une requête toutes les 24 h chez chaque installation
    /// fraîche, et le nuage la compterait dans son quota.
    #[tokio::test]
    async fn une_bibliotheque_vide_ne_touche_pas_le_reseau() {
        let banc = banc(vec![(200, r#"{"subscribed":0}"#, None)]).await;

        let total = envoyer_abonnements(&banc.racine, &reqwest::Client::new(), "inst-1", &[])
            .await
            .unwrap();

        assert_eq!(total, 0);
        assert!(
            banc.recues().is_empty(),
            "aucun appel ne doit partir sur une bibliotheque vide"
        );
    }

    /// Le contrat de fil avec le nuage (site-mozaiklabs#185) : le nom part
    /// toujours, le MBID part quand on l'a et vaut `null` sinon. Un artiste
    /// sans MBID doit partir COMME LES AUTRES — c'est l'apport de #2892, et
    /// c'est ce que ce témoin voit maintenant dans la charge réellement émise.
    #[tokio::test]
    async fn le_nom_et_le_mbid_partent_tels_quels_dans_la_charge() {
        let banc = banc(vec![(200, r#"{"subscribed":2}"#, None)]).await;
        let tous = vec![
            json!({"artist_name": "Superbus", "musicbrainz_artist_id": "abc-123"}),
            json!({"artist_name": "Groupe sans identite", "musicbrainz_artist_id": null}),
        ];

        envoyer_abonnements(&banc.racine, &reqwest::Client::new(), "inst-42", &tous)
            .await
            .unwrap();

        let recues = banc.recues();
        assert_eq!(recues.len(), 1);
        assert!(
            recues[0]["cible"]
                .as_str()
                .unwrap()
                .starts_with("POST /subscribe "),
            "l'abonnement doit taper /subscribe : {:?}",
            recues[0]["cible"]
        );
        assert_eq!(recues[0]["corps"]["instance_id"], "inst-42");
        assert_eq!(recues[0]["corps"]["artists"], json!(tous));
    }

    // -----------------------------------------------------------------------
    // `recuperer_concerts` — appelée par `concerts_a_venir`
    // -----------------------------------------------------------------------

    /// ⭐ La moitié LECTURE du 429. `tune-server/tests/concerts_plugin.rs` garde
    /// le RENDU (`reponse_de_refus`) en lui fabriquant un `CloudError` à la
    /// main ; rien ne vérifiait que la lecture en construit un. Un
    /// `CloudError::Message` rendu ici ferait un 200 parfaitement vert de
    /// l'autre côté.
    #[tokio::test]
    async fn un_429_du_nuage_arrive_en_refus_limite_avec_son_delai() {
        let banc = banc(vec![(429, r#"{"message":"Too Many Attempts."}"#, Some(42))]).await;

        let err = recuperer_concerts_depuis(&banc.racine, &reqwest::Client::new(), "inst-1")
            .await
            .unwrap_err();

        assert!(err.is_rate_limited(), "un 429 doit rester un 429 : {err:?}");
        assert_eq!(
            err.retry_after(),
            Some(42),
            "le delai annonce doit remonter"
        );
        assert_eq!(err.upstream(), Some("Too Many Attempts."));
    }

    /// Contre-épreuve : hors 429, rien ne bouge et surtout aucun délai n'est
    /// fabriqué — le banc en annonce un que le code doit ignorer.
    #[tokio::test]
    async fn contre_epreuve_un_refus_ordinaire_ne_fabrique_aucun_delai() {
        let banc = banc(vec![(500, r#"{"message":"boum"}"#, Some(42))]).await;

        let err = recuperer_concerts_depuis(&banc.racine, &reqwest::Client::new(), "inst-1")
            .await
            .unwrap_err();

        assert!(
            !err.is_rate_limited(),
            "un 500 n'est pas une limite : {err:?}"
        );
        assert_eq!(
            err.retry_after(),
            None,
            "hors 429, aucun delai ne doit etre fabrique"
        );
    }

    /// Le chemin nominal : l'identité de l'instance part en requête, et la
    /// liste rendue est celle du nuage.
    #[tokio::test]
    async fn la_lecture_porte_l_identite_de_l_instance_et_rend_la_liste() {
        let banc = banc(vec![(
            200,
            r#"{"concerts":[{"id":1},{"id":2},{"id":3}]}"#,
            None,
        )])
        .await;

        let concerts = recuperer_concerts_depuis(&banc.racine, &reqwest::Client::new(), "inst-7")
            .await
            .unwrap();

        assert_eq!(concerts.len(), 3);
        let cible = banc.recues()[0]["cible"].as_str().unwrap().to_string();
        assert!(
            cible.starts_with("GET /upcoming?") && cible.contains("instance_id=inst-7"),
            "l'identite doit partir en requete : {cible}"
        );
    }
}
