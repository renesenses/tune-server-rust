//! Persistance des refus 429 du cloud Tune.
//!
//! Un processus redemarre oublie un `sleep` en memoire. Les boucles cloud
//! demarrent justement apres chaque lancement : sans etat persistant, une
//! mise a jour ou plusieurs redemarrages reemettraient aussitot les memes
//! requetes refusees (#2642).

use reqwest::header::HeaderMap;
use serde::Serialize;

use crate::db::settings_repo::SettingsRepo;

const PREFIX: &str = "cloud_rate_limit_until:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloudScope {
    Telemetry,
    InstanceHeartbeat,
    BiosWrite,
    BiosArtistsRead,
    BiosAlbumsRead,
    CommunityResolve,
    CommunityTracks,
    CommunityEnriched,
    CommunityExtraWrite,
    CommunityExtraRead,
    /// `POST /cloud-library/{server}/sync` : la synchro de bibliothèque (CLD-1).
    LibrarySync,
    /// `GET /cloud-library/{server}/proposals` : les propositions reçues (CLD-1).
    MetadataProposalsRead,
    /// `POST /cloud-library/{server}/proposals/decisions` : les décisions renvoyées (CLD-1).
    MetadataDecisionsWrite,
}

impl CloudScope {
    pub const ALL: [Self; 13] = [
        Self::Telemetry,
        Self::InstanceHeartbeat,
        Self::BiosWrite,
        Self::BiosArtistsRead,
        Self::BiosAlbumsRead,
        Self::CommunityResolve,
        Self::CommunityTracks,
        Self::CommunityEnriched,
        Self::CommunityExtraWrite,
        Self::CommunityExtraRead,
        Self::LibrarySync,
        Self::MetadataProposalsRead,
        Self::MetadataDecisionsWrite,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Telemetry => "telemetry",
            Self::InstanceHeartbeat => "instance_heartbeat",
            Self::BiosWrite => "bios_write",
            Self::BiosArtistsRead => "bios_artists_read",
            Self::BiosAlbumsRead => "bios_albums_read",
            Self::CommunityResolve => "community_resolve",
            Self::CommunityTracks => "community_tracks",
            Self::CommunityEnriched => "community_enriched",
            Self::CommunityExtraWrite => "community_extra_write",
            Self::CommunityExtraRead => "community_extra_read",
            Self::LibrarySync => "library_sync",
            Self::MetadataProposalsRead => "metadata_proposals_read",
            Self::MetadataDecisionsWrite => "metadata_decisions_write",
        }
    }

    fn key(self) -> String {
        format!("{PREFIX}{}", self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ActiveCloudBackoff {
    pub scope: &'static str,
    pub until_epoch: u64,
    pub retry_after_seconds: u64,
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Lit le delai standard de Laravel. La forme HTTP-date n'est pas devinee :
/// mozaiklabs emet des delta-secondes et `X-RateLimit-Reset` fournit le repli.
pub fn retry_after_secs(headers: &HeaderMap) -> Option<u64> {
    if let Some(secs) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        return (secs > 0).then_some(secs);
    }

    let reset = headers
        .get("x-ratelimit-reset")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    let now = now_epoch();
    if reset > now { Some(reset - now) } else { None }
}

/// Memorise jusqu'a quand ce sous-systeme doit se taire. Sans en-tete
/// exploitable, rien n'est invente : l'appelant arrete tout de meme son cycle,
/// mais le prochain cycle reste libre de retenter.
pub fn defer_from_headers(
    settings: &SettingsRepo,
    scope: CloudScope,
    headers: &HeaderMap,
) -> Option<ActiveCloudBackoff> {
    let retry_after_seconds = retry_after_secs(headers)?;
    let until_epoch = now_epoch().saturating_add(retry_after_seconds);
    settings.set(&scope.key(), &until_epoch.to_string()).ok()?;
    Some(ActiveCloudBackoff {
        scope: scope.as_str(),
        until_epoch,
        retry_after_seconds,
    })
}

/// Rend le delai encore actif et efface paresseusement une echeance passee.
pub fn active(settings: &SettingsRepo, scope: CloudScope) -> Option<ActiveCloudBackoff> {
    let key = scope.key();
    let until_epoch = settings.get(&key).ok().flatten()?.parse::<u64>().ok()?;
    let now = now_epoch();
    if until_epoch <= now {
        settings.delete(&key).ok();
        return None;
    }
    Some(ActiveCloudBackoff {
        scope: scope.as_str(),
        until_epoch,
        retry_after_seconds: until_epoch - now,
    })
}

pub fn active_all(settings: &SettingsRepo) -> Vec<ActiveCloudBackoff> {
    CloudScope::ALL
        .into_iter()
        .filter_map(|scope| active(settings, scope))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;
    use reqwest::header::HeaderValue;
    use std::sync::Arc;

    fn settings() -> SettingsRepo {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        SettingsRepo::with_backend(Arc::new(db))
    }

    #[test]
    fn retry_after_survit_a_un_nouveau_repo() {
        let settings = settings();
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            HeaderValue::from_static("120"),
        );

        let pose = defer_from_headers(&settings, CloudScope::BiosWrite, &headers).unwrap();
        let relu = active(&settings, CloudScope::BiosWrite).unwrap();

        assert_eq!(relu.scope, "bios_write");
        assert_eq!(relu.until_epoch, pose.until_epoch);
        assert!((119..=120).contains(&relu.retry_after_seconds));
    }

    #[test]
    fn un_refus_de_bios_ne_bloque_pas_les_pistes() {
        let settings = settings();
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, HeaderValue::from_static("60"));

        defer_from_headers(&settings, CloudScope::BiosWrite, &headers).unwrap();

        assert!(active(&settings, CloudScope::BiosWrite).is_some());
        assert!(active(&settings, CloudScope::CommunityTracks).is_none());
        assert_eq!(active_all(&settings).len(), 1);
    }

    #[test]
    fn une_echeance_passee_est_oubliee() {
        let settings = settings();
        settings.set(&CloudScope::Telemetry.key(), "1").unwrap();

        assert!(active(&settings, CloudScope::Telemetry).is_none());
        assert_eq!(settings.get(&CloudScope::Telemetry.key()).unwrap(), None);
    }

    #[test]
    fn aucun_delai_n_est_invente_sans_entete() {
        let settings = settings();
        assert_eq!(
            defer_from_headers(&settings, CloudScope::Telemetry, &HeaderMap::new()),
            None
        );
        assert!(active_all(&settings).is_empty());
    }

    /// Une portée oubliée dans `ALL` serait posée par `defer_from_headers`
    /// mais invisible du diagnostic (`active_all`) : chaque variante doit y
    /// figurer, avec une clef qui ne collisionne avec aucune autre.
    #[test]
    fn chaque_portee_figure_dans_all_avec_sa_propre_clef() {
        let settings = settings();
        let mut headers = HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, HeaderValue::from_static("30"));
        for scope in CloudScope::ALL {
            defer_from_headers(&settings, scope, &headers).unwrap();
        }
        let actives = active_all(&settings);
        assert_eq!(actives.len(), CloudScope::ALL.len());
        let mut noms: Vec<&str> = actives.iter().map(|a| a.scope).collect();
        noms.sort_unstable();
        noms.dedup();
        assert_eq!(
            noms.len(),
            CloudScope::ALL.len(),
            "deux portees partagent une clef"
        );
        for attendu in [
            "library_sync",
            "metadata_proposals_read",
            "metadata_decisions_write",
        ] {
            assert!(noms.contains(&attendu), "portee CLD-1 absente : {attendu}");
        }
    }
}
