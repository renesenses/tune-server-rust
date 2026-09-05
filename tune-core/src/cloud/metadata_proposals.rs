//! Aller-retour d'arbitrage des metadonnees avec mozaiklabs.fr.
//!
//! La communaute elit une valeur ; le cloud la propose aux serveurs qui en
//! portent une autre ; l'utilisateur tranche. Rien n'est applique sans son
//! accord — sauf s'il a lui-meme active la bascule automatique, qui est un
//! reglage LOCAL : le cloud n'a pas a savoir laquelle des deux voies a ete
//! empruntee, il recoit une decision dans les deux cas.
//!
//! Local d'abord, comme les signalements. Une decision prise hors ligne est
//! enregistree et appliquee tout de suite ; sa remontee au cloud est un effet
//! de bord, retente au cycle suivant. Un cloud injoignable ne doit jamais
//! empecher quelqu'un de corriger sa propre bibliotheque.

use std::sync::Arc;

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::cloud::rate_limit::{self, CloudScope};
use crate::db::album_repo::AlbumRepo;
use crate::db::backend::DbBackend;
use crate::db::metadata_proposal_repo::{
    MetadataProposal, MetadataProposalRepo, PROPOSABLE_FIELDS,
};
use crate::db::settings_repo::SettingsRepo;

const CLOUD_LIBRARY_API: &str = "https://mozaiklabs.fr/api/v1/cloud-library";

/// Combien de propositions on accepte de recevoir en un cycle.
const FETCH_LIMIT: usize = 200;

/// Reglage local : appliquer sans demander.
pub const AUTO_APPLY_SETTING: &str = "metadata_proposals_auto_apply";

#[derive(Debug, Deserialize)]
struct ProposalPayload {
    proposals: Vec<CloudProposal>,
}

#[derive(Debug, Deserialize)]
struct CloudProposal {
    entity_type: String,
    entity_id: i64,
    /// Identifiant LOCAL de l'album chez nous — le cloud nous le renvoie tel
    /// qu'on le lui a synchronise, ce qui evite tout re-rapprochement.
    remote_id: i64,
    title: Option<String>,
    artist: Option<String>,
    field: String,
    current: Option<String>,
    proposed: Option<String>,
    servers_count: i64,
}

/// Ce qu'un cycle a produit. Sert au journal et aux tests.
#[derive(Debug, Default, PartialEq)]
pub struct ProposalCycle {
    pub fetched: usize,
    pub stored: usize,
    /// Ecartees parce que le champ n'est pas dans `PROPOSABLE_FIELDS`.
    pub rejected: usize,
    pub auto_applied: usize,
    pub decisions_pushed: usize,
}

/// Applique une proposition a la bibliotheque locale.
///
/// Rendue publique parce que l'ecran de validation s'en sert aussi : la
/// decision de l'utilisateur et la bascule automatique doivent produire
/// exactement le meme effet, sinon les deux chemins divergeraient a la
/// premiere evolution.
pub fn apply(backend: &Arc<dyn DbBackend>, proposal: &MetadataProposal) -> Result<(), String> {
    if !PROPOSABLE_FIELDS.contains(&proposal.field.as_str()) {
        return Err(format!("champ non applicable : {}", proposal.field));
    }

    match proposal.field.as_str() {
        "year" => {
            let annee: i32 = proposal
                .proposed_value
                .as_deref()
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| "annee proposee illisible".to_string())?;

            AlbumRepo::with_backend(backend.clone())
                .set_year(proposal.local_id, annee)
                .map_err(|e| e.to_string())
        }
        autre => Err(format!("champ non applicable : {autre}")),
    }
}

/// Enregistre la reponse de l'utilisateur et, si elle est positive, l'applique.
///
/// L'ordre compte : on applique AVANT de marquer la decision. Si l'ecriture
/// echoue, la proposition reste en attente plutot que d'etre comptee comme
/// acceptee sans que rien n'ait change — un mensonge silencieux serait pire
/// qu'une erreur visible.
pub fn decide(
    backend: &Arc<dyn DbBackend>,
    proposal_id: i64,
    accept: bool,
    now: &str,
) -> Result<MetadataProposal, String> {
    let repo = MetadataProposalRepo::with_backend(backend.clone());
    let proposal = repo
        .get(proposal_id)?
        .ok_or_else(|| format!("proposition {proposal_id} introuvable"))?;

    if accept {
        apply(backend, &proposal)?;
    }

    repo.decide(
        proposal_id,
        if accept { "accepted" } else { "refused" },
        now,
    )?;

    repo.get(proposal_id)?
        .ok_or_else(|| "proposition disparue".to_string())
}

