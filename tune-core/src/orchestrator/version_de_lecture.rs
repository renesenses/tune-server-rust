//! La RÈGLE DE VERSION appliquée à la lecture (#2264, décisions du 07/10/2026).
//!
//! # Ce que fait ce module
//!
//! Lancer une piste — depuis la bibliothèque, une playlist, un favori, la
//! file, l'historique — joue la version que désigne la règle du profil de la
//! zone (`local`, `quality`, `service:<nom>`, voir
//! [`crate::library::regle_de_version`]) parmi les exemplaires du MÊME
//! enregistrement : ceux que « Autres versions » regroupe
//! ([`crate::library::groupes_versions::grouper`]), choisis par la même règle
//! ([`crate::library::groupes_versions::choisir_avec_repli`]).
//!
//! # Un seul point d'entrée
//!
//! [`PlaybackOrchestrator::appliquer_la_regle_de_version`] est appelé par
//! `play_inner`, où passent TOUTES les lectures (`play`, `play_from_queue`,
//! `play_without_history`). Aucun appelant ne recopie la règle.
//!
//! # L'enchaînement sans blanc est gardé (décision du 07/10/2026)
//!
//! La règle ne touche JAMAIS une piste suivante pré-armée : le pré-armement
//! (`resolve_queue_item_url`, `resolve_gapless_next_local_file`) arme la
//! ligne de file telle quelle, et l'avance sans blanc ne passe pas par
//! `play_inner`. Une piste suivante n'est remplacée que si elle démarre par
//! l'avance NORMALE (sortie qui n'enchaîne pas sans blanc, ou pré-armement
//! qui n'a pas eu lieu) : elle passe alors par `play_inner`, sans rien casser
//! du pré-armement puisqu'il n'y en a pas.
//!
//! # Sans règle réglée, rien n'est remplacé
//!
//! [`RegleDeChoix::Aucune`] est le défaut : on joue ce qui est lancé, et la
//! piste en cours ne porte aucune décision.
//!
//! # Ce qui ne change PAS de version
//!
//! * un choix EXPLICITE fait dans « Autres versions » : le gestionnaire de la
//!   route l'épingle ([`PlaybackOrchestrator::epingler_version_explicite`])
//!   et la règle s'efface pour cette lecture-là ;
//! * la piste QUI JOUE DÉJÀ sur la zone (reprise après Stop, avance rapide,
//!   reconnexion, nouvel essai) : on ne change jamais la version en cours ;
//! * ce qui n'est ni une piste de la bibliothèque ni une piste d'un service
//!   de [`SERVICES_DE_VERSIONS`] : radio, podcast, fichier glissé, serveur
//!   UPnP, CD, greffons.
//!
//! # Repli (décision 2)
//!
//! Quand la version préférée est indisponible — fichier absent du disque,
//! piste que le service déclare indisponible, service préféré non connecté ou
//! muet dans le délai —, la suivante disponible joue, et c'est DIT : une ligne
//! de journal `version_de_repli` et `NowPlaying::version.fallback`.
//!
//! # Coût borné
//!
//! * la bibliothèque : trois lectures indexées (la piste, ses identifiants
//!   d'enregistrement — index de la migration 122 —, les pistes du même
//!   artiste à ±2 s), chacune plafonnée ;
//! * les services : seulement ceux dont la règle a besoin (aucun pour
//!   `local`), une recherche chacun, en parallèle, dans un budget de
//!   [`BUDGET_DES_SERVICES`]. Une réponse arrivée après le budget remplit le
//!   cache pour le lancement suivant au lieu d'être perdue.

use std::time::{Duration, Instant};

use super::*;
use crate::library::groupes_versions::{
    Exemplaire, Qualite, RegleDeChoix, SERVICES_DE_VERSIONS, choisir_avec_repli, grouper,
};
use crate::library::regle_de_version::regle_effective;
use crate::library::versions_en_base as base;
use crate::playback::{PisteDemandee, VersionJouee};
use crate::streaming::StreamTrack;

