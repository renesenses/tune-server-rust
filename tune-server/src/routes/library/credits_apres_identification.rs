//! Les crédits MusicBrainz d'un album, lus JUSTE APRÈS son identification
//! (#4805, validé par Bertrand le 06/10/2026).
//!
//! L'identification garde la release en base (`musicbrainz_release_cache`,
//! avec les `inc` des crédits) : la lire tout de suite pour remplir
//! `track_credits` ne coûte, dans le cas courant, AUCUNE requête. Sans ce
//! branchement, les crédits n'arrivaient qu'à la passe manuelle
//! (`POST /system/enrich-credits`).
//!
//! # Quand
//!
//! Après une identification qui a CHANGÉ le pressage (`reidentified`) : le
//! pilote de lot, le bouton « Ré-identifier » et la passe AcoustID. Un
//! pressage retrouvé à l'identique (`unchanged`) ne relance rien, pas plus
//! qu'un `not_found`, un `ambiguous` ou un `no_tracks`.
//!
//! # Ce qui l'arrête
//!
//! * le réglage `credits_auto_enabled` à `"false"` — celui de la passe
//!   automatique des crédits (CRD-5), qu'il coupe de la même façon ;
//! * l'absence du droit `AutoEnrichment`, comme la passe automatique ;
//! * un `albums.credits_mb_at` posé depuis moins de
//!   [`DUREE_DE_VALIDITE_JOURS`] jours : ces crédits sont récents ;
//! * la passe manuelle par disque en cours : elle écrit la même table, et
//!   l'album reste candidat (`credits_mb_at` intact) pour la suivante.
//!
//! La suspension de l'enrichissement GARE la lecture jusqu'à la reprise.
//!
//! # Rythme et exécuteur
//!
//! Une lecture à la fois pour tout le serveur ([`FILE`]). Une requête ne part
//! que si la base n'a pas la release, et elle attend son créneau dans le
//! limiteur MusicBrainz PARTAGÉ (une par seconde). Les écritures en base
//! passent par `spawn_blocking`. Rien n'est écrit dans les fichiers audio :
//! seule la table `track_credits` et la colonne `albums.credits_mb_at`.

use std::sync::Arc;

use tracing::{debug, info, warn};
use tune_core::db::backend::{DbBackend, ToSqlValue};
use tune_core::metadata::credits_release::{self, BilanAlbum, TACHE_CREDITS_RELEASES};
use tune_core::metadata::musicbrainz_release::{self, INC_CREDITS_RELEASE, LectureRelease};
use tune_core::metadata::musicbrainz_release_cache::DUREE_DE_VALIDITE_JOURS;
use tune_core::taches_de_fond::{Tache, attendre_la_reprise};

use super::credits::{REGLAGE_CREDITS_AUTO, passe_auto_coupee};
use crate::state::AppState;

/// Une lecture de crédits à la fois, pour tout le serveur : un lot de mille
/// albums ne lance pas mille lectures concurrentes.
static FILE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Le seul verdict qui déclenche la lecture : le pressage a changé.
const VERDICT_PRESSAGE_CHANGE: &str = "reidentified";

/// Ce que la lecture a donné, pour le journal et les essais.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum IssueCredits {
    /// `credits_auto_enabled` vaut `"false"`.
    Desactivee,
    /// Pas de droit `AutoEnrichment`.
    SansDroit,
    /// L'album n'existe plus, ou ne porte pas de `musicbrainz_release_id`.
    SansPressage,
    /// `credits_mb_at` est posé et récent.
    DejaRecente,
    /// La passe manuelle par disque tourne : elle a la main sur la table.
    PasseManuelleEnCours,
    /// La release est lue et appliquée.
    Appliquee(BilanAlbum),
    /// MusicBrainz ne connaît pas ce pressage : laissé à la passe manuelle.
    Inconnue,
    /// Panne (503, réseau) : laissé à la passe suivante.
    Panne,
}

/// Branchement des identifications. Ne bloque pas l'appelant : la lecture
/// part sur une tâche à elle, après le verdict.
pub(crate) fn apres_identification(state: &AppState, album_id: i64, verdict: &str) {
    if verdict != VERDICT_PRESSAGE_CHANGE {
        return;
    }
    let state = state.clone();
    tokio::spawn(async move {
        let issue = crediter_l_album_par(&state, album_id, |id, inc| async move {
            musicbrainz_release::rate_limit_delay().await;
            musicbrainz_release::lire_release_brute(&id, inc).await
        })
        .await;
        debug!(album_id, ?issue, "credits_apres_identification");
    });
}