fn auto_apply_enabled(backend: &Arc<dyn DbBackend>) -> bool {
    SettingsRepo::with_backend(backend.clone())
        .get(AUTO_APPLY_SETTING)
        .ok()
        .flatten()
        .is_some_and(|v| v == "true" || v == "1")
}

/// Un cycle complet : recuperer, stocker, appliquer si demande, renvoyer.
pub async fn run_cycle(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    server_id: &str,
    access_token: &str,
    now: &str,
) -> Result<ProposalCycle, String> {
    let mut cycle = ProposalCycle::default();
    // Un 429 du cloud est persisté en base (CLD-1) : on respecte son
    // `Retry-After`, redémarrage compris, sans rappeler le serveur.
    let settings = SettingsRepo::with_backend(backend.clone());
    if let Some(backoff) = rate_limit::active(&settings, CloudScope::MetadataProposalsRead) {
        debug!(
            scope = backoff.scope,
            until_epoch = backoff.until_epoch,
            retry_after_seconds = backoff.retry_after_seconds,
            "metadata_proposals_deferred_rate_limit"
        );
        return Ok(cycle);
    }
    let repo = MetadataProposalRepo::with_backend(backend.clone());

    // 1. Ce que la communaute propose.
    let url = format!("{CLOUD_LIBRARY_API}/{server_id}/proposals?limit={FETCH_LIMIT}");
    let resp = http_client
        .get(&url)
        .bearer_auth(access_token)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("appel propositions: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        // 429 et 5xx sont des conditions transitoires du cloud communautaire,
        // pas des pannes : on retentera au cycle suivant sans alarmer.
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            rate_limit::defer_from_headers(
                &settings,
                CloudScope::MetadataProposalsRead,
                resp.headers(),
            );
        }
        if status.as_u16() == 429 || status.is_server_error() {
            debug!(status = %status, "metadata_proposals_throttled");
            return Ok(cycle);
        }
        return Err(format!("propositions HTTP {status}"));
    }

    let payload: ProposalPayload = resp
        .json()
        .await
        .map_err(|e| format!("propositions illisibles: {e}"))?;

    cycle.fetched = payload.proposals.len();

    // 2. Les enregistrer. Un champ hors liste est compte et ignore : le client
    //    ne suit pas aveuglement ce que le serveur lui envoie.
    for p in &payload.proposals {
        if p.entity_type != "album" || !PROPOSABLE_FIELDS.contains(&p.field.as_str()) {
            cycle.rejected += 1;
            continue;
        }

        match repo.upsert(
            &p.entity_type,
            p.entity_id,
            p.remote_id,
            p.title.as_deref(),
            p.artist.as_deref(),
            &p.field,
            p.current.as_deref(),
            p.proposed.as_deref(),
            p.servers_count,
            now,
        ) {
            Ok(()) => cycle.stored += 1,
            Err(e) => warn!(field = %p.field, error = %e, "metadata_proposal_store_failed"),
        }
    }

    // 3. La bascule automatique. Elle traite les propositions EN ATTENTE, y
    //    compris celles arrivees aux cycles precedents : activer la bascule
    //    doit rattraper l'arriere, pas seulement valoir pour la suite.
    if auto_apply_enabled(backend) {
        for proposal in repo.list_pending(FETCH_LIMIT as i64)? {
            match decide(backend, proposal.id, true, now) {
                Ok(_) => cycle.auto_applied += 1,
                Err(e) => {
                    warn!(id = proposal.id, error = %e, "metadata_proposal_auto_apply_failed")
                }
            }
        }
    }

    // 4. Renvoyer les decisions que le cloud n'a pas encore recues — y compris
    //    celles prises hors ligne aux cycles precedents.
    cycle.decisions_pushed =
        push_decisions(backend, http_client, server_id, access_token, now).await?;

    Ok(cycle)
}

