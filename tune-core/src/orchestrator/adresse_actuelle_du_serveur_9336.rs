//! #9336 — une piste indexée depuis un serveur multimédia se joue à
//! l'adresse ACTUELLE de ce serveur, pas à celle de l'indexation.
//!
//! L'URL de lecture d'une piste indexée vit dans l'instantané
//! (`track_metadata.upnp_res_url`), écrit à l'indexation. Sur le .18, un
//! album indexé depuis un autre serveur Tune en 192.168.1.15:8888 ; le
//! serveur est passé en 192.168.1.29:8888. L'instantané portait toujours
//! `http://192.168.1.15:8888/…/audio`, la garde de #4362 laissait passer (le
//! serveur est bien présent, sous sa nouvelle adresse), et l'Eversolo
//! recevait une URL morte : aucun son, et aucune erreur.
//!
//! L'identité de la piste ne dépend pas de l'adresse : `tracks.source_id`
//! porte l'UDN du serveur (`<udn>|<condensat>`). Le registre durable
//! `media_servers` suit, lui, l'emplacement du serveur : une `LOCATION` qui
//! change réenregistre l'appareil (`ssdp_location_changee_appareil_reenregistre`)
//! et l'observation réécrit `host`. À la résolution, l'hôte de l'URL de
//! l'instantané est donc remplacé par l'hôte que le registre connaît pour cet
//! UDN. Le schéma, le port et le chemin ne bougent pas : c'est l'adresse IP
//! qui change quand un serveur change de bail, et un serveur peut publier
//! ses flux sur un autre port que sa description.
//!
//! Rien n'est réécrit quand le registre ne connaît pas l'UDN, quand il n'a
//! pas d'hôte, ou quand l'hôte est déjà le même : l'URL part alors telle
//! quelle, comme avant.

use tracing::info;

use super::PlaybackOrchestrator;