/// Le pressage de l'album et son curseur de crédits.
fn lire_l_album(backend: &Arc<dyn DbBackend>, album_id: i64) -> Option<(String, Option<String>)> {
    let ligne = backend
        .query_one(
            "SELECT musicbrainz_release_id, credits_mb_at FROM albums WHERE id = ?",
            &[&album_id as &dyn ToSqlValue],
        )
        .ok()
        .flatten()?;
    let pressage = ligne
        .first()
        .and_then(|v| v.as_string())
        .filter(|s| !s.trim().is_empty())?;
    let curseur = ligne.get(1).and_then(|v| v.as_string());
    Some((pressage, curseur))
}

/// Vrai pour un `credits_mb_at` posé il y a moins de
/// [`DUREE_DE_VALIDITE_JOURS`] jours. Absent ou illisible : à refaire.
pub(crate) fn credits_recents(
    curseur: Option<&str>,
    maintenant: chrono::DateTime<chrono::Utc>,
) -> bool {
    curseur
        .and_then(|c| chrono::DateTime::parse_from_rfc3339(c.trim()).ok())
        .is_some_and(|pose| {
            maintenant.signed_duration_since(pose.with_timezone(&chrono::Utc))
                < chrono::Duration::days(DUREE_DE_VALIDITE_JOURS)
        })
}