/// Le temps qu'un lancement accorde aux services pour répondre.
pub(crate) const BUDGET_DES_SERVICES: Duration = Duration::from_millis(1200);
/// Durée de vie d'une réponse de service gardée en cache.
const DUREE_DU_CACHE: Duration = Duration::from_secs(30 * 60);
/// Durée de vie d'un choix explicite non consommé.
const DUREE_D_UNE_EPINGLE: Duration = Duration::from_secs(60);
/// Plafond de chaque lecture de candidats locaux.
const PLAFOND_LOCAL: i64 = 50;
/// Résultats demandés à un service.
const RESULTATS_PAR_SERVICE: usize = 10;

/// Ce qu'une demande désigne : une ligne de la bibliothèque, ou une piste
/// d'un service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Identite {
    pub track_id: Option<i64>,
    pub source: String,
    pub source_id: Option<String>,
}

impl Identite {
    fn de_la_demande(req: &PlayRequest) -> Identite {
        Identite::depuis(
            req.track_id,
            req.source.as_deref(),
            req.source_id.as_deref(),
        )
    }

    pub(crate) fn depuis(
        track_id: Option<i64>,
        source: Option<&str>,
        source_id: Option<&str>,
    ) -> Identite {
        let source = source
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_ascii_lowercase)
            .unwrap_or_else(|| "local".to_string());
        Identite {
            // Une piste de service ne se désigne pas par un `track_id`.
            track_id: if source == "local" { track_id } else { None },
            source_id: if source == "local" {
                None
            } else {
                source_id.map(str::to_string)
            },
            source,
        }
    }

    fn piste_demandee(&self) -> PisteDemandee {
        PisteDemandee {
            track_id: self.track_id,
            source: self.source.clone(),
            source_id: self.source_id.clone(),
        }
    }
}

/// Ce que l'appelant connaît de la piste demandée, en plus de son identité :
/// de quoi décrire une piste de service sans l'interroger.
#[derive(Debug, Clone, Default)]
struct Indices {
    titre: Option<String>,
    artiste: Option<String>,
    album: Option<String>,
    duree_ms: Option<i64>,
}

/// L'exemplaire retenu, quand ce n'est pas celui qui a été demandé.
#[derive(Debug, Clone)]
enum Cible {
    Locale(i64),
    Service {
        source: String,
        piste: Box<StreamTrack>,
    },
}

#[derive(Debug, Clone)]
struct Decision {
    cible: Option<Cible>,
    version: VersionJouee,
}

/// La mémoire de la règle de version : les choix explicites en attente et
/// les réponses des services. Verrous std : jamais tenus à travers un await.
#[derive(Default)]
pub(crate) struct EtatDesVersions {
    epingles: std::sync::Mutex<HashMap<i64, (Identite, Instant)>>,
    /// (service, requête) → pistes rendues.
    recherches: Arc<std::sync::Mutex<HashMap<(String, String), (Instant, Vec<StreamTrack>)>>>,
    /// (service, identifiant) → la fiche de la piste demandée.
    fiches: Arc<std::sync::Mutex<HashMap<(String, String), (Instant, Option<StreamTrack>)>>>,
}

fn lire_cache<K: std::hash::Hash + Eq, V: Clone>(
    cache: &std::sync::Mutex<HashMap<K, (Instant, V)>>,
    cle: &K,
) -> Option<V> {
    cache
        .lock()
        .ok()?
        .get(cle)
        .filter(|(quand, _)| quand.elapsed() < DUREE_DU_CACHE)
        .map(|(_, v)| v.clone())
}

fn exemplaire_de_service(service: &str, t: &StreamTrack) -> Exemplaire {
    Exemplaire {
        source: service.to_string(),
        track_id: None,
        source_id: Some(t.id.clone()),
        titre: t.title.clone(),
        artiste: t.artist.clone(),
        album: t.album.clone().unwrap_or_default(),
        isrc: t.isrc.clone(),
        mbid_enregistrement: None,
        duree_ms: (t.duration_ms > 0).then_some(t.duration_ms as i64),
        qualite: t.quality.as_ref().map(|q| Qualite {
            format: Some(q.codec.to_ascii_lowercase()),
            sample_rate: (q.sample_rate > 0).then_some(i64::from(q.sample_rate)),
            bit_depth: (q.bit_depth > 0).then_some(i64::from(q.bit_depth)),
        }),
        disponible: t.disponible,
    }
}

