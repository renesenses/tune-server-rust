//! Les concerts des artistes de la bibliothèque, en [`TunePlugin`] (#2363).
//!
//! « Les artistes que j'écoute jouent-ils près de chez moi ? » — la demande de
//! FabienM et Didier (forum, fil 1540). Le greffon :
//!
//! - abonne, toutes les 24 h, les artistes de la bibliothèque auprès du nuage
//!   (`mozaiklabs.fr/api/v1/premium/concerts/subscribe`) ;
//! - relaie la commune SAISIE par l'utilisateur et le périmètre voulu
//!   (`…/concerts/location`, site-mozaiklabs#186) ;
//! - lit les dates à venir dans ce périmètre (`…/concerts/upcoming`). Les
//!   dates viennent de l'agenda Ticketmaster, rapatrié par le nuage depuis le
//!   30/08/2026 (site-mozaiklabs#185, #189).
//!
//! # Les routes
//!
//! Montées par l'hôte sous `/api/v1/ext/concerts` — le préfixe vient de
//! `name()`, un greffon ne choisit jamais le sien. Le contrat est celui que
//! l'écran `ConcertsView.svelte` (tune-web-client#695) consomme déjà :
//!
//! | Route | 200 |
//! |---|---|
//! | `GET /upcoming` | `{concerts, scope?, radius_km?, city?, country?, code?}` |
//! | `POST /location` | `{scope, city, country, radius_km, located}` |
//! | `GET /location` | la dernière localisation enregistrée, même forme |
//!
//! Le périmètre de `/upcoming` est celui que le NUAGE a appliqué : il le garde
//! par instance (`concert_locations`) et l'applique lui-même à la lecture. Le
//! greffon n'envoie donc que l'identité de l'instance, et remonte à l'écran
//! le périmètre rendu — sans lui, l'écran affiche « 3 concerts » sans pouvoir
//! dire « à moins de 100 km de Dijon », ni proposer d'élargir.
//!
//! Trois crans, jamais un filtre binaire (arbitrage du 29/08) : rayon de 50,
//! 100 ou 200 km autour de la commune, le pays, ou partout. **Pays** par
//! défaut quand l'écran n'en dit rien. Tant qu'aucune localisation n'a été
//! enregistrée, le nuage ne filtre pas du tout (`world`) : il ne cache pas de
//! concerts au nom d'un lieu que personne n'a choisi.
//!
//! **La commune est SAISIE, jamais déduite.** Le nuage connaît des coordonnées
//! tirées de l'adresse IP (`tune_instances.latitude`) : elles ne servent pas
//! ici. Une IP désigne la sortie du fournisseur d'accès — derrière un VPN, un
//! autre pays.
//!
//! # Premium, décidé ICI et pas par l'hôte
//!
//! « Concerts » est un module à lui, inclus dans Premium, sans achat séparé
//! (Bertrand, 30/08 et 25/09/2026). Deux raccourcis sont interdits, parce
//! qu'une version RÉDUITE gratuite viendra plus tard :
//!
//! 1. pas de `require_premium` écrit en dur sur un chemin — le payant est une
//!    propriété du GREFFON ([`ConcertsPlugin::required_feature`]) ;
//! 2. pas de garde monté par l'hôte devant les routes — il ne saurait
//!    qu'ouvrir ou fermer.
//!
//! La licence descend donc jusqu'ici (`PluginContext::license`) et la décision
//! tient dans UNE fonction, [`acces`], qui rend [`Acces`]. Le jour de la
//! version réduite, `Reduit` s'ajoute à l'énumération et aux `match` qui la
//! lisent — le compilateur les désigne tous ; ni l'hôte ni le montage ne
//! bougent.
//!
//! Le refus est un `ModuleRefusal` (402, `error: "module_required"`, `code`
//! `module_account_not_linked` ou `module_not_owned`, `module: "concerts"`) :
//! l'idiome des refus de module du serveur, que le client web reconnaît comme
//! un refus d'offre et non comme une panne ([`refus_du_module`]). Il porte sur les ROUTES, jamais sur le chargement : un
//! compte gratuit peut installer le greffon, et l'écran lui dit pourquoi il
//! reste verrouillé.
//!
//! # L'extraction a été rebasée sur #2892, pas sur la version d'avant
//!
//! Ce greffon a d'abord été un portage littéral de `concert_alerts.rs` **tel
//! qu'il était le 29/08**. Le 30/08, #2892 a réécrit ce même fichier dans la
//! ligne de release : l'abonnement porte désormais sur TOUTE la bibliothèque
//! et non plus sur les seuls artistes identifiés par un MusicBrainz ID. Prendre
//! la suppression à la fusion aurait annulé #2892 **sans qu'aucun test ne
//! rougisse**. Le comportement est donc porté ici, et gardé par des tests
//! (`tune-server/tests/concerts_plugin.rs`) qui portent sur le fait de base :
//! un artiste sans MBID est abonné comme les autres.
//!
//! # Et l'apport de #2178, porté à la fusion de `rc/v0.9.130`
//!
//! Même piège, une seconde fois : « un 429 du nuage dit la limite et le délai,
//! partout ». Le comportement est porté ([`reponse_de_refus`]) et gardé par des
//! tests qui portent sur le fait de base : un 429 du nuage arrive au client
//! **en 429, avec son délai**.

use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Map, Value, json};
use tracing::{debug, info, warn};

use tune_core::cloud::refusal::CloudError;
use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::event_bus::TuneEvent;
use tune_core::license::{Feature, LicenseManager, ModuleRefusal};
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

const CONCERTS_API: &str = "https://mozaiklabs.fr/api/v1/premium/concerts";

