//! Tune Circle, étape T5 (#5328) : les playlists collaboratives, par
//! RÉFÉRENCES. Contrat du cloud : site-mozaiklabs#236.
//!
//! | Tune (`/api/v1/ext/circle`)                          | mozaiklabs (`/api/v1/circle`)                |
//! |------------------------------------------------------|----------------------------------------------|
//! | `GET /playlists`                                     | idem                                         |
//! | `POST /playlists` `{ circle_id, name }`              | idem                                         |
//! | `GET /playlists/{id}`                                | idem                                         |
//! | `PATCH /playlists/{id}` `{ name, version }`          | idem                                         |
//! | `DELETE /playlists/{id}`                             | idem                                         |
//! | `POST /playlists/{id}/items` `{ items?, track_ids?, service_tracks?, position?, version }` | `{ items, position?, version }` |
//! | `DELETE /playlists/{id}/items/{item_id}?version=`    | idem, requête comprise                       |
//! | `PUT /playlists/{id}/order` `{ item_ids, version }`  | idem                                         |
//! | `POST /playlists/{id}/resolve`                       | `GET /playlists/{id}`, résolu ICI            |
//! | `POST /playlists/{id}/play` `{ zone_id }`            | `GET /playlists/{id}`, résolu et joué ICI    |
//! | `GET /recoverable-playlists`                         | idem                                         |
//! | `GET /recoverable-playlists/{id}`                    | idem                                         |
//! | `DELETE /recoverable-playlists/{id}`                 | idem (« je n'en veux pas »)                  |
//! | `POST /recoverable-playlists/{id}/copy`              | `GET …/{id}`, copie locale ICI, puis `DELETE …/{id}` |
//!
//! * **Le droit est au cloud.** Chaque route repart vers lui ; rien n'est
//!   gardé. Un contact retiré ou révoqué reçoit du cloud un 404, relayé tel
//!   quel au premier appel qui suit — `resolve`, `play` et `copy` compris,
//!   qui relisent la playlist à chaque fois.
//! * **Seuls les champs du contrat partent** : `circle_id` et `name` à la
//!   création, `name` et `version` au renommage, `item_ids` et `version` à
//!   l'ordre ; `items`, `position` et `version` à l'ajout. `version` ne part
//!   que si le client l'a donnée ; sinon son en-tête `If-Match` part à sa
//!   place (le cloud accepte l'un ou l'autre). Le cloud juge la validité
//!   (422), la version (409 `version_conflict`, corps et `ETag` relayés) et
//!   la propriété (404).
//! * **Ajouter depuis Tune** : `track_ids` (pistes de la base, locales ou de
//!   service) et `service_tracks` (`[{ source, source_id }]`, un titre d'un
//!   service connecté) deviennent des RÉFÉRENCES construites ici
//!   ([`crate::references`]) — jamais un chemin, jamais un `source_id` local.
//!   Les `items` bruts du client partent tels quels : c'est le cloud qui
//!   refuse une clé hors de sa liste blanche.
//! * **Rejouer** : [`crate::resolution`], chez l'appelant, sans écrire chez
//!   aucun service.
//! * **Récupérer** (décisions 3 et 5 du 28/09) : la copie locale n'existe
//!   qu'à partir d'une playlist RÉCUPÉRABLE, c'est-à-dire d'un cercle
//!   supprimé. Aucune route ne copie une playlist vivante. L'archive n'est
//!   libérée qu'une fois tout copié ; une copie partielle se complète par
//!   un nouvel appel ([`PREFIXE_COPIE_EN_COURS`]).

use std::sync::Arc;

use axum::Json;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use reqwest::Method;
use serde_json::{Map, Value, json};
use tune_core::db::play_queue_repo::QueueInput;
use tune_core::db::playlist_repo::{EntryContent, PlaylistRepo, ServiceEntry};
use tune_core::db::settings_repo::SettingsRepo;

use crate::lecture::Lecture;
use crate::references::{self, Reference};
use crate::relais::{Issue, Relais};
use crate::resolution::{Resolveur, Trouvee, ligne};
use crate::routes::{en_reponse, identifiant_valide, introuvable, refus};