/// Le fichier d'une ligne locale est-il là ? Son propre chemin, sinon une de
/// ses copies à l'identique (`track_copies`, #4907) : la lecture locale saura
/// l'ouvrir.
fn fichier_local_present(
    db: &Arc<dyn crate::db::backend::DbBackend>,
    track_id: i64,
    chemin: Option<&str>,
) -> bool {
    use crate::db::backend::ToSqlValue;
    if chemin.is_some_and(|c| resolve_existing_local_path(c).is_some()) {
        return true;
    }
    let sql = match db.engine() {
        crate::db::engine::Engine::Sqlite => {
            "SELECT file_path FROM track_copies WHERE track_id = ?"
        }
        crate::db::engine::Engine::Postgres => {
            "SELECT file_path FROM track_copies WHERE track_id = $1"
        }
    };
    db.query_many(sql, &[&track_id as &dyn ToSqlValue])
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.first().and_then(|v| v.as_string()))
        .any(|c| resolve_existing_local_path(&c).is_some())
}

impl PlaybackOrchestrator {
    /// Le gestionnaire de lecture le dit : cette piste a été choisie À LA
    /// MAIN dans « Autres versions ». La prochaine lecture de cette identité
    /// sur cette zone la joue telle quelle, sans règle.
    pub fn epingler_version_explicite(
        &self,
        zone_id: i64,
        track_id: Option<i64>,
        source: Option<&str>,
        source_id: Option<&str>,
    ) {
        if let Ok(mut e) = self.versions.epingles.lock() {
            e.insert(
                zone_id,
                (
                    Identite::depuis(track_id, source, source_id),
                    Instant::now(),
                ),
            );
        }
    }

    fn consommer_l_epingle(&self, zone_id: i64, identite: &Identite) -> bool {
        let Ok(mut e) = self.versions.epingles.lock() else {
            return false;
        };
        match e.get(&zone_id) {
            Some((id, quand)) if id == identite && quand.elapsed() < DUREE_D_UNE_EPINGLE => {
                e.remove(&zone_id);
                true
            }
            Some((_, quand)) if quand.elapsed() >= DUREE_D_UNE_EPINGLE => {
                e.remove(&zone_id);
                false
            }
            _ => false,
        }
    }

    /// Le point unique de la règle de version (voir l'en-tête). Réécrit
    /// `req` quand la règle fait jouer un autre exemplaire, et rend ce qu'il
    /// faut publier dans la piste en cours ; `None` quand aucune règle n'a eu
    /// à se prononcer.
    pub(super) async fn appliquer_la_regle_de_version(
        &self,
        req: &mut PlayRequest,
    ) -> Option<VersionJouee> {
        if req.temp_file_path.is_some() {
            return None;
        }
        let identite = Identite::de_la_demande(req);
        let etat = self.playback.get_state(req.zone_id).await;

        // La piste qui joue déjà garde sa version : une reprise ou une
        // re-création ne doit pas en changer.
        if let Some(np) = etat.now_playing.as_ref()
            && Self::is_same_track_retap(np, req)
        {
            return np.version.clone();
        }

        if !self.eligible(&identite).await {
            return None;
        }
        let (regle, origine) = regle_effective(&self.db, etat.session_profile_id);

        if self.consommer_l_epingle(req.zone_id, &identite) {
            info!(
                zone_id = req.zone_id,
                track_id = ?identite.track_id,
                source = %identite.source,
                source_id = ?identite.source_id,
                "version_choisie_explicitement"
            );
            return Some(VersionJouee {
                origin: "explicit".into(),
                rule: regle.texte(),
                rule_origin: origine.nom().into(),
                ..Default::default()
            });
        }

        // Rien de réglé (ou `none` choisi) : on joue ce qui est lancé.
        if regle == RegleDeChoix::Aucune {
            return None;
        }
        let indices = Indices {
            titre: req.title.clone(),
            artiste: req.artist_name.clone(),
            album: req.album_title.clone(),
            duree_ms: req.duration_ms,
        };
        let t0 = Instant::now();
        let decision = self.decider(&identite, &indices, &regle).await?;
        let mut version = decision.version;
        version.rule_origin = origine.nom().into();

        if let Some(cible) = decision.cible {
            Self::reecrire_la_demande(req, &cible);
        }
        let jouee = Identite::de_la_demande(req);
        info!(
            zone_id = req.zone_id,
            regle = %version.rule,
            origine = %version.rule_origin,
            demandee_track_id = ?identite.track_id,
            demandee_source = %identite.source,
            demandee_source_id = ?identite.source_id,
            jouee_track_id = ?jouee.track_id,
            jouee_source = %jouee.source,
            jouee_source_id = ?jouee.source_id,
            repli = version.fallback,
            duree_ms = t0.elapsed().as_millis() as u64,
            "version_regle_appliquee"
        );
        if version.fallback {
            warn!(
                zone_id = req.zone_id,
                regle = %version.rule,
                indisponible = ?version.unavailable_source,
                jouee_source = %jouee.source,
                jouee_track_id = ?jouee.track_id,
                jouee_source_id = ?jouee.source_id,
                "version_de_repli"
            );
        }
        Some(version)
    }