/// Le réglage où la dernière localisation ACCEPTÉE par le nuage est gardée.
///
/// Le nuage n'a pas de route de lecture de la localisation : sans cette copie,
/// `GET /location` n'aurait rien à rendre. Elle n'est écrite qu'après un
/// succès du nuage — elle dit ce que le nuage applique, pas ce qu'on a tenté.
pub const CLE_LOCALISATION: &str = "plugin_concerts_localisation";

/// Les trois crans, dans les mots du nuage (`PerimetreConcerts::valeurs()`).
pub const PERIMETRES: [&str; 3] = ["radius", "country", "world"];

/// Le cran appliqué quand l'écran n'en dit rien — `PerimetreConcerts::DEFAUT`.
pub const PERIMETRE_PAR_DEFAUT: &str = "country";

/// Les rayons, liste FERMÉE et identique au nuage (`PerimetreConcerts::RAYONS`)
/// et au client (`RAYONS_CONCERTS`) : un rayon libre serait un « partout »
/// déguisé. Le nuage refuse tout autre valeur par un 422.
pub const RAYONS_KM: [i64; 3] = [50, 100, 200];

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
/// `tune-server/src/plugins.rs`. La licence, elle, arrive par le contexte
/// (`PluginContext::license`) : c'est le chemin commun à tout greffon payant.
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
        "Concerts (Premium) : les dates à venir des artistes de votre bibliothèque, autour de chez vous"
    }

    /// Opt-in, comme `dj`, `karaoke` et `bandcamp`.
    fn default_enabled(&self) -> bool {
        false
    }

    /// Au catalogue depuis que l'écran existe (tune-web-client#695,
    /// `ConcertsView.svelte`) et que le nuage a des dates : les deux raisons
    /// qui le tenaient dehors (#2090) sont tombées. Dit explicitement plutôt
    /// que laissé au défaut, pour qu'un retour en arrière soit un acte visible.
    fn catalogued(&self) -> bool {
        true
    }

    /// Le module Premium auquel ce greffon appartient. Le gestionnaire s'en
    /// sert pour montrer le cadenas avant le clic ; le comportement réel se
    /// décide dans [`acces`].
    fn required_feature(&self) -> Option<Feature> {
        Some(Feature::Concerts)
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        let license = ctx.license.clone();
        if !ctx.feature_licensed(Feature::Concerts).await {
            // Chargé quand même : le refus porte sur les routes, et l'écran
            // doit pouvoir dire pourquoi il reste verrouillé.
            info!("concerts_charge_sans_premium");
        }
        ctx.register_router(router(self.backend.clone(), license.clone()));
        self.tache = Some(lancer_synchronisation(self.backend.clone(), license));
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
// Le droit — la SEULE décision du payant
// ---------------------------------------------------------------------------

/// Ce que ce serveur a le droit de recevoir du module, selon sa licence.
///
/// La variante manquante est `Reduit` : la version limitée des comptes
/// gratuits, prévue une fois le module abouti. Elle s'ajoutera ici, et le
/// compilateur désignera les `match` à compléter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acces {
    Complet,
    Refuse,
}

/// LE point de décision du payant. Publique pour être observable depuis
/// `tune-server/tests/concerts_plugin.rs`.
///
/// ⚠️ Une licence ABSENTE ne vaut pas une autorisation. C'est le cas d'un hôte
/// qui n'en fournit pas — tests, `tune-cli`, ou une construction future qui
/// oublierait de la brancher. Interpréter l'absence en faveur du doute
/// ouvrirait le module à tout le monde le jour où quelqu'un déplace une ligne.
pub async fn acces(license: Option<&LicenseManager>) -> Acces {
    match license {
        Some(l) if l.check_feature(Feature::Concerts).await => Acces::Complet,
        _ => Acces::Refuse,
    }
}

/// Le nom du module dans les refus — le même que `name()` et que le chemin.
pub const MODULE: &str = "concerts";

/// Un compte mozaiklabs est-il lié à ce serveur ? Même lecture que
/// `discovery_setup::compte_mozaik_lie` (tune-server) : un jeton stocké.
pub fn compte_lie(backend: &Arc<dyn DbBackend>) -> bool {
    SettingsRepo::with_backend(backend.clone())
        .get("mozaik_access_token")
        .ok()
        .flatten()
        .is_some_and(|t| !t.is_empty())
}

/// Le refus du module, dans la forme de [`ModuleRefusal`] — l'idiome des
/// refus de module du serveur (#2392) : `error: "module_required"`, et un
/// `code` qui nomme la raison et le geste attendu :
///
/// - `module_account_not_linked` (`action: link_account`) : aucun compte lié,
///   le droit Premium ne peut pas parvenir au serveur ;
/// - `module_not_owned` (`action: purchase_module`) : compte lié, sans Premium.
///
/// Statut **402** : celui de tous les refus d'offre du serveur. Le client web
/// le reconnaît deux fois — par le corps (`module_required`) et par le
/// statut (`estRefusPremium`) —, jamais comme une panne. `message` est le
/// repli anglais de `ModuleRefusal`, jamais destiné à l'affichage : le client
/// traduit le `code`.
pub fn refus_du_module(raison: ModuleRefusal) -> Response {
    let mut corps = raison.to_json(MODULE);
    if let Some(objet) = corps.as_object_mut() {
        // Le droit qui ouvre le module, pour l'écran qui voudrait le nommer.
        objet.insert("feature".into(), json!(Feature::Concerts.code()));
    }
    (StatusCode::PAYMENT_REQUIRED, Json(corps)).into_response()
}

// ---------------------------------------------------------------------------
// Routes — montées par l'hôte sous /api/v1/ext/concerts
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct EtatConcerts {
    backend: Arc<dyn DbBackend>,
    license: Option<Arc<LicenseManager>>,
    /// La racine de l'API du nuage : [`CONCERTS_API`] en production, un banc
    /// local dans les essais ([`router_vers`]).
    racine: Arc<str>,
}