/// Une piste désignée pour l'ajout n'existe pas (ni en base, ni chez le
/// service) : rien n'est parti.
pub const CODE_PISTE_INCONNUE: &str = "circle.unknown_track";
/// Plus de [`AJOUTS_MAX`] morceaux dans un seul ajout : rien n'est parti.
pub const CODE_TROP_D_AJOUTS: &str = "circle.too_many_items";
/// `play` sans `zone_id` entier.
pub const CODE_ZONE_REQUISE: &str = "circle.zone_required";
/// `play` : aucun morceau de la playlist n'est jouable chez l'appelant.
pub const CODE_RIEN_A_JOUER: &str = "circle.nothing_playable";
/// `play` : la zone n'a pas pu lancer la lecture.
pub const CODE_LECTURE_ECHOUEE: &str = "circle.play_failed";
/// `copy` : la playlist locale n'a pas pu être écrite ; le droit de
/// récupération est gardé.
pub const CODE_COPIE_ECHOUEE: &str = "circle.copy_failed";

/// Plafond d'ajouts par requête, celui du contrat cloud (100 par requête) :
/// au-delà, aucune référence n'est construite ni cherchée chez un service.
pub const AJOUTS_MAX: usize = 100;

/// Profil par défaut, celui de `tune_http_types::DEFAULT_PROFILE_ID`.
const PROFIL_PAR_DEFAUT: i64 = 1;

/// Ce que les routes T5 tiennent : le relais, les sources de l'appelant, et
/// de quoi jouer.
pub struct Collaboratif {
    pub relais: Arc<Relais>,
    pub resolveur: Resolveur,
    pub lecture: Arc<dyn Lecture>,
}

pub fn router(etat: Arc<Collaboratif>) -> Router<()> {
    Router::new()
        .route("/playlists", get(lister).post(creer))
        .route(
            "/playlists/{id}",
            get(lire).patch(renommer).delete(supprimer),
        )
        .route("/playlists/{id}/items", post(ajouter))
        .route("/playlists/{id}/items/{item_id}", delete(retirer))
        .route("/playlists/{id}/order", put(ordonner))
        .route("/playlists/{id}/resolve", post(resoudre))
        .route("/playlists/{id}/play", post(jouer))
        .route("/recoverable-playlists", get(lister_recuperables))
        .route(
            "/recoverable-playlists/{id}",
            get(lire_recuperable).delete(renoncer),
        )
        .route("/recoverable-playlists/{id}/copy", post(copier))
        .with_state(etat)
}

fn lu(corps: &Bytes) -> Value {
    serde_json::from_slice::<Value>(corps).unwrap_or(Value::Null)
}

/// Les champs nommés du corps du client, `null` pour un champ absent ; le
/// champ `version`, lui, n'est écrit que s'il a été donné.
fn champs(corps: &Value, noms: &[&str]) -> Value {
    let mut sortie = Map::new();
    for nom in noms {
        let v = corps.get(*nom).cloned().unwrap_or(Value::Null);
        if *nom == "version" && v.is_null() {
            continue;
        }
        sortie.insert(nom.to_string(), v);
    }
    Value::Object(sortie)
}

fn if_match(entetes: &HeaderMap) -> Option<&axum::http::HeaderValue> {
    entetes.get(header::IF_MATCH)
}

async fn lister(State(e): State<Arc<Collaboratif>>) -> Response {
    en_reponse(
        e.relais
            .appeler("GET /playlists", Method::GET, &["playlists"], None)
            .await,
    )
}

async fn creer(State(e): State<Arc<Collaboratif>>, corps: Bytes) -> Response {
    let envoi = champs(&lu(&corps), &["circle_id", "name"]);
    en_reponse(
        e.relais
            .appeler(
                "POST /playlists",
                Method::POST,
                &["playlists"],
                Some(&envoi),
            )
            .await,
    )
}

async fn lire(State(e): State<Arc<Collaboratif>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    en_reponse(lire_la_playlist(&e.relais, &id).await)
}

async fn lire_la_playlist(relais: &Relais, id: &str) -> Issue {
    relais
        .appeler("GET /playlists/{id}", Method::GET, &["playlists", id], None)
        .await
}