    /// Seules les pistes de la bibliothèque (ligne locale) et celles des
    /// services de [`SERVICES_DE_VERSIONS`] ont d'autres exemplaires.
    async fn eligible(&self, identite: &Identite) -> bool {
        if identite.source == "local" {
            let Some(id) = identite.track_id else {
                return false;
            };
            // Une ligne indexée depuis un serveur UPnP n'est pas locale.
            return self
                .source_de_la_ligne(id)
                .is_none_or(|s| s.eq_ignore_ascii_case("local"));
        }
        identite.source_id.as_deref().is_some_and(|s| !s.is_empty())
            && SERVICES_DE_VERSIONS.contains(&identite.source.as_str())
    }

    /// Rassemble les exemplaires, groupe, choisit. `None` : la référence est
    /// introuvable.
    async fn decider(
        &self,
        identite: &Identite,
        indices: &Indices,
        regle: &RegleDeChoix,
    ) -> Option<Decision> {
        // 1. La référence, en tête (indice 0).
        let mut fichiers: Vec<Option<String>> = Vec::new();
        let mut pistes_de_service: Vec<Option<StreamTrack>> = Vec::new();
        let reference = if let Some(id) = identite.track_id {
            let (e, ligne) = base::lire_piste(&self.db, id)?;
            fichiers.push(ligne.get(base::COL_FICHIER).and_then(|v| v.as_string()));
            pistes_de_service.push(None);
            e
        } else {
            let service = identite.source.clone();
            let sid = identite.source_id.clone()?;
            let fiche = self.fiche_de_service(&service, &sid).await;
            let mut e = match &fiche {
                Some(t) => exemplaire_de_service(&service, t),
                None => Exemplaire {
                    source: service.clone(),
                    source_id: Some(sid.clone()),
                    ..Default::default()
                },
            };
            // Ce que la demande sait comble ce que la fiche n'a pas dit.
            if e.titre.is_empty() {
                e.titre = indices.titre.clone().unwrap_or_default();
            }
            if e.artiste.is_empty() {
                e.artiste = indices.artiste.clone().unwrap_or_default();
            }
            if e.album.is_empty() {
                e.album = indices.album.clone().unwrap_or_default();
            }
            if e.duree_ms.is_none() {
                e.duree_ms = indices.duree_ms.filter(|d| *d > 0);
            }
            fichiers.push(None);
            // La référence n'est jamais une cible : rien à garder d'elle.
            pistes_de_service.push(None);
            e
        };

        let mut exemplaires = vec![reference.clone()];
        let mut vus_locaux: std::collections::HashSet<i64> =
            identite.track_id.into_iter().collect();
        let mut vus_service: std::collections::HashSet<(String, String)> = identite
            .source_id
            .clone()
            .map(|s| (identite.source.clone(), s))
            .into_iter()
            .collect();

        // 2. La bibliothèque : par identifiant, puis par artiste et durée.
        let locales =
            base::pistes_par_identifiant(&self.db, &reference, identite.track_id, PLAFOND_LOCAL)
                .into_iter()
                .chain(base::pistes_locales_par_titre(
                    &self.db,
                    &reference,
                    identite.track_id,
                    PLAFOND_LOCAL,
                ));
        for (e, ligne) in locales {
            if !e.est_local() {
                continue;
            }
            if let Some(id) = e.track_id
                && vus_locaux.insert(id)
            {
                fichiers.push(ligne.get(base::COL_FICHIER).and_then(|v| v.as_string()));
                pistes_de_service.push(None);
                exemplaires.push(e);
            }
        }

        // 3. Les services dont la règle a besoin.
        let a_interroger: Vec<String> = match regle {
            RegleDeChoix::Aucune | RegleDeChoix::PrefererLocal => Vec::new(),
            RegleDeChoix::MeilleureQualite => {
                SERVICES_DE_VERSIONS.iter().map(|s| s.to_string()).collect()
            }
            RegleDeChoix::PrefererService(s) => vec![s.clone()],
        };
        let requete = format!("{} {}", reference.artiste, reference.titre)
            .trim()
            .to_string();
        let mut muets: Vec<String> = Vec::new();
        if !a_interroger.is_empty() && !reference.titre.is_empty() {
            let reponses = self
                .rechercher_dans_les_services(&a_interroger, &requete)
                .await;
            for (service, pistes) in reponses {
                let Some(pistes) = pistes else {
                    muets.push(service);
                    continue;
                };
                for t in pistes {
                    if vus_service.insert((service.clone(), t.id.clone())) {
                        exemplaires.push(exemplaire_de_service(&service, &t));
                        fichiers.push(None);
                        pistes_de_service.push(Some(t));
                    }
                }
            }
        }

        // 4. Le groupe de la référence, puis la disponibilité de ses seuls
        //    membres (un `exists()` par fichier local, pas plus).
        let groupe = grouper(&exemplaires)
            .into_iter()
            .find(|g| g.membres.iter().any(|m| m.indice == 0))?;
        let membres: Vec<usize> = groupe.membres.iter().map(|m| m.indice).collect();
        for &i in &membres {
            if let Some(id) = exemplaires[i].track_id {
                let present = fichier_local_present(&self.db, id, fichiers[i].as_deref());
                exemplaires[i].disponible = Some(present);
            }
        }
        let choix = choisir_avec_repli(&exemplaires, &membres, regle);

        // 5. Le repli : la préférée indisponible, ou le service préféré muet
        //    (non connecté, en erreur, hors délai) alors que la version jouée
        //    n'est pas de lui.
        let jouee = choix.map(|c| c.indice);
        let mut fallback = choix.is_some_and(|c| c.repli());
        let mut indisponible = choix
            .and_then(|c| c.prefere_indisponible)
            .map(|i| exemplaires[i].source.clone());
        if let RegleDeChoix::PrefererService(s) = regle
            && muets.contains(s)
            && jouee.is_none_or(|i| !exemplaires[i].source.eq_ignore_ascii_case(s))
        {
            fallback = true;
            indisponible = Some(s.clone());
        }

        let cible = match jouee {
            Some(0) | None => None,
            Some(i) => match (exemplaires[i].track_id, pistes_de_service[i].clone()) {
                (Some(id), _) => Some(Cible::Locale(id)),
                (None, Some(t)) => Some(Cible::Service {
                    source: exemplaires[i].source.clone(),
                    piste: Box::new(t),
                }),
                (None, None) => None,
            },
        };
        Some(Decision {
            version: VersionJouee {
                origin: "rule".into(),
                rule: regle.texte(),
                rule_origin: String::new(),
                requested: cible.as_ref().map(|_| identite.piste_demandee()),
                fallback,
                unavailable_source: indisponible.filter(|_| fallback),
            },
            cible,
        })
    }

