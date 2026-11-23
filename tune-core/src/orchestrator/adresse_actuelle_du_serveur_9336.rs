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
//!
//! **Deux gardes de plus (relecture rc14).** L'hôte de la `LOCATION` n'est pas
//! toujours celui qui sert les fichiers : un serveur derrière un mandataire,
//! dans un conteneur ou sur plusieurs interfaces publie ses `res` sur un
//! autre hôte que sa description, et un NAS qui s'annonce sur deux
//! interfaces fait alterner la dernière `LOCATION`. Réécrire toute URL
//! cassait ces cas-là. Le report n'a donc lieu que si :
//!
//! 1. l'URL indexée pointait sur l'hôte d'ANNONCE de l'époque, rangé à
//!    l'indexation sous [`CLE_HOTE_D_ANNONCE_UPNP`]. Une piste indexée avant
//!    cette clé (aucune valeur rangée) passe à la garde suivante seule ;
//! 2. et l'adresse d'origine ne répond plus : une connexion TCP à son
//!    hôte et à son port échoue dans [`DELAI_DE_LA_SONDE`]. On ne quitte
//!    jamais une adresse qui répond.

use std::time::Duration;

use tracing::{debug, info};

use super::PlaybackOrchestrator;
use crate::db::media_server_repo::{CLE_HOTE_D_ANNONCE_UPNP, MediaServerRepo};

/// Le temps laissé à l'adresse d'origine pour accepter une connexion TCP.
/// Sur un réseau local, une adresse vivante répond en quelques millisecondes ;
/// la sonde ne coûte ce délai qu'à une piste dont le serveur a bougé.
pub(super) const DELAI_DE_LA_SONDE: Duration = Duration::from_millis(800);

fn sans_crochets(hote: &str) -> &str {
    hote.trim().trim_start_matches('[').trim_end_matches(']')
}

/// `url` dont l'hôte est remplacé par `hote_actuel`. `None` quand il n'y a
/// rien à faire (même hôte, hôte vide) ou que l'URL ne se lit pas.
pub(super) fn sur_l_hote_actuel(url: &str, hote_actuel: &str) -> Option<String> {
    let hote = sans_crochets(hote_actuel);
    if hote.is_empty() {
        return None;
    }
    let mut u = reqwest::Url::parse(url.trim()).ok()?;
    if !matches!(u.scheme(), "http" | "https") {
        return None;
    }
    let ancien = sans_crochets(u.host_str()?).to_string();
    if ancien.eq_ignore_ascii_case(hote) {
        return None;
    }
    match hote.parse::<std::net::IpAddr>() {
        Ok(ip) => u.set_ip_host(ip).ok()?,
        Err(_) => u.set_host(Some(hote)).ok()?,
    }
    Some(u.to_string())
}

/// La règle de la première garde : `url` est-elle CANDIDATE au report vers
/// `hote_actuel` ? Elle l'est quand son hôte est celui de l'annonce de
/// l'époque (`hote_a_l_indexation`), ou quand aucune annonce n'a été rangée
/// (indexation antérieure). Rend la nouvelle URL, ou `None`.
pub(super) fn candidate_au_report(
    url: &str,
    hote_actuel: &str,
    hote_a_l_indexation: Option<&str>,
) -> Option<String> {
    if let Some(alors) = hote_a_l_indexation
        .map(sans_crochets)
        .filter(|h| !h.is_empty())
    {
        let hote_de_l_url = reqwest::Url::parse(url.trim())
            .ok()?
            .host_str()?
            .to_string();
        if !sans_crochets(&hote_de_l_url).eq_ignore_ascii_case(alors) {
            // Les fichiers viennent d'un autre hôte que la description :
            // l'adresse du serveur n'a rien à dire de celle des fichiers.
            return None;
        }
    }
    sur_l_hote_actuel(url, hote_actuel)
}

/// La seconde garde : l'adresse d'origine accepte-t-elle encore une
/// connexion TCP ? Une URL illisible compte comme « ne répond pas ».
pub(super) async fn l_adresse_repond(url: &str, delai: Duration) -> bool {
    let Ok(u) = reqwest::Url::parse(url.trim()) else {
        return false;
    };
    let (Some(hote), Some(port)) = (u.host_str(), u.port_or_known_default()) else {
        return false;
    };
    let hote = sans_crochets(hote).to_string();
    matches!(
        tokio::time::timeout(delai, tokio::net::TcpStream::connect((hote.as_str(), port))).await,
        Ok(Ok(_))
    )
}