async fn renommer(
    State(e): State<Arc<Collaboratif>>,
    Path(id): Path<String>,
    entetes: HeaderMap,
    corps: Bytes,
) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    let envoi = champs(&lu(&corps), &["name", "version"]);
    en_reponse(
        e.relais
            .appeler_avec(
                "PATCH /playlists/{id}",
                Method::PATCH,
                &["playlists", &id],
                None,
                Some(&envoi),
                if_match(&entetes),
            )
            .await,
    )
}

async fn supprimer(State(e): State<Arc<Collaboratif>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    en_reponse(
        e.relais
            .appeler(
                "DELETE /playlists/{id}",
                Method::DELETE,
                &["playlists", &id],
                None,
            )
            .await,
    )
}

fn piste_inconnue(pistes: Vec<Value>) -> Response {
    refus(
        StatusCode::UNPROCESSABLE_ENTITY,
        json!({ "code": CODE_PISTE_INCONNUE, "tracks": pistes }),
    )
}

/// Les références à ajouter : `items` bruts, puis `track_ids`, puis
/// `service_tracks`, dans cet ordre. `Err` : la réponse à rendre sans appel.
async fn references_a_ajouter(
    e: &Collaboratif,
    corps: &Value,
) -> Result<Vec<Value>, Box<Response>> {
    let brutes = corps["items"].as_array().cloned().unwrap_or_default();
    let track_ids = corps["track_ids"].as_array().cloned().unwrap_or_default();
    let de_service = corps["service_tracks"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if brutes.len() + track_ids.len() + de_service.len() > AJOUTS_MAX {
        // Le cloud refuserait de même (100 par requête) : pas de recherche
        // chez un service pour rien.
        return Err(Box::new(refus(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({ "code": CODE_TROP_D_AJOUTS, "max": AJOUTS_MAX }),
        )));
    }
    let mut sortie = brutes;
    let mut inconnues = Vec::new();
    for v in &track_ids {
        let Some(tid) = v.as_i64() else {
            inconnues.push(v.clone());
            continue;
        };
        match references::depuis_la_bibliotheque(e.resolveur.backend(), tid) {
            Ok(Some(r)) => sortie.push(r),
            _ => inconnues.push(v.clone()),
        }
    }
    for v in &de_service {
        let source = v["source"].as_str().unwrap_or_default();
        let source_id = match &v["source_id"] {
            Value::String(s) => s.trim().to_string(),
            Value::Number(n) => n.to_string(),
            _ => String::new(),
        };
        let trouvee = if source.is_empty() || source_id.is_empty() {
            None
        } else {
            e.resolveur.piste_du_service(source, &source_id).await
        };
        match trouvee.and_then(|p| references::depuis_le_service(source, &p)) {
            Some(r) => sortie.push(r),
            None => inconnues.push(json!({ "source": source, "source_id": source_id })),
        }
    }
    if !inconnues.is_empty() {
        return Err(Box::new(piste_inconnue(inconnues)));
    }
    Ok(sortie)
}

async fn ajouter(
    State(e): State<Arc<Collaboratif>>,
    Path(id): Path<String>,
    entetes: HeaderMap,
    corps: Bytes,
) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    let demande = lu(&corps);
    let items = match references_a_ajouter(&e, &demande).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let mut envoi = champs(&demande, &["version"]);
    envoi["items"] = json!(items);
    // `position`, facultative, part seulement si le client l'a donnée.
    if let Some(p) = demande.get("position").filter(|p| !p.is_null()) {
        envoi["position"] = p.clone();
    }
    en_reponse(
        e.relais
            .appeler_avec(
                "POST /playlists/{id}/items",
                Method::POST,
                &["playlists", &id, "items"],
                None,
                Some(&envoi),
                if_match(&entetes),
            )
            .await,
    )
}

async fn retirer(
    State(e): State<Arc<Collaboratif>>,
    Path((id, item_id)): Path<(String, String)>,
    RawQuery(requete): RawQuery,
    entetes: HeaderMap,
) -> Response {
    if !identifiant_valide(&id) || !identifiant_valide(&item_id) {
        return introuvable();
    }
    en_reponse(
        e.relais
            .appeler_avec(
                "DELETE /playlists/{id}/items/{item_id}",
                Method::DELETE,
                &["playlists", &id, "items", &item_id],
                requete.as_deref(),
                None,
                if_match(&entetes),
            )
            .await,
    )
}