impl EtatConcerts {
    /// Le refus à rendre, ou `None` si la route peut servir.
    ///
    /// Toutes les routes passent par ici, et par rien d'autre : une route
    /// ajoutée sans son portillon est l'erreur classique — la lecture refuse,
    /// l'écriture passe.
    async fn refus(&self) -> Option<Response> {
        match acces(self.license.as_deref()).await {
            Acces::Complet => None,
            Acces::Refuse => {
                // `evaluate(false, …)` rend toujours une raison ; `NotOwned` n'est
                // qu'un filet si la règle venait à changer.
                let raison = ModuleRefusal::evaluate(false, compte_lie(&self.backend))
                    .unwrap_or(ModuleRefusal::NotOwned);
                info!(code = raison.code(), "concerts_refuse_sans_premium");
                Some(refus_du_module(raison))
            }
        }
    }

    fn settings(&self) -> SettingsRepo {
        SettingsRepo::with_backend(self.backend.clone())
    }

    fn instance_id(&self) -> String {
        self.settings()
            .get("instance_id")
            .ok()
            .flatten()
            .unwrap_or_default()
    }
}

/// Le routeur de production, qui parle à `mozaiklabs.fr`.
pub fn router(backend: Arc<dyn DbBackend>, license: Option<Arc<LicenseManager>>) -> Router<()> {
    router_vers(CONCERTS_API, backend, license)
}

/// Le même routeur, vers une autre racine d'API — le seul moyen d'exercer les
/// routes de bout en bout sans appeler `mozaiklabs.fr` depuis un essai.
/// L'unique appelant en production est [`router`], juste au-dessus.
pub fn router_vers(
    racine: &str,
    backend: Arc<dyn DbBackend>,
    license: Option<Arc<LicenseManager>>,
) -> Router<()> {
    Router::new()
        .route("/upcoming", get(concerts_a_venir))
        .route("/location", get(lire_localisation).post(poser_localisation))
        .with_state(EtatConcerts {
            backend,
            license,
            racine: Arc::from(racine),
        })
}

fn client_du_nuage() -> Result<reqwest::Client, reqwest::Error> {
    tune_core::http::client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
}

/// `GET /api/v1/ext/concerts/upcoming` — remplace `GET /system/concerts`.
///
/// Le corps d'erreur de l'ancienne route était une chaîne technique anglaise
/// (`{"concerts": [], "error": "concerts: HTTP 500"}`) qu'une interface
/// traduite en 11 langues aurait affichée telle quelle. On rend désormais un
/// **code stable**, traduisible côté client, et le détail part au journal.
async fn concerts_a_venir(State(etat): State<EtatConcerts>) -> Response {
    if let Some(refus) = etat.refus().await {
        return refus;
    }

    let instance_id = etat.instance_id();
    if instance_id.is_empty() {
        return Json(json!({"concerts": [], "code": "concerts.no_instance_id"})).into_response();
    }

    let client = match client_du_nuage() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "concerts_client_build_failed");
            return Json(json!({"concerts": [], "code": "concerts.unavailable"})).into_response();
        }
    };

    match recuperer_concerts_depuis(&etat.racine, &client, &instance_id).await {
        Ok(corps) => Json(corps).into_response(),
        Err(e) => {
            warn!(error = %e, retry_after = ?e.retry_after(), "concerts_fetch_failed");
            reponse_de_refus(&e)
        }
    }
}

/// `GET /api/v1/ext/concerts/location` — la localisation que le nuage applique.
///
/// Rien d'enregistré : le nuage ne filtre pas (`world`), et on le dit par un
/// code. Les champs restent des chaînes (vides) et le rayon vaut celui du
/// nuage par défaut (100 km) : l'écran pré-remplit son formulaire avec, et un
/// `null` y casserait un `.trim()`.
async fn lire_localisation(State(etat): State<EtatConcerts>) -> Response {
    if let Some(refus) = etat.refus().await {
        return refus;
    }
    let gardee = etat
        .settings()
        .get(CLE_LOCALISATION)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(Value::is_object);
    match gardee {
        Some(localisation) => Json(localisation).into_response(),
        None => Json(json!({
            "scope": "world",
            "city": "",
            "postal_code": null,
            "country": "",
            "radius_km": 100,
            "code": "concerts.no_location",
        }))
        .into_response(),
    }
}