/// Remonte les decisions en attente. Par lot : une bascule automatique en
/// produit autant qu'il y avait de propositions.
pub async fn push_decisions(
    backend: &Arc<dyn DbBackend>,
    http_client: &reqwest::Client,
    server_id: &str,
    access_token: &str,
    now: &str,
) -> Result<usize, String> {
    let repo = MetadataProposalRepo::with_backend(backend.clone());
    let pending = repo.list_undelivered(FETCH_LIMIT as i64)?;

    if pending.is_empty() {
        return Ok(0);
    }
    let settings = SettingsRepo::with_backend(backend.clone());
    if let Some(backoff) = rate_limit::active(&settings, CloudScope::MetadataDecisionsWrite) {
        debug!(
            scope = backoff.scope,
            until_epoch = backoff.until_epoch,
            retry_after_seconds = backoff.retry_after_seconds,
            "metadata_decisions_deferred_rate_limit"
        );
        return Ok(0);
    }

    let decisions: Vec<serde_json::Value> = pending
        .iter()
        .map(|p| {
            serde_json::json!({
                "entity_type": p.entity,
                "entity_id": p.cloud_entity_id,
                "field": p.field,
                "proposed_value": p.proposed_value,
                "decision": p.decision,
            })
        })
        .collect();

    let resp = http_client
        .post(format!(
            "{CLOUD_LIBRARY_API}/{server_id}/proposals/decisions"
        ))
        .bearer_auth(access_token)
        .json(&serde_json::json!({ "decisions": decisions }))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("envoi decisions: {e}"))?;

    let status = resp.status();
    if !status.is_success() {
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            rate_limit::defer_from_headers(
                &settings,
                CloudScope::MetadataDecisionsWrite,
                resp.headers(),
            );
        }
        if status.as_u16() == 429 || status.is_server_error() {
            debug!(status = %status, "metadata_decisions_throttled");
            return Ok(0);
        }
        return Err(format!("decisions HTTP {status}"));
    }

    // Marquees remontees seulement apres l'accuse de reception : un echec
    // laisse la ligne en attente et la fait repartir au cycle suivant.
    for p in &pending {
        repo.mark_pushed(p.id, now).ok();
    }

    Ok(pending.len())
}

// ---------------------------------------------------------------------------
// spawn — tache de fond
// ---------------------------------------------------------------------------