async fn ordonner(
    State(e): State<Arc<Collaboratif>>,
    Path(id): Path<String>,
    entetes: HeaderMap,
    corps: Bytes,
) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    let envoi = champs(&lu(&corps), &["item_ids", "version"]);
    en_reponse(
        e.relais
            .appeler_avec(
                "PUT /playlists/{id}/order",
                Method::PUT,
                &["playlists", &id, "order"],
                None,
                Some(&envoi),
                if_match(&entetes),
            )
            .await,
    )
}

/// Une playlist relue au cloud, et chacun de ses morceaux résolu chez
/// l'appelant. `Err` : la réponse du cloud (404 compris) à relayer telle
/// quelle.
async fn resolue(e: &Collaboratif, issue: Issue) -> Result<(Value, Vec<Resolu>), Box<Response>> {
    let playlist = match &issue {
        Issue::Reponse {
            statut: 200, corps, ..
        } => serde_json::from_slice::<Value>(corps).ok(),
        _ => None,
    };
    let Some(playlist) = playlist.filter(|p| p["items"].is_array()) else {
        return Err(Box::new(en_reponse(issue)));
    };
    let services = e.resolveur.services_utilisables().await;
    let mut resolus = Vec::new();
    for item in playlist["items"].as_array().into_iter().flatten() {
        let trouvee = e
            .resolveur
            .resoudre(&Reference::lire(item), &services)
            .await;
        resolus.push((item["item_id"].clone(), trouvee));
    }
    Ok((playlist, resolus))
}

/// `(item_id, ce qu'il est devenu chez l'appelant)`.
type Resolu = (Value, Option<Trouvee>);

/// `[{ item_id, status, source, source_id, … }]`, dans l'ordre de la
/// playlist : la forme que lit l'écran (web#1731).
fn bilan(resolus: &[Resolu]) -> Value {
    Value::Array(resolus.iter().map(|(i, t)| ligne(i, t.as_ref())).collect())
}

fn manquants(resolus: &[Resolu]) -> Vec<Value> {
    resolus
        .iter()
        .filter(|(_, t)| t.is_none())
        .map(|(i, _)| i.clone())
        .collect()
}

async fn resoudre(State(e): State<Arc<Collaboratif>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    match resolue(&e, lire_la_playlist(&e.relais, &id).await).await {
        Ok((_, resolus)) => Json(bilan(&resolus)).into_response(),
        Err(r) => *r,
    }
}

fn en_file(trouvee: &Trouvee) -> Option<QueueInput> {
    match trouvee {
        Trouvee::Bibliotheque { piste, .. } => {
            piste.id.map(|track_id| QueueInput::Local { track_id })
        }
        Trouvee::Service { service, piste, .. } => Some(QueueInput::Streaming {
            source: service.clone(),
            source_id: piste.id.clone(),
            title: piste.title.clone(),
            artist: piste.artist.clone(),
            album: piste.album.clone(),
            cover_url: piste.cover_path.clone(),
            duration_ms: piste.duration_ms as i64,
            track_number: piste.track_number.map(i64::from),
            disc_number: piste.disc_number.map(i64::from),
            album_ref: piste.album_id.clone(),
        }),
    }
}

async fn jouer(
    State(e): State<Arc<Collaboratif>>,
    Path(id): Path<String>,
    corps: Bytes,
) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    let Some(zone_id) = lu(&corps)["zone_id"].as_i64() else {
        return refus(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({ "code": CODE_ZONE_REQUISE }),
        );
    };
    let (_, resolus) = match resolue(&e, lire_la_playlist(&e.relais, &id).await).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let rendu = bilan(&resolus);
    let manquants = manquants(&resolus);
    let file: Vec<QueueInput> = resolus
        .iter()
        .filter_map(|(_, t)| t.as_ref().and_then(en_file))
        .collect();
    if file.is_empty() {
        return refus(
            StatusCode::UNPROCESSABLE_ENTITY,
            json!({ "code": CODE_RIEN_A_JOUER, "missing": manquants, "resolution": rendu }),
        );
    }
    match e.lecture.jouer(zone_id, file).await {
        Ok(entrees) => Json(json!({
            "ok": true,
            "zone_id": zone_id,
            "queued": entrees,
            "missing": manquants,
            "resolution": rendu,
        }))
        .into_response(),
        Err(erreur) => {
            tracing::warn!(zone_id, error = %erreur, "circle_lecture_echouee");
            refus(
                StatusCode::BAD_GATEWAY,
                json!({ "code": CODE_LECTURE_ECHOUEE }),
            )
        }
    }
}