/// `POST /api/v1/ext/concerts/location` — la commune SAISIE et le périmètre.
///
/// Corps attendu, celui de `setLocalisationConcerts` (tune-web-client) :
/// `{city, postal_code?, country, scope, radius_km?}`. Validé ICI avant tout
/// appel, avec les règles du nuage ([`valider_localisation`]) : un 422 du nuage
/// ne dirait pas quel champ est en cause.
///
/// Le corps est lu brut, pas par l'extracteur `Json` : un corps illisible doit
/// d'abord passer le portillon, puis finir en 422 à code — pas en rejet
/// texte d'axum avant même la question du droit.
///
/// Réponses : 200 (la réponse du nuage), 402 (sans Premium), 422
/// `concerts.invalid_location` (+ `field`), 409 `concerts.no_instance_id`,
/// 429 `concerts.rate_limited` (+ délai), 502 `concerts.unavailable`. Tout
/// échec est un statut d'erreur : l'écran, qui ne regarde que les exceptions
/// sur cette route, prendrait sinon un corps sans `scope` pour un succès.
async fn poser_localisation(State(etat): State<EtatConcerts>, corps: Bytes) -> Response {
    if let Some(refus) = etat.refus().await {
        return refus;
    }

    let demande: Value = serde_json::from_slice(&corps).unwrap_or(Value::Null);
    let demande = match valider_localisation(&demande) {
        Ok(d) => d,
        Err(champ) => return localisation_invalide(Some(champ)),
    };

    let instance_id = etat.instance_id();
    if instance_id.is_empty() {
        return (
            StatusCode::CONFLICT,
            Json(json!({"code": "concerts.no_instance_id"})),
        )
            .into_response();
    }

    let client = match client_du_nuage() {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "concerts_client_build_failed");
            return refus_du_nuage(&CloudError::from(e.to_string()), false);
        }
    };

    match enregistrer_localisation_vers(&etat.racine, &client, &instance_id, &demande).await {
        Ok(rendu) => {
            let mut gardee = rendu.clone();
            if let (Some(objet), Some(cp)) = (gardee.as_object_mut(), demande.get("postal_code")) {
                objet.insert("postal_code".into(), cp.clone());
            }
            if let Err(e) = etat.settings().set(CLE_LOCALISATION, &gardee.to_string()) {
                // Le nuage a accepté : c'est lui qui filtre. La copie locale ne
                // sert qu'à `GET /location` — la perdre ne défait rien.
                warn!(error = %e, "concerts_localisation_non_gardee");
            }
            info!(scope = ?rendu.get("scope"), located = ?rendu.get("located"), "concerts_localisation_enregistree");
            Json(rendu).into_response()
        }
        Err(EchecLocalisation::Refusee) => localisation_invalide(None),
        Err(EchecLocalisation::Nuage(e)) => {
            warn!(error = %e, retry_after = ?e.retry_after(), "concerts_location_failed");
            refus_du_nuage(&e, false)
        }
    }
}

fn localisation_invalide(champ: Option<&'static str>) -> Response {
    let mut corps = Map::new();
    corps.insert("code".into(), json!("concerts.invalid_location"));
    if let Some(champ) = champ {
        corps.insert("field".into(), json!(champ));
    }
    (StatusCode::UNPROCESSABLE_ENTITY, Json(Value::Object(corps))).into_response()
}

/// Valide et normalise une demande de localisation, avec les règles EXACTES
/// de la route du nuage (`POST /concerts/location`, site-mozaiklabs#186) :
///
/// - `city` : obligatoire, 255 caractères au plus ;
/// - `postal_code` : facultatif, 16 au plus ;
/// - `country` : obligatoire, 5 au plus — mis en majuscules ;
/// - `scope` : `radius` | `country` | `world`, [`PERIMETRE_PAR_DEFAUT`] si absent ;
/// - `radius_km` : 50, 100 ou 200 s'il est donné.
///
/// Rend le corps à relayer, ou le nom du champ fautif. Aucun `instance_id` :
/// celui qui part au nuage est TOUJOURS celui de ce serveur.
pub fn valider_localisation(demande: &Value) -> Result<Value, &'static str> {
    fn texte<'a>(demande: &'a Value, cle: &str) -> Option<&'a str> {
        demande
            .get(cle)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    let city = texte(demande, "city").ok_or("city")?;
    if city.chars().count() > 255 {
        return Err("city");
    }
    let country = texte(demande, "country").ok_or("country")?.to_uppercase();
    if country.chars().count() > 5 {
        return Err("country");
    }
    let postal_code = texte(demande, "postal_code");
    if postal_code.is_some_and(|cp| cp.chars().count() > 16) {
        return Err("postal_code");
    }
    let scope = match demande.get("scope") {
        None | Some(Value::Null) => PERIMETRE_PAR_DEFAUT,
        Some(v) => v
            .as_str()
            .filter(|s| PERIMETRES.contains(s))
            .ok_or("scope")?,
    };
    let radius_km = match demande.get("radius_km") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_i64()
                .filter(|km| RAYONS_KM.contains(km))
                .ok_or("radius_km")?,
        ),
    };

    let mut corps = Map::new();
    corps.insert("city".into(), json!(city));
    corps.insert("postal_code".into(), json!(postal_code));
    corps.insert("country".into(), json!(country));
    corps.insert("scope".into(), json!(scope));
    if let Some(km) = radius_km {
        corps.insert("radius_km".into(), json!(km));
    }
    Ok(Value::Object(corps))
}

/// Rend un refus du nuage **sans en perdre le motif** — la forme greffon de
/// `routes::cloud_error::reponse` (#2178), pour la lecture des concerts.
///
/// # Pourquoi ce n'est pas un appel à la fabrique commune
///
/// `tune-server/src/routes/cloud_error.rs` rend ce contrat pour les
/// gestionnaires du cœur. Ce greffon **ne peut pas l'appeler** : il dépend de
/// `tune-core`, jamais de `tune-server` — l'inverse ferait un cycle, puisque
/// c'est `tune-server` qui monte ce routeur. Ce qui est partagé l'est au bon
/// niveau : le **type** du refus, [`CloudError`], et la lecture du délai
/// (`cloud::rate_limit::retry_after_secs`), tous deux dans `tune-core`.
///
/// Le greffon ne traduit pas : il rend un **code stable** que le client
/// traduit. Le reste du contrat est tenu mot pour mot :
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
/// de l'autre côté de la frontière de crate.
pub fn reponse_de_refus(err: &CloudError) -> Response {
    refus_du_nuage(err, true)
}