/// `url` dont l'hôte est remplacé par `hote_actuel`. `None` quand il n'y a
/// rien à faire (même hôte, hôte vide) ou que l'URL ne se lit pas.
pub(super) fn sur_l_hote_actuel(url: &str, hote_actuel: &str) -> Option<String> {
    let hote = hote_actuel
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    if hote.is_empty() {
        return None;
    }
    let mut u = reqwest::Url::parse(url.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let ancien = u
        .host_str()?
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    if ancien.eq_ignore_ascii_case(hote) {
        return None;
    }
    match hote.parse::<std::net::IpAddr>() {
        Ok(ip) => u.set_ip_host(ip).ok()?,
        Err(_) => u.set_host(Some(hote)).ok()?,
    }
    Some(u.to_string())
}

/// L'hôte d'une `LOCATION` SSDP (`http://hôte:port/…`), pour un registre
/// dont la colonne `host` serait vide.
fn hote_de_la_location(location: &str) -> Option<String> {
    reqwest::Url::parse(location.trim())
        .ok()?
        .host_str()
        .map(str::to_string)
}

impl PlaybackOrchestrator {
    /// L'URL de lecture indexée `url` de la piste `track_id`, reportée sur
    /// l'emplacement actuel de son serveur (voir le module). `url` telle
    /// quelle à la moindre incertitude.
    pub(super) fn url_sur_l_emplacement_actuel(&self, track_id: i64, url: String) -> String {
        use crate::db::backend::ToSqlValue;
        let Some(source_id) = self
            .db
            .query_one(
                "SELECT source_id FROM tracks WHERE id = ?",
                &[&track_id as &dyn ToSqlValue],
            )
            .ok()
            .flatten()
            .and_then(|ligne| ligne.first().and_then(|v| v.as_string()))
        else {
            return url;
        };
        let Some(udn) = super::serveur_source_absent_4362::udn_de_la_piste(&source_id) else {
            return url;
        };
        let Some(serveur) =
            crate::db::media_server_repo::MediaServerRepo::with_backend(self.db.clone())
                .lister()
                .ok()
                .and_then(|registre| registre.into_iter().find(|s| s.udn == udn))
        else {
            return url;
        };
        let Some(hote) = serveur
            .host
            .clone()
            .filter(|h| !h.trim().is_empty())
            .or_else(|| hote_de_la_location(&serveur.location))
        else {
            return url;
        };
        match sur_l_hote_actuel(&url, &hote) {
            Some(nouvelle) => {
                info!(
                    track_id,
                    serveur = %serveur.name,
                    ancienne = %crate::confidentialite::masquer_url(&url),
                    nouvelle = %crate::confidentialite::masquer_url(&nouvelle),
                    "upnp_url_reportee_sur_l_emplacement_actuel_du_serveur"
                );
                nouvelle
            }
            None => url,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::backend::DbBackend;
    use crate::db::media_server_repo::{MediaServerRepo, ObservationServeurRecue};
    use crate::db::migrations::run_migrations;
    use crate::db::sqlite::SqliteDb;
    use crate::http::streamer::AudioStreamer;
    use crate::orchestrator::PlayRequest;
    use crate::outputs::registry::OutputRegistry;
    use crate::playback::PlaybackManager;
    use crate::streaming::registry::ServiceRegistry;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    const ID: i64 = 93_361;
    const UDN: &str = "uuid:tune-nas-9336";
    /// L'URL rangée à l'indexation : le serveur était en A.
    const EN_A: &str = "http://192.168.1.15:8888/api/v1/library/tracks/4242/audio";
    /// La même piste, servie depuis l'emplacement actuel B.
    const EN_B: &str = "http://192.168.1.29:8888/api/v1/library/tracks/4242/audio";

    fn orchestrateur(annonce_en: Option<&str>) -> PlaybackOrchestrator {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        db.execute_batch(&format!(
            "INSERT INTO tracks (id,title,source,source_id,format) \
             VALUES ({ID},'Une piste du NAS','upnp','{UDN}|85944171f73967e8','flac'); \
             INSERT INTO track_metadata (track_id,key,value) \
             VALUES ({ID},'upnp_res_url','{EN_A}');"
        ))
        .unwrap();
        let db: Arc<dyn DbBackend> = Arc::new(db);
        if let Some(hote) = annonce_en {
            MediaServerRepo::with_backend(db.clone())
                .enregistrer_observation(&ObservationServeurRecue {
                    udn: UDN.into(),
                    name: "Tune du NAS".into(),
                    device_type: "upnp_media_server".into(),
                    location: format!("http://{hote}:8888/upnp/description.xml"),
                    host: Some(hote.into()),
                    port: Some(8888),
                    ..Default::default()
                })
                .unwrap();
        }
        PlaybackOrchestrator::new(
            db,
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(Mutex::new(OutputRegistry::new())),
            None,
        )
    }

    fn demande() -> PlayRequest {
        PlayRequest {
            zone_id: 1,
            output_device_id: Some("dlna:renderer-1".into()),
            track_id: Some(ID),
            source: None,
            source_id: None,
            title: Some("Une piste du NAS".into()),
            artist_name: None,
            album_title: None,
            cover_url: None,
            duration_ms: Some(212_000),
            seek_ms: None,
            temp_file_path: None,
            sample_rate: Some(44_100),
            bit_depth: Some(16),
            media_format: None,
            track_number: None,
            disc_number: None,
            album_ref: None,
        }
    }

    /// **Le témoin de l'issue.** Indexé en A, annoncé en B : le renderer
    /// reçoit une URL en B. Sans le correctif, l'URL reste en A.
    #[tokio::test]
    async fn un_serveur_indexe_en_a_puis_annonce_en_b_joue_en_b_9336() {
        let orch = orchestrateur(Some("192.168.1.29"));
        let resolu = orch.resolve_stream(&demande()).await.unwrap();
        assert_eq!(resolu.source, "upnp");
        assert_eq!(
            resolu.url, EN_B,
            "l'URL doit suivre l'emplacement actuel du serveur, pas l'instantané"
        );
    }

    /// Le serveur n'a pas bougé : l'URL part à l'octet près.
    #[tokio::test]
    async fn un_serveur_qui_n_a_pas_bouge_garde_son_url_9336() {
        let orch = orchestrateur(Some("192.168.1.15"));
        let resolu = orch.resolve_stream(&demande()).await.unwrap();
        assert_eq!(resolu.url, EN_A);
    }

    /// Un serveur inconnu du registre : rien n'est inventé.
    #[tokio::test]
    async fn sans_registre_l_url_part_telle_quelle_9336() {
        let orch = orchestrateur(None);
        let resolu = orch.resolve_stream(&demande()).await.unwrap();
        assert_eq!(resolu.url, EN_A);
    }

    #[test]
    fn seul_l_hote_change_9336() {
        assert_eq!(
            sur_l_hote_actuel(
                "http://192.168.1.15:9790/minimserver/*/Musique/a%20b.flac?x=1",
                "192.168.1.29"
            )
            .as_deref(),
            Some("http://192.168.1.29:9790/minimserver/*/Musique/a%20b.flac?x=1")
        );
        assert_eq!(sur_l_hote_actuel(EN_A, "192.168.1.15"), None);
        assert_eq!(sur_l_hote_actuel(EN_A, " "), None);
        assert_eq!(
            sur_l_hote_actuel("file:///etc/passwd", "192.168.1.29"),
            None
        );
        assert_eq!(
            sur_l_hote_actuel(EN_A, "[fe80::1]").as_deref(),
            Some("http://[fe80::1]:8888/api/v1/library/tracks/4242/audio")
        );
        assert_eq!(
            sur_l_hote_actuel(EN_A, "nas.local").as_deref(),
            Some("http://nas.local:8888/api/v1/library/tracks/4242/audio")
        );
    }
}