impl PlaybackOrchestrator {
    fn valeur_de_l_instantane(&self, track_id: i64, cle: &str) -> Option<String> {
        use crate::db::backend::ToSqlValue;
        self.db
            .query_one(
                "SELECT value FROM track_metadata WHERE track_id = ? AND key = ?",
                &[&track_id as &dyn ToSqlValue, &cle.to_string()],
            )
            .ok()
            .flatten()
            .and_then(|ligne| ligne.first().and_then(|v| v.as_string()))
    }

    /// L'URL de lecture indexée `url` de la piste `track_id`, reportée sur
    /// l'emplacement actuel de son serveur (voir le module). `url` telle
    /// quelle à la moindre incertitude.
    pub(super) async fn url_sur_l_emplacement_actuel(&self, track_id: i64, url: String) -> String {
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
        let Some(hote) = MediaServerRepo::with_backend(self.db.clone()).hote_d_annonce(udn) else {
            return url;
        };
        let hote_a_l_indexation = self.valeur_de_l_instantane(track_id, CLE_HOTE_D_ANNONCE_UPNP);
        let Some(nouvelle) = candidate_au_report(&url, &hote, hote_a_l_indexation.as_deref())
        else {
            return url;
        };
        if l_adresse_repond(&url, DELAI_DE_LA_SONDE).await {
            debug!(
                track_id,
                ancienne = %crate::confidentialite::masquer_url(&url),
                "upnp_url_d_origine_joignable_gardee"
            );
            return url;
        }
        info!(
            track_id,
            udn = %udn,
            ancienne = %crate::confidentialite::masquer_url(&url),
            nouvelle = %crate::confidentialite::masquer_url(&nouvelle),
            "upnp_url_reportee_sur_l_emplacement_actuel_du_serveur"
        );
        nouvelle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::backend::DbBackend;
    use crate::db::media_server_repo::ObservationServeurRecue;
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
    /// L'emplacement de l'indexation (A) et l'emplacement actuel (B). Des
    /// adresses de bouclage : la sonde reste sur la machine du test.
    const A: &str = "127.0.0.1";
    const B: &str = "127.0.0.2";
    /// Un hôte de fichiers distinct de la description (mandataire, conteneur).
    const FICHIERS: &str = "127.0.0.3";

    fn chemin(hote: &str, port: u16) -> String {
        format!("http://{hote}:{port}/api/v1/library/tracks/4242/audio")
    }

    /// Un port de bouclage où personne n'écoute : la connexion est refusée.
    fn port_ferme() -> u16 {
        let l = std::net::TcpListener::bind((A, 0)).unwrap();
        l.local_addr().unwrap().port()
    }

    /// `url` indexée ; `hote_d_annonce_alors` rangé à l'indexation (ou non) ;
    /// `annonce_en` l'hôte que le registre connaît aujourd'hui (ou aucun).
    fn orchestrateur(
        url: &str,
        hote_d_annonce_alors: Option<&str>,
        annonce_en: Option<&str>,
    ) -> PlaybackOrchestrator {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        db.execute_batch(&format!(
            "INSERT INTO tracks (id,title,source,source_id,format) \
             VALUES ({ID},'Une piste du NAS','upnp','{UDN}|85944171f73967e8','flac'); \
             INSERT INTO track_metadata (track_id,key,value) \
             VALUES ({ID},'upnp_res_url','{url}');"
        ))
        .unwrap();
        if let Some(alors) = hote_d_annonce_alors {
            db.execute_batch(&format!(
                "INSERT INTO track_metadata (track_id,key,value) \
                 VALUES ({ID},'{CLE_HOTE_D_ANNONCE_UPNP}','{alors}');"
            ))
            .unwrap();
        }
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

    async fn url_resolue(orch: &PlaybackOrchestrator) -> String {
        let resolu = orch.resolve_stream(&demande()).await.unwrap();
        assert_eq!(resolu.source, "upnp");
        resolu.url
    }

    /// **Le témoin de l'issue.** Indexé en A (annonce A rangée), annoncé
    /// aujourd'hui en B, A ne répond plus : le renderer reçoit une URL en B.
    #[tokio::test]
    async fn un_serveur_indexe_en_a_puis_annonce_en_b_joue_en_b_9336() {
        let port = port_ferme();
        let orch = orchestrateur(&chemin(A, port), Some(A), Some(B));
        assert_eq!(
            url_resolue(&orch).await,
            chemin(B, port),
            "l'URL doit suivre l'emplacement actuel du serveur, pas l'instantané"
        );
    }

    /// Une piste indexée AVANT que l'annonce soit rangée (le .18) : le report
    /// a toujours lieu quand l'adresse d'origine ne répond plus.
    #[tokio::test]
    async fn indexation_anterieure_reportee_si_l_origine_ne_repond_plus_9336() {
        let port = port_ferme();
        let orch = orchestrateur(&chemin(A, port), None, Some(B));
        assert_eq!(url_resolue(&orch).await, chemin(B, port));
    }

    /// **Relecture rc14.** Le serveur sert ses fichiers depuis un autre hôte
    /// que sa description : l'URL n'est pas réécrite, même si l'hôte des
    /// fichiers ne répond pas à l'instant.
    #[tokio::test]
    async fn des_fichiers_servis_par_un_autre_hote_ne_sont_pas_reecrits_9336() {
        let url = chemin(FICHIERS, port_ferme());
        let orch = orchestrateur(&url, Some(A), Some(B));
        assert_eq!(url_resolue(&orch).await, url);
    }

    /// **Relecture rc14.** Un NAS qui s'annonce sur deux interfaces : la
    /// dernière annonce est en B, mais l'adresse indexée A répond toujours.
    /// On ne la quitte pas.
    #[tokio::test]
    async fn une_adresse_d_origine_qui_repond_est_gardee_9336() {
        let ecoute = tokio::net::TcpListener::bind((A, 0)).await.unwrap();
        let url = chemin(A, ecoute.local_addr().unwrap().port());
        let orch = orchestrateur(&url, Some(A), Some(B));
        assert_eq!(url_resolue(&orch).await, url);
        drop(ecoute);
    }

    /// Le serveur n'a pas bougé : l'URL part à l'octet près.
    #[tokio::test]
    async fn un_serveur_qui_n_a_pas_bouge_garde_son_url_9336() {
        let url = chemin(A, port_ferme());
        let orch = orchestrateur(&url, Some(A), Some(A));
        assert_eq!(url_resolue(&orch).await, url);
    }

    /// Un serveur inconnu du registre : rien n'est inventé.
    #[tokio::test]
    async fn sans_registre_l_url_part_telle_quelle_9336() {
        let url = chemin(A, port_ferme());
        let orch = orchestrateur(&url, Some(A), None);
        assert_eq!(url_resolue(&orch).await, url);
    }

    #[test]
    fn la_regle_de_l_hote_d_annonce_9336() {
        let en_a = "http://192.168.1.15:8888/x.flac";
        assert_eq!(
            candidate_au_report(en_a, "192.168.1.29", Some("192.168.1.15")).as_deref(),
            Some("http://192.168.1.29:8888/x.flac")
        );
        assert_eq!(
            candidate_au_report(en_a, "192.168.1.29", None).as_deref(),
            Some("http://192.168.1.29:8888/x.flac"),
            "indexation antérieure : candidate, la sonde tranchera"
        );
        assert_eq!(
            candidate_au_report(
                "http://10.0.0.5:9000/x.flac",
                "192.168.1.29",
                Some("192.168.1.15")
            ),
            None,
            "fichiers sur un autre hôte que la description"
        );
        assert_eq!(
            candidate_au_report("http://[fe80::1]:80/x", "fe80::2", Some("[fe80::1]")).as_deref(),
            Some("http://[fe80::2]/x")
        );
    }

    #[test]
    fn seul_l_hote_change_9336() {
        let en_a = "http://192.168.1.15:8888/api/v1/library/tracks/4242/audio";
        assert_eq!(
            sur_l_hote_actuel(
                "http://192.168.1.15:9790/minimserver/*/Musique/a%20b.flac?x=1",
                "192.168.1.29"
            )
            .as_deref(),
            Some("http://192.168.1.29:9790/minimserver/*/Musique/a%20b.flac?x=1")
        );
        assert_eq!(sur_l_hote_actuel(en_a, "192.168.1.15"), None);
        assert_eq!(sur_l_hote_actuel(en_a, " "), None);
        assert_eq!(
            sur_l_hote_actuel("file:///etc/passwd", "192.168.1.29"),
            None
        );
        assert_eq!(
            sur_l_hote_actuel(en_a, "[fe80::1]").as_deref(),
            Some("http://[fe80::1]:8888/api/v1/library/tracks/4242/audio")
        );
        assert_eq!(
            sur_l_hote_actuel(en_a, "nas.local").as_deref(),
            Some("http://nas.local:8888/api/v1/library/tracks/4242/audio")
        );
    }
}
