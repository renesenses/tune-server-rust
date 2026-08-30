//! Les concerts des artistes de la bibliothèque, en [`TunePlugin`] (#2363).
//!
//! Extrait mot pour mot du cœur toujours-compilé, sans changer un
//! comportement :
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
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use tune_core::db::backend::DbBackend;
use tune_core::db::settings_repo::SettingsRepo;
use tune_core::event_bus::TuneEvent;
use tune_core::license::{Feature, LicenseManager};
use tune_core::plugin_sdk::{PluginContext, TunePlugin};

const CONCERTS_API: &str = "https://mozaiklabs.fr/api/v1/premium/concerts";

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

    /// Le module Premium auquel ce greffon appartient.
    ///
    /// Déclaré ici, et non câblé sur le chemin d'URL par l'hôte : un second
    /// greffon, public celui-là, ne déclarera rien et passera librement, sans
    /// qu'on ait à défaire quoi que ce soit.
    ///
    /// ⚠️ Cette déclaration ne FERME rien à elle seule. Elle sert au
    /// gestionnaire de greffons, qui affiche le cadenas avant le clic. Le
    /// comportement réel se décide dans [`acces`], chez le greffon — parce
    /// que « Concerts » doit un jour servir une version RÉDUITE aux comptes
    /// gratuits, ce qu'un garde tout-ou-rien de l'hôte ne saurait pas faire.
    fn required_feature(&self) -> Option<Feature> {
        Some(Feature::Concerts)
    }

    /// Hors catalogue tant qu'aucun écran ne consomme ces routes — voir l'en-
    /// tête du module. Le plugin reste compilé, testé, et se charge si
    /// `plugin_concerts_installed` est posé à la main.
    fn catalogued(&self) -> bool {
        false
    }

    async fn setup(&mut self, ctx: &PluginContext) -> Result<(), String> {
        ctx.register_router(router(self.backend.clone(), ctx.license.clone()));
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
    license: Option<Arc<LicenseManager>>,
}

/// Ce que le serveur a le droit de rendre, selon sa licence.
///
/// ⚠️ C'EST LE SEUL ENDROIT OÙ LE PAYANT SE DÉCIDE, et c'est délibéré.
///
/// La variante manquante ici est `Reduit` : une version limitée pour les
/// comptes gratuits, prévue une fois le module abouti. Le jour venu, elle
/// s'ajoute à cette énumération et à l'unique `match` qui la lit — rien
/// d'autre ne bouge. Si le refus avait été monté par l'hôte devant les routes,
/// il aurait fallu le défaire.
enum Acces {
    Complet,
    Refuse,
}

async fn acces(license: &Option<Arc<LicenseManager>>) -> Acces {
    // Pas de licence fournie par l'hôte = pas de Premium. Une absence ne
    // s'interprète jamais en faveur du doute.
    match license {
        Some(l) if l.check_feature(Feature::Concerts).await => Acces::Complet,
        _ => Acces::Refuse,
    }
}

pub fn router(backend: Arc<dyn DbBackend>, license: Option<Arc<LicenseManager>>) -> Router<()> {
    Router::new()
        .route("/upcoming", get(concerts_a_venir))
        .with_state(EtatConcerts { backend, license })
}

/// `GET /api/v1/ext/concerts/upcoming` — remplace `GET /system/concerts`.
///
/// Le corps d'erreur de l'ancienne route était une chaîne technique anglaise
/// (`{"concerts": [], "error": "concerts: HTTP 500"}`) qu'une interface
/// traduite en 11 langues aurait affichée telle quelle. On rend désormais un
/// **code stable**, traduisible côté client, et le détail part au journal.
async fn concerts_a_venir(
    axum::extract::State(etat): axum::extract::State<EtatConcerts>,
) -> axum::response::Response {
    match acces(&etat.license).await {
        Acces::Complet => {}
        // Même corps que `require_premium` de l'hôte : le client sait déjà
        // reconnaître ce refus comme un refus d'offre et non comme une panne
        // (`estRefusPremium`, tune-web-client). Un corps de plus aurait obligé
        // l'écran à apprendre une deuxième forme, donc à en oublier une.
        Acces::Refuse => {
            return (
                axum::http::StatusCode::PAYMENT_REQUIRED,
                Json(json!({
                    "error": "premium_required",
                    "feature": Feature::Concerts.display_name(),
                    "upgrade_url": "https://mozaiklabs.fr/pricing",
                })),
            )
                .into_response();
        }
    }

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
            warn!(error = %e, "concerts_fetch_failed");
            Json(json!({"concerts": [], "code": "concerts.unavailable"})).into_response()
        }
    }
}

// ---------------------------------------------------------------------------
// Le cloud — repris tel quel de tune-core/src/cloud/concert_alerts.rs
// ---------------------------------------------------------------------------

/// Le cloud n'accepte pas plus de 200 artistes par appel
/// (`'artists' => 'required|array|max:200'`).
const LOT: usize = 200;