// Récupération d'une copie à la suppression du cercle -------------------------

async fn lister_recuperables(State(e): State<Arc<Collaboratif>>) -> Response {
    en_reponse(
        e.relais
            .appeler(
                "GET /recoverable-playlists",
                Method::GET,
                &["recoverable-playlists"],
                None,
            )
            .await,
    )
}

async fn lire_la_recuperable(relais: &Relais, id: &str) -> Issue {
    relais
        .appeler(
            "GET /recoverable-playlists/{id}",
            Method::GET,
            &["recoverable-playlists", id],
            None,
        )
        .await
}

async fn lire_recuperable(State(e): State<Arc<Collaboratif>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    en_reponse(lire_la_recuperable(&e.relais, &id).await)
}

async fn renoncer_au_cloud(relais: &Relais, id: &str) -> Issue {
    relais
        .appeler(
            "DELETE /recoverable-playlists/{id}",
            Method::DELETE,
            &["recoverable-playlists", id],
            None,
        )
        .await
}

async fn renoncer(State(e): State<Arc<Collaboratif>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    en_reponse(renoncer_au_cloud(&e.relais, &id).await)
}

/// Le profil où ranger la copie : celui que retient le serveur pour ce qui
/// n'a pas d'en-tête de profil (`active_profile_id`, puis le profil par
/// défaut), comme les playlists créées par un greffon WASM
/// (`plugins_host::profil_actif`).
fn profil_actif(e: &Collaboratif) -> i64 {
    SettingsRepo::with_backend(e.resolveur.backend().clone())
        .get("active_profile_id")
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|id| *id > 0)
        .unwrap_or(PROFIL_PAR_DEFAUT)
}

fn en_ligne_de_playlist(trouvee: &Trouvee) -> Option<EntryContent> {
    match trouvee {
        Trouvee::Bibliotheque { piste, .. } => piste.id.map(EntryContent::Local),
        Trouvee::Service { service, piste, .. } => Some(EntryContent::Service(ServiceEntry {
            source: service.clone(),
            source_id: piste.id.clone(),
            title: piste.title.clone(),
            artist: Some(piste.artist.clone()).filter(|a| !a.is_empty()),
            album: piste.album.clone(),
            album_source_id: piste.album_id.clone(),
            duration_ms: Some(piste.duration_ms as i64),
            cover_url: piste.cover_path.clone(),
        })),
    }
}

/// Préfixe du réglage qui relie une playlist récupérable du cloud à la copie
/// locale déjà commencée : `circle_copie_recuperable:{id de l'archive}` →
/// identifiant de la playlist locale.
///
/// C'est le SEUL état que T5 garde ici, et il ne dit rien du cercle : deux
/// identifiants, pour qu'une seconde copie complète la même playlist au lieu
/// d'en créer une autre. Il vit dans `settings`, comme le stockage des
/// greffons WASM, et disparaît quand l'archive est libérée. Il ne sert rien
/// à lui seul : la copie relit toujours l'archive au cloud (404 si le droit
/// est perdu), et l'échéance de 30 jours reste celle du cloud.
pub const PREFIXE_COPIE_EN_COURS: &str = "circle_copie_recuperable:";

fn cle_de_copie(archive: &str) -> String {
    format!("{PREFIXE_COPIE_EN_COURS}{archive}")
}

/// La copie locale déjà commencée pour cette archive, si elle existe encore
/// dans le profil (l'utilisateur a pu la supprimer entre-temps).
fn copie_en_cours(e: &Collaboratif, archive: &str, profil: i64) -> Option<i64> {
    let id = SettingsRepo::with_backend(e.resolveur.backend().clone())
        .get(&cle_de_copie(archive))
        .ok()
        .flatten()?
        .trim()
        .parse::<i64>()
        .ok()?;
    PlaylistRepo::with_backend(e.resolveur.backend().clone())
        .get_for_profile(id, profil)
        .ok()
        .flatten()
        .map(|_| id)
}