/// Le corps, transport en paramètre : `interroger` reçoit le MBID et les `inc`
/// à demander, et porte l'attente du créneau. Les essais y passent une
/// doublure qui compte les requêtes.
pub(crate) async fn crediter_l_album_par<F, Fut>(
    state: &AppState,
    album_id: i64,
    interroger: F,
) -> IssueCredits
where
    F: FnOnce(String, &'static str) -> Fut,
    Fut: std::future::Future<Output = LectureRelease>,
{
    let _tour = FILE.lock().await;

    if !state
        .license
        .check_feature(tune_core::license::Feature::AutoEnrichment)
        .await
    {
        return IssueCredits::SansDroit;
    }

    let backend = state.backend.clone();
    let lu = tokio::task::spawn_blocking(move || {
        let reglage = tune_core::db::settings_repo::SettingsRepo::with_backend(backend.clone())
            .get(REGLAGE_CREDITS_AUTO)
            .ok()
            .flatten();
        (passe_auto_coupee(reglage), lire_l_album(&backend, album_id))
    })
    .await;
    let (coupee, album) = match lu {
        Ok(l) => l,
        Err(e) => {
            warn!(album_id, error = %e, "credits_apres_identification_lecture_impossible");
            return IssueCredits::SansPressage;
        }
    };
    if coupee {
        return IssueCredits::Desactivee;
    }
    let Some((pressage, curseur)) = album else {
        return IssueCredits::SansPressage;
    };
    if credits_recents(curseur.as_deref(), chrono::Utc::now()) {
        return IssueCredits::DejaRecente;
    }
    if state
        .background_tasks
        .snapshot()
        .iter()
        .any(|t| t.id == TACHE_CREDITS_RELEASES)
    {
        return IssueCredits::PasseManuelleEnCours;
    }

    attendre_la_reprise(Tache::Enrichissement).await;

    let (lecture, provenance) = musicbrainz_release::lire_release_gardee_par(
        &state.backend,
        &pressage,
        INC_CREDITS_RELEASE,
        interroger,
    )
    .await;
    match lecture {
        LectureRelease::Lue(release) => {
            let backend = state.backend.clone();
            match tokio::task::spawn_blocking(move || {
                credits_release::appliquer_release(&backend, album_id, &release)
            })
            .await
            {
                Ok(bilan) => {
                    info!(
                        album_id,
                        release_id = %pressage,
                        ?provenance,
                        pistes_creditees = bilan.pistes_creditees,
                        pistes_sans_correspondance = bilan.pistes_sans_correspondance,
                        "credits_apres_identification_appliques"
                    );
                    IssueCredits::Appliquee(bilan)
                }
                Err(e) => {
                    warn!(album_id, error = %e, "credits_apres_identification_ecriture_impossible");
                    IssueCredits::Panne
                }
            }
        }
        LectureRelease::Inconnue => IssueCredits::Inconnue,
        LectureRelease::Panne(motif) => {
            debug!(album_id, motif = %motif, "credits_apres_identification_panne");
            IssueCredits::Panne
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    use super::*;

    /// Un album identifié, une piste portant le MBID d'enregistrement.
    async fn etat(curseur: Option<&str>) -> AppState {
        let state = AppState::new(":memory:", 0, Default::default()).unwrap();
        state.license.set_account_premium(true, None).await;
        state
            .backend
            .execute(
                "INSERT INTO albums (id, title, source, musicbrainz_release_id, credits_mb_at) \
                 VALUES (1, 'Kind of Blue', 'local', 'rel-kob', ?)",
                &[&curseur.map(str::to_string) as &dyn ToSqlValue],
            )
            .unwrap();
        state
            .backend
            .execute(
                "INSERT INTO tracks (id, album_id, title, track_number, disc_number, \
                 musicbrainz_recording_id, file_path) \
                 VALUES (10, 1, 'So What', 1, 1, 'rec-so-what', '/m/so_what.flac')",
                &[],
            )
            .unwrap();
        state
    }

    fn release() -> serde_json::Value {
        json!({
            "id": "rel-kob",
            "media": [{ "position": 1, "tracks": [{
                "position": 1,
                "title": "So What",
                "recording": {
                    "id": "rec-so-what",
                    "title": "So What",
                    "relations": [{
                        "type": "instrument",
                        "attributes": ["piano"],
                        "artist": { "id": "a-bill-evans", "name": "Bill Evans" }
                    }]
                }
            }]}]
        })
    }

    fn credits(state: &AppState) -> i64 {
        state
            .backend
            .query_one(
                "SELECT COUNT(*) FROM track_credits WHERE track_id = 10",
                &[],
            )
            .unwrap()
            .and_then(|r| r.first().and_then(|v| v.as_i64()))
            .unwrap_or(0)
    }

    async fn lancer(state: &AppState, appels: &Arc<AtomicUsize>) -> IssueCredits {
        let a = appels.clone();
        crediter_l_album_par(state, 1, move |_id, _inc| async move {
            a.fetch_add(1, Ordering::SeqCst);
            LectureRelease::Lue(release())
        })
        .await
    }

    #[tokio::test]
    async fn un_album_jamais_credite_recoit_ses_credits() {
        let state = etat(None).await;
        let appels = Arc::new(AtomicUsize::new(0));
        let issue = lancer(&state, &appels).await;
        assert!(
            matches!(issue, IssueCredits::Appliquee(ref b) if b.pistes_creditees == 1),
            "{issue:?}"
        );
        assert_eq!(appels.load(Ordering::SeqCst), 1);
        assert!(credits(&state) >= 1);
    }

    /// Des crédits récents ne se refont pas : aucune requête, aucune écriture.
    #[tokio::test]
    async fn des_credits_recents_ne_se_refont_pas() {
        let hier = (chrono::Utc::now() - chrono::Duration::days(1)).to_rfc3339();
        let state = etat(Some(&hier)).await;
        let appels = Arc::new(AtomicUsize::new(0));
        assert_eq!(lancer(&state, &appels).await, IssueCredits::DejaRecente);
        assert_eq!(appels.load(Ordering::SeqCst), 0);
        assert_eq!(credits(&state), 0);
    }

    #[tokio::test]
    async fn des_credits_anciens_se_refont() {
        let vieux =
            (chrono::Utc::now() - chrono::Duration::days(DUREE_DE_VALIDITE_JOURS + 1)).to_rfc3339();
        let state = etat(Some(&vieux)).await;
        let appels = Arc::new(AtomicUsize::new(0));
        assert!(matches!(
            lancer(&state, &appels).await,
            IssueCredits::Appliquee(_)
        ));
        assert!(credits(&state) >= 1);
    }

    /// Le réglage de la passe automatique des crédits coupe aussi celle-ci.
    #[tokio::test]
    async fn le_reglage_coupe_la_lecture() {
        let state = etat(None).await;
        tune_core::db::settings_repo::SettingsRepo::with_backend(state.backend.clone())
            .set(REGLAGE_CREDITS_AUTO, "false")
            .unwrap();
        let appels = Arc::new(AtomicUsize::new(0));
        assert_eq!(lancer(&state, &appels).await, IssueCredits::Desactivee);
        assert_eq!(appels.load(Ordering::SeqCst), 0);
        assert_eq!(credits(&state), 0);
    }

    #[tokio::test]
    async fn sans_droit_rien_ne_part() {
        let state = etat(None).await;
        state.license.set_account_premium(false, None).await;
        let appels = Arc::new(AtomicUsize::new(0));
        assert_eq!(lancer(&state, &appels).await, IssueCredits::SansDroit);
        assert_eq!(appels.load(Ordering::SeqCst), 0);
    }

    /// La release gardée en base par l'identification se relit sans requête.
    #[tokio::test]
    async fn la_release_gardee_ne_coute_aucune_requete() {
        let state = etat(None).await;
        tune_core::metadata::musicbrainz_release_cache::ecrire(
            &state.backend,
            "rel-kob",
            musicbrainz_release::INC_RELEASE_COMPLET,
            &release(),
            chrono::Utc::now(),
        );
        let appels = Arc::new(AtomicUsize::new(0));
        assert!(matches!(
            lancer(&state, &appels).await,
            IssueCredits::Appliquee(_)
        ));
        assert_eq!(appels.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn la_fraicheur_se_lit_sur_le_curseur() {
        let maintenant = chrono::Utc::now();
        assert!(!credits_recents(None, maintenant));
        assert!(!credits_recents(Some("pas une date"), maintenant));
        assert!(credits_recents(Some(&maintenant.to_rfc3339()), maintenant));
    }
}