/// Le rendu commun d'un refus du nuage. `lecture` = la route `/upcoming`, qui
/// garde l'enveloppe `{"concerts": []}` et répond 200 hors 429 (contrat
/// historique). L'écriture (`/location`) répond 502 hors 429 : un succès
/// apparent y serait lu comme « localisation enregistrée ».
fn refus_du_nuage(err: &CloudError, lecture: bool) -> Response {
    let CloudError::RateLimited {
        retry_after,
        upstream,
        ..
    } = err
    else {
        if lecture {
            return Json(json!({"concerts": [], "code": "concerts.unavailable"})).into_response();
        }
        return (
            StatusCode::BAD_GATEWAY,
            Json(json!({"code": "concerts.unavailable"})),
        )
            .into_response();
    };

    let mut corps = Map::new();
    if lecture {
        corps.insert("concerts".into(), json!([]));
    }
    corps.insert("code".into(), json!("concerts.rate_limited"));
    if let Some(secs) = retry_after {
        corps.insert("retry_after".into(), json!(secs));
    }
    if !upstream.is_empty() {
        corps.insert("upstream_message".into(), json!(upstream));
    }

    let mut resp = (StatusCode::TOO_MANY_REQUESTS, Json(Value::Object(corps))).into_response();
    if let Some(secs) = retry_after
        && let Ok(v) = header::HeaderValue::from_str(&secs.to_string())
    {
        resp.headers_mut().insert(header::RETRY_AFTER, v);
    }
    resp
}

// ---------------------------------------------------------------------------
// Le cloud — repris de tune-core/src/cloud/concert_alerts.rs, dans son état
// après #2892 (40f9342c) : l'abonnement porte sur toute la bibliothèque.
// La lecture (`recuperer_concerts_depuis`) a reçu l'apport de #2178
// (64e8378f) : elle rend un `CloudError`, et un 429 du nuage arrive au client
// en 429. Puis site-mozaiklabs#186 : la localisation et le périmètre.
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

/// La lecture des concerts à venir, avec la racine de l'API en argument —
/// même raison que [`envoyer_abonnements`] : c'est le seul moyen d'exercer la
/// traduction d'un refus du nuage en [`CloudError`] sans appeler
/// `mozaiklabs.fr`. Rend le corps destiné à l'écran ([`corps_a_venir`]).
///
/// Le refus est rendu en [`CloudError`] et non en `String` (#2178) : un 429 y
/// garde son délai (`Retry-After`, à défaut `X-RateLimit-Reset`) et le texte du
/// distant, que [`reponse_de_refus`] fait ressortir jusqu'au client.
///
/// Seule l'identité de l'instance part : le périmètre est gardé et appliqué
/// par le nuage lui-même (`concert_locations`), qui le rend dans sa réponse.
pub async fn recuperer_concerts_depuis(
    racine: &str,
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<Value, CloudError> {
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
    let corps = corps_a_venir(&data);
    info!(
        count = corps["concerts"].as_array().map_or(0, Vec::len),
        scope = ?corps.get("scope"),
        "upcoming_concerts_fetched"
    );
    Ok(corps)
}

/// Le corps rendu à l'écran : la liste, et le périmètre que le nuage a
/// APPLIQUÉ (`scope`, `radius_km`, `city`, `country`, site-mozaiklabs#186).
///
/// Champs nommés un à un plutôt que le corps du nuage relayé tel quel : ce que
/// l'écran reçoit est le contrat de CE serveur (`ConcertsAVenir`,
/// `docs/contrat-web.json`), pas ce que le nuage ajoutera demain. Un champ nul
/// est omis — le client les déclare facultatifs.
pub fn corps_a_venir(data: &Value) -> Value {
    let mut corps = Map::new();
    corps.insert(
        "concerts".into(),
        data.get("concerts")
            .filter(|c| c.is_array())
            .cloned()
            .unwrap_or_else(|| json!([])),
    );
    for champ in ["scope", "radius_km", "city", "country"] {
        if let Some(v) = data.get(champ).filter(|v| !v.is_null()) {
            corps.insert(champ.into(), v.clone());
        }
    }
    Value::Object(corps)
}

/// Pourquoi une localisation n'a pas été enregistrée.
#[derive(Debug)]
pub enum EchecLocalisation {
    /// Le nuage a refusé la demande elle-même (422) : commune, pays, cran ou
    /// rayon hors de ses règles. Distingué d'une panne : réessayer la même
    /// demande ne donnera rien.
    Refusee,
    /// Réseau, limite (429), panne du nuage, réponse illisible.
    Nuage(CloudError),
}

/// Enregistre la commune SAISIE et le périmètre auprès du nuage
/// (`POST {racine}/location`). Rend ce que le nuage a retenu :
/// `{scope, city, country, radius_km, located}`.
///
/// ⚠️ L'`instance_id` est IMPOSÉ ici, par-dessus la demande : c'est celui de
/// CE serveur, jamais celui qu'un client prétendrait — sinon n'importe qui
/// poserait la commune d'une autre instance.
///
/// `located == false` : le rayon était demandé mais le géocodeur n'a pas
/// trouvé la commune ; le nuage retombe alors sur le pays. L'écran le dit.
pub async fn enregistrer_localisation_vers(
    racine: &str,
    http_client: &reqwest::Client,
    instance_id: &str,
    demande: &Value,
) -> Result<Value, EchecLocalisation> {
    let mut corps = demande.clone();
    if let Some(objet) = corps.as_object_mut() {
        objet.insert("instance_id".into(), json!(instance_id));
    }

    let resp = http_client
        .post(format!("{racine}/location"))
        .json(&corps)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| EchecLocalisation::Nuage(format!("concerts location: {e}").into()))?;

    let status = resp.status();
    if status.as_u16() == 422 {
        let detail = resp.text().await.unwrap_or_default();
        warn!(detail = %detail.chars().take(300).collect::<String>(), "concerts_location_refusee_par_le_nuage");
        return Err(EchecLocalisation::Refusee);
    }
    if !status.is_success() {
        return Err(EchecLocalisation::Nuage(
            CloudError::from_response(format!("concerts location: HTTP {status}"), resp).await,
        ));
    }

    let data: Value = resp
        .json()
        .await
        .map_err(|e| EchecLocalisation::Nuage(format!("parse: {e}").into()))?;
    let mut rendu = Map::new();
    for champ in ["scope", "city", "country", "radius_km", "located"] {
        if let Some(v) = data.get(champ) {
            rendu.insert(champ.into(), v.clone());
        }
    }
    Ok(Value::Object(rendu))
}