/// Lance le cycle d'arbitrage. Toutes les six heures : le consensus evolue au
/// rythme des synchronisations nocturnes du cloud, pas a la minute, et une
/// proposition qui arrive six heures plus tard n'a rien perdu de sa valeur.
pub fn spawn(backend: Arc<dyn DbBackend>, license: Arc<crate::license::LicenseManager>) {
    let client = match crate::http::client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("Tune/2.0 (https://mozaiklabs.fr)")
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "metadata_proposals_client_build_failed");
            return;
        }
    };

    tokio::spawn(async move {
        // Apres la synchronisation de bibliotheque, qui demarre a 2 minutes :
        // demander des propositions sur un catalogue pas encore pousse ne
        // donnerait rien.
        tokio::time::sleep(std::time::Duration::from_secs(600)).await;

        loop {
            if license.is_premium().await {
                let settings = SettingsRepo::with_backend(backend.clone());
                let server_id = settings.get("server_id").ok().flatten().unwrap_or_default();
                let token = settings.get("mozaik_access_token").ok().flatten();

                if let (Some(token), false) = (token, server_id.is_empty()) {
                    let now = chrono::Utc::now().to_rfc3339();
                    match run_cycle(&backend, &client, &server_id, &token, &now).await {
                        Ok(cycle) if cycle.fetched > 0 || cycle.decisions_pushed > 0 => {
                            info!(
                                fetched = cycle.fetched,
                                stored = cycle.stored,
                                rejected = cycle.rejected,
                                auto_applied = cycle.auto_applied,
                                decisions_pushed = cycle.decisions_pushed,
                                "metadata_proposals_cycle_complete"
                            );
                        }
                        Ok(_) => debug!("metadata_proposals_cycle_empty"),
                        Err(e) => warn!(error = %e, "metadata_proposals_cycle_failed"),
                    }
                }
            }

            tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;
    use crate::db::sqlite::SqliteDb;

    fn setup() -> Arc<dyn DbBackend> {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        migrations::run_migrations(&db).unwrap();
        Arc::new(db)
    }

    /// CLD-1 : un `Retry-After` encore en cours, lu en base, suffit à ne PAS
    /// rappeler le cloud. Le client HTTP pointe une adresse injoignable :
    /// s'il était sollicité, `run_cycle` rendrait une erreur et non le cycle
    /// vide.
    #[tokio::test]
    async fn un_429_en_cours_retient_le_cycle_sans_appeler_le_cloud() {
        let backend = setup();
        let settings = SettingsRepo::with_backend(backend.clone());
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            reqwest::header::HeaderValue::from_static("600"),
        );
        rate_limit::defer_from_headers(&settings, CloudScope::MetadataProposalsRead, &headers)
            .expect("un Retry-After pose une echeance");

        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(200))
            .build()
            .unwrap();
        let cycle = run_cycle(
            &backend,
            &client,
            "srv-test",
            "jeton",
            "2026-09-05T00:00:00Z",
        )
        .await
        .expect("le cycle retenu n'est pas une erreur");
        assert_eq!(cycle.fetched, 0, "rien ne doit avoir ete demande au cloud");
    }

    /// Cree un album et renvoie son id local.
    fn album(backend: &Arc<dyn DbBackend>, titre: &str, annee: i32) -> i64 {
        // Un artiste reel : `albums.artist_id` porte une contrainte de cle
        // etrangere, un 0 de commodite la ferait echouer.
        let artiste = crate::db::artist_repo::ArtistRepo::with_backend(backend.clone())
            .get_or_create("Pink Floyd", None, None)
            .unwrap()
            .id
            .expect("artiste cree sans id");

        let album = AlbumRepo::with_backend(backend.clone())
            .get_or_create(titre, artiste, Some(annee))
            .unwrap();
        album.id.expect("album cree sans id")
    }

    fn propose(backend: &Arc<dyn DbBackend>, local_id: i64, courant: &str, propose: &str) -> i64 {
        let repo = MetadataProposalRepo::with_backend(backend.clone());
        repo.upsert(
            "album",
            777,
            local_id,
            Some("The Wall"),
            Some("Pink Floyd"),
            "year",
            Some(courant),
            Some(propose),
            4,
            "2026-08-12T10:00:00Z",
        )
        .unwrap();
        repo.list_pending(10).unwrap()[0].id
    }

    fn annee_de(backend: &Arc<dyn DbBackend>, id: i64) -> Option<i32> {
        AlbumRepo::with_backend(backend.clone())
            .get(id)
            .ok()
            .flatten()
            .and_then(|a| a.year)
    }

    #[test]
    fn accepter_ecrase_bien_l_annee_en_place() {
        // Le piege : `update_dates` fait un COALESCE et n'aurait rien change,
        // l'arbitrage aurait ete sans effet en silence.
        let backend = setup();
        let id = album(&backend, "The Wall", 1980);
        let pid = propose(&backend, id, "1980", "1979");

        let apres = decide(&backend, pid, true, "2026-08-12T11:00:00Z").unwrap();

        assert_eq!(annee_de(&backend, id), Some(1979));
        assert_eq!(apres.decision.as_deref(), Some("accepted"));
    }

    #[test]
    fn refuser_ne_touche_pas_a_la_bibliotheque() {
        let backend = setup();
        let id = album(&backend, "The Wall", 1980);
        let pid = propose(&backend, id, "1980", "1979");

        let apres = decide(&backend, pid, false, "2026-08-12T11:00:00Z").unwrap();

        assert_eq!(annee_de(&backend, id), Some(1980));
        assert_eq!(apres.decision.as_deref(), Some("refused"));
    }

    #[test]
    fn une_decision_prise_attend_sa_remontee() {
        let backend = setup();
        let id = album(&backend, "The Wall", 1980);
        let pid = propose(&backend, id, "1980", "1979");
        decide(&backend, pid, true, "2026-08-12T11:00:00Z").unwrap();

        let repo = MetadataProposalRepo::with_backend(backend.clone());
        let a_remonter = repo.list_undelivered(10).unwrap();

        assert_eq!(a_remonter.len(), 1);
        assert_eq!(a_remonter[0].cloud_entity_id, 777);
        assert!(a_remonter[0].pushed_at.is_none());
    }

    #[test]
    fn une_valeur_proposee_illisible_n_applique_rien_et_laisse_en_attente() {
        let backend = setup();
        let id = album(&backend, "The Wall", 1980);
        let repo = MetadataProposalRepo::with_backend(backend.clone());
        repo.upsert(
            "album",
            777,
            id,
            None,
            None,
            "year",
            Some("1980"),
            Some("mille neuf cent"),
            4,
            "2026-08-12T10:00:00Z",
        )
        .unwrap();
        let pid = repo.list_pending(10).unwrap()[0].id;

        assert!(decide(&backend, pid, true, "2026-08-12T11:00:00Z").is_err());
        assert_eq!(annee_de(&backend, id), Some(1980));
        // Toujours en attente : rien n'a ete compte comme accepte.
        assert_eq!(repo.count_pending(), 1);
    }

    #[test]
    fn la_bascule_automatique_est_desactivee_par_defaut() {
        let backend = setup();
        assert!(!auto_apply_enabled(&backend));

        SettingsRepo::with_backend(backend.clone())
            .set(AUTO_APPLY_SETTING, "true")
            .unwrap();
        assert!(auto_apply_enabled(&backend));
    }
}