    /// La fiche d'une piste de service (pour son ISRC et sa qualité), en
    /// cache, dans le budget. `None` : service absent, muet ou hors délai.
    async fn fiche_de_service(&self, service: &str, source_id: &str) -> Option<StreamTrack> {
        let cle = (service.to_string(), source_id.to_string());
        if let Some(v) = lire_cache(&self.versions.fiches, &cle) {
            return v;
        }
        let arc = { self.services.lock().await.get(service) }?;
        let cache = self.versions.fiches.clone();
        let sid = source_id.to_string();
        let tache = tokio::spawn(async move {
            let svc = arc.read().await;
            if !svc.utilisable().await {
                return None;
            }
            let fiche = svc.get_track(&sid).await.ok();
            if let Ok(mut c) = cache.lock() {
                c.insert(cle, (Instant::now(), fiche.clone()));
            }
            fiche
        });
        tokio::time::timeout(BUDGET_DES_SERVICES, tache)
            .await
            .ok()
            .and_then(Result::ok)
            .flatten()
    }

    /// Une recherche par service, en parallèle, dans le budget. Rend, pour
    /// chaque service, ses pistes — ou `None` quand il est absent, non
    /// connecté, en erreur ou hors délai (« muet »).
    async fn rechercher_dans_les_services(
        &self,
        services: &[String],
        requete: &str,
    ) -> Vec<(String, Option<Vec<StreamTrack>>)> {
        let mut taches = Vec::new();
        for service in services {
            let cle = (service.clone(), requete.to_lowercase());
            if let Some(v) = lire_cache(&self.versions.recherches, &cle) {
                taches.push((service.clone(), None, Some(v)));
                continue;
            }
            let arc = { self.services.lock().await.get(service) };
            let Some(arc) = arc else {
                taches.push((service.clone(), None, None));
                continue;
            };
            let cache = self.versions.recherches.clone();
            let q = requete.to_string();
            let tache = tokio::spawn(async move {
                let svc = arc.read().await;
                if !svc.utilisable().await {
                    return None;
                }
                let pistes = svc.search(&q, RESULTATS_PAR_SERVICE).await.ok()?.tracks;
                if let Ok(mut c) = cache.lock() {
                    c.insert(cle, (Instant::now(), pistes.clone()));
                }
                Some(pistes)
            });
            taches.push((service.clone(), Some(tache), None));
        }
        let echeance = tokio::time::Instant::now() + BUDGET_DES_SERVICES;
        let mut rendu = Vec::new();
        for (service, tache, en_cache) in taches {
            let pistes = match (tache, en_cache) {
                (_, Some(v)) => Some(v),
                (Some(t), None) => tokio::time::timeout_at(echeance, t)
                    .await
                    .ok()
                    .and_then(Result::ok)
                    .flatten(),
                (None, None) => None,
            };
            rendu.push((service, pistes));
        }
        rendu
    }