/// Le greffon a-t-il le droit d'envoyer la liste des artistes au nuage ?
///
/// Deux conditions, et plus `community_sync_enabled` :
///
/// 1. **Premium** — sans le module, l'abonnement ne servirait à rien : les
///    routes refusent. Envoyer chaque jour la bibliothèque d'un compte gratuit
///    serait un envoi sans usage.
/// 2. **La télémétrie n'est pas refusée** (`TUNE_TELEMETRY`, ou le réglage de
///    l'interface) — un refus, d'où qu'il vienne, reste souverain (#3383).
///
/// ⚠️ Pourquoi le greffon ne lit plus `community_sync_enabled`. Ce réglage est
/// la bascule « Partage communautaire des métadonnées », désactivée par
/// défaut, qui gouverne l'envoi de TOUTES les métadonnées des pistes toutes
/// les 30 minutes. L'exiger ici rendait le module inutilisable par défaut —
/// liste toujours vide — ou obligeait à partager bien plus que ce qu'il
/// demande. Installer « Concerts » est le consentement à SON envoi, borné à
/// sa finalité : les noms d'artistes (et leur MBID quand on l'a), rien d'autre.
///
/// Le `match` sur [`Acces`] est exhaustif exprès : le jour où `Reduit` arrive,
/// le compilateur demande ici ce que la version réduite synchronise.
pub fn synchronisation_autorisee(settings: &SettingsRepo, acces: Acces) -> bool {
    let premium = match acces {
        Acces::Complet => true,
        Acces::Refuse => false,
    };
    premium && tune_core::cloud::telemetry::TelemetryReporter::is_enabled_for(settings)
}