/// `POST /recoverable-playlists/{id}/copy` : la playlist d'un cercle
/// supprimé devient une playlist LOCALE de l'appelant, sans lien retour.
/// Chaque morceau est résolu chez lui comme pour `play`.
///
/// Décision de Bertrand (28/09) : l'archive n'est PAS libérée tant qu'un
/// morceau reste introuvable. Une copie partielle garde l'archive
/// (`released: false`, la liste dans `not_copied`) ; une copie suivante — par
/// exemple après avoir branché un service — COMPLÈTE la même playlist locale
/// (ajout sans doublon, en fin de liste) et libère l'archive (`DELETE` du
/// cloud) quand plus rien ne manque. Le droit n'est jamais rendu si
/// l'écriture locale a échoué.
async fn copier(State(e): State<Arc<Collaboratif>>, Path(id): Path<String>) -> Response {
    if !identifiant_valide(&id) {
        return introuvable();
    }
    let (playlist, resolus) = match resolue(&e, lire_la_recuperable(&e.relais, &id).await).await {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let nom = playlist["name"]
        .as_str()
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .unwrap_or("Playlist du cercle")
        .to_string();
    let lignes: Vec<EntryContent> = resolus
        .iter()
        .filter_map(|(_, t)| t.as_ref().and_then(en_ligne_de_playlist))
        .collect();
    let profil = profil_actif(&e);
    let repo = PlaylistRepo::with_backend(e.resolveur.backend().clone());
    let reglages = SettingsRepo::with_backend(e.resolveur.backend().clone());
    let existante = copie_en_cours(&e, &id, profil);
    let ecrite = match existante {
        Some(pid) => repo
            .add_entries_deduped(pid, &lignes, None)
            .map(|ajoutees| (pid, ajoutees)),
        None => repo.create_with_entries(&nom, None, profil, &lignes),
    };
    let (playlist_id, ecrites) = match ecrite {
        Ok(v) => v,
        Err(erreur) => {
            tracing::warn!(error = %erreur, "circle_copie_locale_echouee");
            return refus(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "code": CODE_COPIE_ECHOUEE }),
            );
        }
    };
    // Ce qui n'a pas pu entrer dans la copie : une playlist locale ne porte
    // que des pistes de la bibliothèque ou des titres de service.
    let non_copies: Vec<Value> = resolus
        .iter()
        .zip(playlist["items"].as_array().into_iter().flatten())
        .filter(|((_, t), _)| t.is_none())
        .map(|(_, item)| {
            json!({
                "item_id": item["item_id"],
                "title": item["title"],
                "artist_name": item["artist_name"],
            })
        })
        .collect();
    let rendu_au_cloud = if non_copies.is_empty() {
        let rendu = renoncer_au_cloud(&e.relais, &id).await;
        let ok = matches!(&rendu, Issue::Reponse { statut, .. } if (200..300).contains(statut));
        if ok {
            reglages.delete(&cle_de_copie(&id)).ok();
        } else {
            tracing::warn!(playlist_id, "circle_droit_de_recuperation_non_rendu");
            reglages
                .set(&cle_de_copie(&id), &playlist_id.to_string())
                .ok();
        }
        ok
    } else {
        // Archive gardée : le lien vers la copie permet de la compléter.
        reglages
            .set(&cle_de_copie(&id), &playlist_id.to_string())
            .ok();
        false
    };
    let creee = repo.get(playlist_id).ok().flatten();
    Json(json!({
        "ok": true,
        // La playlist locale, à la forme des routes `/playlists`.
        "playlist": creee,
        "playlist_id": playlist_id,
        "name": nom,
        // `true` : cet appel a complété une copie commencée plus tôt.
        "completed_existing": existante.is_some(),
        // Lignes écrites par CET appel.
        "copied": ecrites.len(),
        "missing": manquants(&resolus),
        "not_copied": non_copies,
        "resolution": bilan(&resolus),
        // `false` : l'archive est gardée au cloud — des morceaux restent
        // introuvables (`not_copied`), ou le cloud n'a pas répondu. Une
        // nouvelle copie complétera la même playlist.
        "released": rendu_au_cloud,
    }))
    .into_response()
}