/// Plafond de sécurité, en artistes. Une bibliothèque ordinaire en compte
/// quelques milliers — 1 747 sur le serveur de référence, soit 9 appels. Ce
/// plafond n'existe que pour qu'une bibliothèque pathologique ne parte pas en
/// centaines de requêtes, et toute troncature est JOURNALISÉE : un abonnement
/// amputé en silence se lit « ce groupe ne joue nulle part » côté utilisateur.
const PLAFOND: usize = 5_000;

/// Les artistes de la bibliothèque, prêts à être abonnés.
///
/// ⚠️ LE MBID N'EST PLUS EXIGÉ, ET LA BORNE DE 200 N'EST PLUS UNE COUPE.
///
/// La version d'origine filtrait `musicbrainz_id IS NOT NULL` puis coupait à
/// `LIMIT 200`. Son commentaire affirmait que cette borne « n'est pas un
/// défaut » puisqu'elle épouse celle du cloud. La mesure dit l'inverse : la
/// borne du cloud est celle d'UN APPEL, pas d'une bibliothèque. S'y aligner au
/// lieu de découper, c'est abandonner tout le reste sans le dire.
///
/// Mesuré le 30/08/2026 sur les 1 747 artistes du serveur de référence :
///
///   - 881 (50,4 %) sont reconnus par Ticketmaster par leur seul NOM, mais
///     seules 460 des attractions correspondantes portent un lien MusicBrainz —
///     exiger le MBID écarte donc la moitié des concerts que la source rend ;
///   - avec `LIMIT 200`, **1 547 artistes sur 1 747 n'étaient jamais abonnés**,
///     et rien dans le journal ne le signalait.
///
/// Le cloud classe les abonnements par nom replié depuis site-mozaiklabs#185 :
/// le MBID reste envoyé quand on l'a — c'est la meilleure identité disponible —
/// mais il a cessé d'être une condition d'entrée.
///
/// `GROUP BY name` parce que la même personne apparaît souvent deux fois, une
/// ligne identifiée par un scan enrichi et une ligne nue.
fn artistes_de_la_bibliotheque(backend: &Arc<dyn DbBackend>) -> Result<Vec<Value>, String> {
    let rows = backend
        .query_many(
            "SELECT name, MAX(musicbrainz_id) FROM artists \
             WHERE name IS NOT NULL AND name != '' \
             GROUP BY name ORDER BY name \
             LIMIT 5000",
            &[],
        )
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
/// ⚠️ ORDRE DE DÉPLOIEMENT. La charge utile porte désormais des artistes SANS
/// `musicbrainz_artist_id`. Le cloud ne l'accepte que depuis
/// site-mozaiklabs#185, déployé le 30/08/2026 ; une version antérieure
/// répondait 422 sur la charge entière.
pub async fn synchroniser_abonnements(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<usize, String> {
    let artistes = artistes_de_la_bibliotheque(backend)?;

    if artistes.is_empty() {
        debug!("concert_alerts_no_artists");
        return Ok(0);
    }

    if artistes.len() >= PLAFOND {
        warn!(
            plafond = PLAFOND,
            "concert_subscriptions_tronquees: bibliotheque au-dela du plafond"
        );
    }

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
            .post(format!("{CONCERTS_API}/subscribe"))
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
                warn!(statut = %r.status(), "concert_subscribe_lot_refuse");
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
        // Le cloud écarte les noms qui ne désignent aucun artiste
        // (« Various Artists », « Unknown »...).
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
pub async fn recuperer_concerts(
    http_client: &reqwest::Client,
    instance_id: &str,
) -> Result<Vec<Value>, String> {
    let resp = http_client
        .get(format!("{CONCERTS_API}/upcoming"))
        .query(&[("instance_id", instance_id)])
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .map_err(|e| format!("concerts: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("concerts: HTTP {}", resp.status()));
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

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::db::migrations;
    use tune_core::db::sqlite::SqliteDb;

    fn base_avec_artistes(artistes: &[(&str, Option<&str>)]) -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();

        for (nom, mbid) in artistes {
            db.execute(
                "INSERT INTO artists (name, musicbrainz_id) VALUES (?, ?)",
                &[nom, mbid],
            )
            .unwrap();
        }

        Arc::new(db)
    }

    fn noms(artistes: &[Value]) -> Vec<String> {
        artistes
            .iter()
            .map(|a| a["artist_name"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// CONTRE-ÉPREUVE DU CORRECTIF. Avec le filtre d'origine
    /// `WHERE musicbrainz_id IS NOT NULL`, Melissa Laveaux n'était jamais
    /// abonnée — et c'est justement une artiste que Ticketmaster reconnaît par
    /// son nom, avec des dates mesurées à La Ferté-Bernard et Grasse le
    /// 30/08/2026. Ce test échoue si le filtre revient.
    #[test]
    fn un_artiste_sans_mbid_est_abonne() {
        let backend = base_avec_artistes(&[
            ("Melissa Laveaux", None),
            (
                "Bernard Lavilliers",
                Some("8bef9bae-a250-4c4e-8e5e-b2f81607db2a"),
            ),
        ]);

        let artistes = artistes_de_la_bibliotheque(&backend).unwrap();

        assert_eq!(
            noms(&artistes),
            vec!["Bernard Lavilliers", "Melissa Laveaux"],
            "un artiste sans MBID doit partir comme les autres"
        );
        assert!(
            artistes[1]["musicbrainz_artist_id"].is_null(),
            "l'absence de MBID s'envoie comme nulle, pas comme chaine vide"
        );
    }

    #[test]
    fn le_mbid_est_conserve_quand_on_l_a() {
        let backend = base_avec_artistes(&[(
            "Fatoumata Diawara",
            Some("6f5064bb-7dbb-4a44-bac5-04c467394817"),
        )]);

        let artistes = artistes_de_la_bibliotheque(&backend).unwrap();

        assert_eq!(
            artistes[0]["musicbrainz_artist_id"], "6f5064bb-7dbb-4a44-bac5-04c467394817",
            "le MBID reste la meilleure identite disponible"
        );
    }

    /// La même personne apparaît souvent deux fois : une ligne identifiée par
    /// un scan enrichi, une autre nue. Le cloud classant par nom replié,
    /// envoyer les deux ne ferait que gonfler la charge utile.
    #[test]
    fn un_artiste_present_deux_fois_ne_part_qu_une_fois_avec_son_mbid() {
        let backend = base_avec_artistes(&[
            ("Yael Naim", None),
            ("Yael Naim", Some("11111111-1111-4111-8111-111111111111")),
        ]);

        let artistes = artistes_de_la_bibliotheque(&backend).unwrap();

        assert_eq!(artistes.len(), 1, "un seul envoi pour un seul artiste");
        assert_eq!(
            artistes[0]["musicbrainz_artist_id"], "11111111-1111-4111-8111-111111111111",
            "entre une ligne identifiee et une ligne nue, on garde l'identite"
        );
    }

    #[test]
    fn un_nom_vide_ne_part_pas() {
        let backend = base_avec_artistes(&[("", None), ("Superbus", None)]);

        assert_eq!(
            noms(&artistes_de_la_bibliotheque(&backend).unwrap()),
            vec!["Superbus"]
        );
    }

    /// Le cloud refuse plus de 200 artistes par appel. La version d'origine
    /// s'ALIGNAIT sur cette borne au lieu de découper : sur une bibliothèque de
    /// 1 747 artistes, 1 547 n'étaient jamais abonnés et rien ne le signalait.
    #[test]
    fn au_dela_de_200_artistes_le_decoupage_les_emmene_tous() {
        let noms_generes: Vec<String> = (0..450).map(|i| format!("Artiste {i:04}")).collect();
        let refs: Vec<(&str, Option<&str>)> =
            noms_generes.iter().map(|n| (n.as_str(), None)).collect();

        let backend = base_avec_artistes(&refs);
        let tous = artistes_de_la_bibliotheque(&backend).unwrap();

        assert_eq!(tous.len(), 450, "aucun artiste ne doit etre perdu en amont");

        let lots: Vec<_> = tous.chunks(LOT).collect();
        assert_eq!(lots.len(), 3, "450 artistes = 3 appels de 200 au plus");
        assert_eq!(lots[0].len(), 200);
        assert_eq!(lots[2].len(), 50);
        assert_eq!(
            lots.iter().map(|l| l.len()).sum::<usize>(),
            450,
            "la somme des lots doit rendre la bibliotheque entiere"
        );
    }
}

#[cfg(test)]
mod tests_portillon {
    use super::*;
    use tune_core::db::migrations;
    use tune_core::db::sqlite::SqliteDb;
    use tune_core::license::LicenseManager;

    fn base() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// Une licence neuve : compte gratuit, comme une installation qui vient de
    /// démarrer.
    fn licence() -> Arc<LicenseManager> {
        Arc::new(LicenseManager::new(base()))
    }

    /// ⚠️ Une licence absente ne vaut PAS une autorisation.
    ///
    /// C'est le cas d'un hôte qui n'en fournit pas — tests, tune-cli, ou une
    /// construction future qui oublierait de la brancher. Interpréter
    /// l'absence en faveur du doute ouvrirait la fonction à tout le monde le
    /// jour où quelqu'un déplace une ligne dans `AppState`.
    #[tokio::test]
    async fn sans_licence_l_acces_est_refuse() {
        assert!(matches!(acces(&None).await, Acces::Refuse));
    }

    #[tokio::test]
    async fn un_compte_gratuit_est_refuse() {
        let l = licence();
        assert!(matches!(acces(&Some(l)).await, Acces::Refuse));
    }

    #[tokio::test]
    async fn un_compte_premium_a_l_acces_complet() {
        let l = licence();
        l.set_account_premium(true, None).await;
        assert!(matches!(acces(&Some(l)).await, Acces::Complet));
    }

    /// Le greffon déclare son module pour que le gestionnaire affiche le
    /// cadenas AVANT le clic — sans quoi l'utilisateur installe, redémarre, et
    /// n'obtient qu'un 402.
    #[test]
    fn le_greffon_nomme_son_module() {
        let greffon = ConcertsPlugin::new(HostServices { backend: base() });
        assert_eq!(greffon.required_feature(), Some(Feature::Concerts));
    }
}