/// La tâche périodique : abonnement toutes les 24 h, 2 min après le démarrage.
///
/// Elle ne tourne que si le greffon est installé (sinon il n'est pas chargé),
/// et n'envoie que si [`synchronisation_autorisee`] et un `instance_id` non
/// vide le permettent — relus à chaque tour : un Premium qui arrive ou qui
/// part est pris en compte au tour suivant, sans redémarrage.
fn lancer_synchronisation(
    backend: Arc<dyn DbBackend>,
    license: Option<Arc<LicenseManager>>,
) -> tokio::task::JoinHandle<()> {
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
            let droit = acces(license.as_deref()).await;

            if synchronisation_autorisee(&settings, droit) {
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
            } else {
                debug!(acces = ?droit, "concert_alerts_skipped_not_allowed");
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
// haute — le montage du routeur, le catalogue, l'arrêt de la tâche, la
// requête d'artistes, et le RENDU d'un refus par `reponse_de_refus`.
//
// Ce qui restait sans aucun témoin, c'est tout ce qui parle au nuage :
//
//   * le découpage en lots de `LOT` et sa tolérance au lot perdu. L'essai de
//     `tune-server` découpe LUI-MÊME un vecteur avec `chunks(LOT)` et vérifie
//     sa propre arithmétique — il relit le code au lieu de l'appeler, et
//     resterait vert si la boucle d'envoi se remettait à couper à 200 ;
//   * la LECTURE d'un refus : `reponse_de_refus` est gardée, mais rien ne
//     vérifiait que `recuperer_concerts_depuis` construit bien le `CloudError` qu'elle
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
            tune_core::http::client::shared(),
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
            tune_core::http::client::shared(),
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
            tune_core::http::client::shared(),
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
            tune_core::http::client::shared(),
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

        let total = envoyer_abonnements(
            &banc.racine,
            tune_core::http::client::shared(),
            "inst-1",
            &[],
        )
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

        envoyer_abonnements(
            &banc.racine,
            tune_core::http::client::shared(),
            "inst-42",
            &tous,
        )
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
    // `recuperer_concerts_depuis` — appelée par `concerts_a_venir`
    // -----------------------------------------------------------------------

    /// ⭐ La moitié LECTURE du 429. `tune-server/tests/concerts_plugin.rs` garde
    /// le RENDU (`reponse_de_refus`) en lui fabriquant un `CloudError` à la
    /// main ; rien ne vérifiait que la lecture en construit un. Un
    /// `CloudError::Message` rendu ici ferait un 200 parfaitement vert de
    /// l'autre côté.
    #[tokio::test]
    async fn un_429_du_nuage_arrive_en_refus_limite_avec_son_delai() {
        let banc = banc(vec![(429, r#"{"message":"Too Many Attempts."}"#, Some(42))]).await;

        let err =
            recuperer_concerts_depuis(&banc.racine, tune_core::http::client::shared(), "inst-1")
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

        let err =
            recuperer_concerts_depuis(&banc.racine, tune_core::http::client::shared(), "inst-1")
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

        let corps =
            recuperer_concerts_depuis(&banc.racine, tune_core::http::client::shared(), "inst-7")
                .await
                .unwrap();

        assert_eq!(corps["concerts"].as_array().unwrap().len(), 3);
        let cible = banc.recues()[0]["cible"].as_str().unwrap().to_string();
        assert!(
            cible.starts_with("GET /upcoming?") && cible.contains("instance_id=inst-7"),
            "l'identite doit partir en requete : {cible}"
        );
    }

    // -----------------------------------------------------------------------
    // Le périmètre (site-mozaiklabs#186)
    // -----------------------------------------------------------------------

    /// ⭐ Le périmètre APPLIQUÉ par le nuage remonte jusqu'à l'écran. Avant, la
    /// lecture ne gardait que `concerts` : l'écran affichait « 3 concerts »
    /// sans pouvoir dire « à moins de 100 km de Dijon », ni proposer d'élargir.
    #[tokio::test]
    async fn la_lecture_remonte_le_perimetre_applique_par_le_nuage() {
        let banc = banc(vec![(
            200,
            r#"{"concerts":[{"artist_name":"Superbus","event_date":"2026-11-02"}],
                "scope":"radius","radius_km":100,"city":"Dijon","country":"FR",
                "interne":"ne doit pas sortir"}"#,
            None,
        )])
        .await;

        let corps =
            recuperer_concerts_depuis(&banc.racine, tune_core::http::client::shared(), "inst-7")
                .await
                .unwrap();

        assert_eq!(corps["scope"], "radius");
        assert_eq!(corps["radius_km"], 100);
        assert_eq!(corps["city"], "Dijon");
        assert_eq!(corps["country"], "FR");
        assert_eq!(corps["concerts"][0]["artist_name"], "Superbus");
        assert!(
            corps.get("interne").is_none(),
            "l'ecran recoit le contrat de CE serveur, pas le corps brut du nuage"
        );
    }

    /// Contre-épreuve : un nuage qui ne dit rien du périmètre (version
    /// antérieure à #186, ou champs nuls) ne fait fabriquer AUCUN périmètre.
    /// Un `scope` inventé ferait mentir l'écran sur le filtre appliqué.
    #[tokio::test]
    async fn contre_epreuve_aucun_perimetre_n_est_fabrique() {
        let banc = banc(vec![(200, r#"{"concerts":[],"radius_km":null}"#, None)]).await;

        let corps =
            recuperer_concerts_depuis(&banc.racine, tune_core::http::client::shared(), "inst-7")
                .await
                .unwrap();

        assert_eq!(corps, json!({"concerts": []}));
    }

    /// ⭐ L'aller de la localisation : la demande part telle quelle, sauf
    /// l'identité — c'est TOUJOURS celle de ce serveur. Et le retour rend ce
    /// que le nuage a retenu, `located` compris.
    #[tokio::test]
    async fn la_localisation_part_avec_l_instance_du_serveur_et_revient_entiere() {
        let banc = banc(vec![(
            200,
            r#"{"scope":"radius","city":"Dijon","country":"FR","radius_km":50,"located":true}"#,
            None,
        )])
        .await;
        let demande = json!({
            "city": "Dijon", "postal_code": "21000", "country": "FR",
            "scope": "radius", "radius_km": 50,
            "instance_id": "celle-d-un-autre",
        });

        let rendu = enregistrer_localisation_vers(
            &banc.racine,
            tune_core::http::client::shared(),
            "inst-9",
            &demande,
        )
        .await
        .unwrap();

        let recues = banc.recues();
        assert!(
            recues[0]["cible"]
                .as_str()
                .unwrap()
                .starts_with("POST /location "),
            "{:?}",
            recues[0]["cible"]
        );
        assert_eq!(
            recues[0]["corps"]["instance_id"], "inst-9",
            "l'instance est celle du serveur, jamais celle que la demande pretend"
        );
        assert_eq!(recues[0]["corps"]["city"], "Dijon");
        assert_eq!(recues[0]["corps"]["postal_code"], "21000");
        assert_eq!(recues[0]["corps"]["radius_km"], 50);
        assert_eq!(
            rendu,
            json!({"scope":"radius","city":"Dijon","country":"FR","radius_km":50,"located":true})
        );
    }

    /// Un 422 du nuage est une demande refusée, pas une panne : réessayer la
    /// même chose ne donnera rien, et l'écran doit le dire autrement.
    #[tokio::test]
    async fn un_422_du_nuage_est_une_demande_refusee_pas_une_panne() {
        let banc = banc(vec![(
            422,
            r#"{"message":"The radius km field is invalid."}"#,
            None,
        )])
        .await;

        let echec = enregistrer_localisation_vers(
            &banc.racine,
            tune_core::http::client::shared(),
            "inst-9",
            &json!({"city": "Dijon", "country": "FR"}),
        )
        .await
        .unwrap_err();

        assert!(matches!(echec, EchecLocalisation::Refusee), "{echec:?}");
    }

    /// Contre-épreuve du précédent : une limite (429) reste une limite, avec
    /// son délai — elle ne se confond pas avec une demande refusée.
    #[tokio::test]
    async fn contre_epreuve_un_429_sur_la_localisation_reste_une_limite() {
        let banc = banc(vec![(429, r#"{"message":"Too Many Attempts."}"#, Some(12))]).await;

        let echec = enregistrer_localisation_vers(
            &banc.racine,
            tune_core::http::client::shared(),
            "inst-9",
            &json!({"city": "Dijon", "country": "FR"}),
        )
        .await
        .unwrap_err();

        let EchecLocalisation::Nuage(e) = echec else {
            panic!("un 429 n'est pas une demande refusee");
        };
        assert!(e.is_rate_limited());
        assert_eq!(e.retry_after(), Some(12));
    }

    // -----------------------------------------------------------------------
    // `valider_localisation` — les règles du nuage, appliquées avant l'appel
    // -----------------------------------------------------------------------

    /// Le cran par défaut est le PAYS (arbitrage du 29/08) : mieux vaut montrer
    /// trop que trop peu. Et le pays part en majuscules, comme le nuage le
    /// compare (`where('country', …)`).
    #[test]
    fn sans_cran_la_demande_part_au_pays_et_en_majuscules() {
        let corps = valider_localisation(&json!({"city": " Dijon ", "country": "fr"})).unwrap();
        assert_eq!(corps["scope"], PERIMETRE_PAR_DEFAUT);
        assert_eq!(corps["scope"], "country");
        assert_eq!(corps["country"], "FR");
        assert_eq!(corps["city"], "Dijon");
        assert!(corps.get("radius_km").is_none(), "aucun rayon invente");
    }

    /// Le corps que l'écran envoie aujourd'hui (`setLocalisationConcerts`),
    /// cran par cran : il doit passer tel quel.
    #[test]
    fn les_trois_crans_de_l_ecran_passent() {
        for scope in PERIMETRES {
            for km in RAYONS_KM {
                let corps = valider_localisation(&json!({
                    "city": "—", "postal_code": null, "country": "FR",
                    "scope": scope, "radius_km": km,
                }))
                .unwrap_or_else(|champ| panic!("{scope}/{km} refuse sur {champ}"));
                assert_eq!(corps["scope"], scope);
                assert_eq!(corps["radius_km"], km);
            }
        }
    }

    /// Liste fermée : un rayon libre serait un « partout » déguisé, et le
    /// nuage le refuserait en 422 sans dire quel champ.
    #[test]
    fn un_rayon_hors_liste_ou_un_cran_inconnu_nomme_son_champ() {
        let base = json!({"city": "Dijon", "country": "FR"});
        let avec = |cle: &str, v: Value| {
            let mut d = base.clone();
            d[cle] = v;
            valider_localisation(&d)
        };
        assert_eq!(avec("radius_km", json!(75)), Err("radius_km"));
        assert_eq!(avec("radius_km", json!("100")), Err("radius_km"));
        assert_eq!(avec("scope", json!("galaxy")), Err("scope"));
        assert_eq!(avec("country", json!("FRANCE")), Err("country"));
        assert_eq!(avec("city", json!("   ")), Err("city"));
        assert_eq!(valider_localisation(&json!({"country": "FR"})), Err("city"));
        assert_eq!(valider_localisation(&Value::Null), Err("city"));
    }

    /// L'identité ne vient JAMAIS du client : la validation ne la recopie pas.
    #[test]
    fn la_validation_ne_recopie_pas_l_instance_du_client() {
        let corps = valider_localisation(
            &json!({"city": "Dijon", "country": "FR", "instance_id": "pirate"}),
        )
        .unwrap();
        assert!(corps.get("instance_id").is_none());
    }

    // -----------------------------------------------------------------------
    // `acces` — la seule décision du payant
    // -----------------------------------------------------------------------

    fn licence() -> Arc<LicenseManager> {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        Arc::new(LicenseManager::new(Arc::new(db)))
    }

    /// ⚠️ Une licence absente ne vaut PAS une autorisation — c'est la
    /// contre-épreuve de tout le portillon.
    #[tokio::test]
    async fn sans_licence_l_acces_est_refuse() {
        assert_eq!(acces(None).await, Acces::Refuse);
    }

    #[tokio::test]
    async fn un_compte_gratuit_est_refuse_un_compte_premium_a_l_acces_complet() {
        let l = licence();
        assert_eq!(
            acces(Some(l.as_ref())).await,
            Acces::Refuse,
            "compte neuf = gratuit"
        );
        l.set_account_premium(true, None).await;
        assert_eq!(acces(Some(l.as_ref())).await, Acces::Complet);
        l.set_account_premium(false, None).await;
        assert_eq!(
            acces(Some(l.as_ref())).await,
            Acces::Refuse,
            "un Premium qui s'en va referme le module"
        );
    }

    /// Le greffon nomme son module (cadenas avant le clic) et entre au
    /// catalogue : l'écran existe, le nuage a des dates.
    #[test]
    fn le_greffon_nomme_son_module_et_entre_au_catalogue() {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        let greffon = ConcertsPlugin::new(HostServices {
            backend: Arc::new(db),
        });
        assert_eq!(greffon.required_feature(), Some(Feature::Concerts));
        assert!(greffon.catalogued());
        assert!(!greffon.default_enabled(), "toujours opt-in");
    }

    // -----------------------------------------------------------------------
    // `synchronisation_autorisee`
    // -----------------------------------------------------------------------

    fn reglages() -> SettingsRepo {
        let db = tune_core::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        tune_core::db::migrations::run_migrations(&db).unwrap();
        SettingsRepo::with_backend(Arc::new(db))
    }

    /// Un compte gratuit n'envoie pas sa bibliothèque : ses routes refusent,
    /// l'abonnement ne servirait à rien.
    #[test]
    fn sans_premium_la_bibliotheque_ne_part_pas() {
        assert!(!synchronisation_autorisee(&reglages(), Acces::Refuse));
    }

    /// ⭐ Premium, installation neuve : l'abonnement part SANS que le
    /// « partage communautaire des métadonnées » soit coché. Avant, il
    /// l'exigeait, et la liste restait vide pour tout le monde par défaut.
    #[test]
    fn premium_l_abonnement_part_sans_le_partage_communautaire() {
        if !tune_core::cloud::telemetry::TelemetryReporter::is_enabled() {
            eprintln!("TUNE_TELEMETRY refuse dans cet environnement : cas non jouable ici");
            return;
        }
        let settings = reglages();
        assert!(
            settings
                .get(tune_core::cloud::consent::SYNC_SETTING_KEY)
                .unwrap()
                .is_none(),
            "le partage communautaire n'est pas coche dans une base neuve"
        );
        assert!(synchronisation_autorisee(&settings, Acces::Complet));
    }

    /// Contre-épreuve : un refus de télémétrie reste souverain (#3383), même
    /// Premium.
    #[test]
    fn contre_epreuve_le_refus_de_telemetrie_arrete_l_abonnement() {
        let settings = reglages();
        settings
            .set(tune_core::cloud::telemetry::TELEMETRY_SETTING_KEY, "false")
            .unwrap();
        assert!(!synchronisation_autorisee(&settings, Acces::Complet));
    }
}