    fn reecrire_la_demande(req: &mut PlayRequest, cible: &Cible) {
        // Tout ce qui décrivait la piste DEMANDÉE est effacé : la résolution
        // et la piste en cours doivent décrire celle qui JOUE.
        req.title = None;
        req.artist_name = None;
        req.album_title = None;
        req.cover_url = None;
        req.duration_ms = None;
        req.sample_rate = None;
        req.bit_depth = None;
        req.media_format = None;
        req.track_number = None;
        req.disc_number = None;
        req.album_ref = None;
        match cible {
            Cible::Locale(id) => {
                req.track_id = Some(*id);
                req.source = None;
                req.source_id = None;
            }
            Cible::Service { source, piste } => {
                req.track_id = None;
                req.source = Some(source.clone());
                req.source_id = Some(piste.id.clone());
                req.title = Some(piste.title.clone());
                req.artist_name = Some(piste.artist.clone());
                req.album_title = piste.album.clone();
                req.cover_url = piste.cover_path.clone();
                req.duration_ms = (piste.duration_ms > 0).then_some(piste.duration_ms as i64);
                req.track_number = piste.track_number;
                req.disc_number = piste.disc_number;
                req.album_ref = piste.album_id.clone();
            }
        }
    }
}

#[cfg(test)]
#[path = "version_de_lecture_tests.rs"]
mod tests;
