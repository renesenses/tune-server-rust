//! #9207 (fil 2962) — une URI de NOTRE bibliothèque, reçue par le
//! MediaRenderer, se joue comme une piste locale.
//!
//! Le rapport : une zone à sortie locale (ALSA, volume fixe, PURE) publiée en
//! MediaRenderer ; un point de contrôle parcourt le MediaServer de Tune
//! lui-même et lance une piste AIFF de la bibliothèque sur cette zone. Le
//! renderer reçoit `http://<nous>:<port>/api/v1/library/tracks/<id>/audio`,
//! la lance en `source = "upnp"`, et `resolve_direct_url` la confie telle
//! quelle à la sortie : l'AIFF brut servi par notre propre route, sondé par
//! symphonia → `ContainerUnrecognised`. Depuis l'interface web, le même
//! fichier passe par `resolve_local_track` (conversion WAV, bit-perfect, PURE)
//! et joue.
//!
//! #4323 relisait déjà le `track_id` dans cette URI, mais pour les seules
//! métadonnées : le chemin des OCTETS restait le flux HTTP. Ici, quand l'URI
//! est bien la nôtre, la résolution est celle de la piste locale. `source` et
//! `source_id` ne bougent pas (`upnp`, l'URI) : le renderer reconnaît sa
//! session en les comparant (`doit_reprendre`, `lecture_de_session`, la file
//! de la suivante), et la fin de file d'un point de contrôle ne doit pas
//! amorcer l'autoplay de la bibliothèque.
//!
//! Sécurité : on ne lit un fichier local QUE si l'URI désigne cette instance
//! (hôte reconnu SANS appel DNS, port du serveur, chemin exact, ligne locale
//! existante). Toute autre URI reste un flux distant, comme avant : une URL
//! d'un autre hôte ne fait jamais ouvrir un fichier de la machine.

use tracing::info;

use super::{PlayRequest, PlaybackOrchestrator, ResolvedStream};

/// L'identifiant de piste porté par `uri` quand elle désigne l'audio d'une
/// piste servie par CETTE instance, `None` sinon.
///
/// - schéma `http` ou `https` ;
/// - port (explicite, ou celui du schéma) = `port_du_serveur` ;
/// - hôte accepté par `hote_est_le_notre` ;
/// - chemin `{API_PATH}/library/tracks/<id>/audio`, éventuellement suivi
///   d'une extension (`/audio.aif`) ; requête et fragment ignorés.
pub(super) fn id_de_piste_de_cette_instance(
    uri: &str,
    port_du_serveur: u16,
    hote_est_le_notre: impl Fn(&reqwest::Url) -> bool,
) -> Option<i64> {
    let url = reqwest::Url::parse(uri.trim()).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if port_du_serveur == 0 || url.port_or_known_default() != Some(port_du_serveur) {
        return None;
    }
    // Pas d'identifiants dans l'URL : ce n'est pas ce que publie notre
    // MediaServer, et `user@hôte` est la forme classique d'un hôte déguisé.
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    if !hote_est_le_notre(&url) {
        return None;
    }
    let reste = url
        .path()
        .strip_prefix(crate::upnp_server::API_PATH)?
        .strip_prefix("/library/tracks/")?;
    let (id, fin) = reste.split_once('/')?;
    let fin_valide = fin == "audio"
        || fin
            .strip_prefix("audio.")
            .is_some_and(|ext| !ext.is_empty() && ext.chars().all(|c| c.is_ascii_alphanumeric()));
    if !fin_valide || id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    id.parse::<i64>().ok().filter(|id| *id > 0)
}

/// L'hôte de `url` désigne-t-il cette machine ?
///
/// Boucle locale, `localhost`, une adresse IPv4 portée par une interface,
/// l'adresse annoncée par la configuration (`advertised_ip`), ou le NOM de la
/// machine (`hostname`, `hostname.local`). Aucun appel DNS : un nom inconnu
/// est refusé, et le refus ne coûte que le flux distant d'avant.
pub(super) fn hote_de_cette_instance(
    url: &reqwest::Url,
    adresses_locales: &[std::net::Ipv4Addr],
    ip_annoncee: Option<&str>,
    nom_de_la_machine: &str,
) -> bool {
    let Some(hote) = url.host_str() else {
        return false;
    };
    let litteral = hote.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = litteral.parse::<std::net::IpAddr>() {
        return match ip {
            std::net::IpAddr::V4(ip) => {
                ip.is_loopback()
                    || adresses_locales.contains(&ip)
                    || ip_annoncee
                        .and_then(|a| a.trim().parse::<std::net::Ipv4Addr>().ok())
                        .is_some_and(|a| a == ip)
            }
            std::net::IpAddr::V6(ip) => ip.is_loopback(),
        };
    }
    let nom = hote.trim_end_matches('.').to_ascii_lowercase();
    if nom == "localhost" {
        return true;
    }
    let machine = nom_de_la_machine
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let court = machine.trim_end_matches(".local");
    if court.is_empty() {
        return false;
    }
    // Le nom tel que l'annonce mDNS le publie (`<étiquette>.local`).
    let etiquette = crate::discovery::mdns_host_label(&machine).to_ascii_lowercase();
    let sans_local = nom.strip_suffix(".local").unwrap_or(&nom);
    nom == machine || sans_local == court || sans_local == etiquette
}

impl PlaybackOrchestrator {
    /// La piste LOCALE que désigne une URI reçue par le renderer, quand cette
    /// URI est une adresse audio de cette instance et que la ligne existe
    /// sans venir d'un serveur UPnP indexé (dont le fichier n'est pas ici).
    pub(super) fn piste_locale_de_notre_uri(&self, uri: &str) -> Option<i64> {
        let adresses = crate::discovery::ssdp::local_ipv4_addresses();
        let machine = crate::discovery::system_hostname();
        let ip_annoncee = self.advertised_ip.as_deref();
        let id = id_de_piste_de_cette_instance(uri, self.streamer.port(), |url| {
            hote_de_cette_instance(url, &adresses, ip_annoncee, &machine)
        })?;
        self.ligne_locale(id).then_some(id)
    }

    /// La ligne existe-t-elle, et sa source est-elle locale (vide ou `local`) ?
    fn ligne_locale(&self, track_id: i64) -> bool {
        use crate::db::backend::ToSqlValue;
        match self.db.query_one(
            "SELECT source FROM tracks WHERE id = ?",
            &[&track_id as &dyn ToSqlValue],
        ) {
            Ok(Some(ligne)) => {
                let source = ligne
                    .first()
                    .and_then(|v| v.as_string())
                    .unwrap_or_default();
                source.is_empty() || source == "local"
            }
            _ => false,
        }
    }

    /// Résout une demande `upnp` dont l'URI est la nôtre comme la piste
    /// locale qu'elle désigne : même chemin que l'interface web
    /// (`resolve_local_track`), puis `source = "upnp"` sur le résultat, comme
    /// le DSD d'un serveur média servi en local (`dsd_upnp_sortie_locale`).
    pub(super) async fn resoudre_notre_uri_en_piste_locale(
        &self,
        req: &PlayRequest,
        track_de_l_uri: i64,
    ) -> Result<ResolvedStream, String> {
        // `play_inner` a déjà relu l'URI (#4323) puis appliqué la règle de
        // version (#2264), qui peut avoir choisi un autre exemplaire LOCAL de
        // l'enregistrement : il prime. Sinon (avance de file), celui de l'URI.
        let track_id = req
            .track_id
            .filter(|id| *id == track_de_l_uri || self.ligne_locale(*id))
            .unwrap_or(track_de_l_uri);
        let locale = PlayRequest {
            track_id: Some(track_id),
            source: None,
            source_id: None,
            temp_file_path: None,
            ..req.clone()
        };
        let mut resolu = Box::pin(self.resolve_local_track(&locale)).await?;
        resolu.source = "upnp".into();
        info!(
            zone_id = req.zone_id,
            track_id,
            mime = %resolu.mime_type,
            "upnp_uri_de_notre_bibliotheque_jouee_en_piste_locale"
        );
        Ok(resolu)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    const PORT: u16 = 8888;
    const LAN: Ipv4Addr = Ipv4Addr::new(192, 0, 2, 10);

    fn id(uri: &str) -> Option<i64> {
        id_de_piste_de_cette_instance(uri, PORT, |url| {
            hote_de_cette_instance(url, &[LAN], Some("192.0.2.77"), "studio")
        })
    }

    /// L'URI du rapport, à l'adresse près, et ses variantes.
    #[test]
    fn notre_uri_rend_la_piste_9207() {
        for uri in [
            "http://192.0.2.10:8888/api/v1/library/tracks/2982/audio",
            "  http://192.0.2.10:8888/api/v1/library/tracks/2982/audio  ",
            "http://192.0.2.10:8888/api/v1/library/tracks/2982/audio?format=aiff",
            "http://192.0.2.10:8888/api/v1/library/tracks/2982/audio#x",
            "http://192.0.2.10:8888/api/v1/library/tracks/2982/audio.aif",
            "https://192.0.2.10:8888/api/v1/library/tracks/2982/audio",
            "http://127.0.0.1:8888/api/v1/library/tracks/2982/audio",
            "http://[::1]:8888/api/v1/library/tracks/2982/audio",
            "http://localhost:8888/api/v1/library/tracks/2982/audio",
            "http://192.0.2.77:8888/api/v1/library/tracks/2982/audio",
            "http://studio:8888/api/v1/library/tracks/2982/audio",
            "http://Studio.local:8888/api/v1/library/tracks/2982/audio",
        ] {
            assert_eq!(
                id(uri),
                Some(2982),
                "« {uri} » devrait désigner la piste 2982"
            );
        }
    }

    /// Contre-épreuve : tout ce qui n'est pas NOUS reste un flux distant.
    #[test]
    fn une_autre_uri_ne_designe_aucune_piste_locale_9207() {
        for uri in [
            // Autre hôte (un second Tune du réseau, un NAS).
            "http://192.0.2.11:8888/api/v1/library/tracks/2982/audio",
            "http://198.51.100.7:8888/api/v1/library/tracks/2982/audio",
            "http://nas.local:8888/api/v1/library/tracks/2982/audio",
            "http://studio.example.org:8888/api/v1/library/tracks/2982/audio",
            // Hôte déguisé derrière des identifiants.
            "http://127.0.0.1:8888@198.51.100.7:8888/api/v1/library/tracks/2982/audio",
            "http://user:pw@127.0.0.1:8888/api/v1/library/tracks/2982/audio",
            // Autre port, port implicite.
            "http://127.0.0.1:9999/api/v1/library/tracks/2982/audio",
            "http://127.0.0.1/api/v1/library/tracks/2982/audio",
            // Autre chemin, identifiant illisible.
            "http://127.0.0.1:8888/api/v1/library/tracks/2982",
            "http://127.0.0.1:8888/api/v1/library/tracks/2982/audio/x",
            "http://127.0.0.1:8888/api/v1/library/tracks/abc/audio",
            "http://127.0.0.1:8888/api/v1/library/tracks/-3/audio",
            "http://127.0.0.1:8888/api/v1/library/tracks/0/audio",
            "http://127.0.0.1:8888/api/v1/library/tracks/../tracks/2982/audio.x/y",
            "http://127.0.0.1:8888/library/tracks/2982/audio",
            "http://127.0.0.1:8888/api/v1/radios/3/audio.wav",
            // Pas http(s).
            "file:///api/v1/library/tracks/2982/audio",
            "/api/v1/library/tracks/2982/audio",
            "",
        ] {
            assert_eq!(
                id(uri),
                None,
                "« {uri} » ne doit désigner aucune piste locale"
            );
        }
    }

    /// Un serveur dont le port est inconnu (0) ne reconnaît rien.
    #[test]
    fn port_inconnu_rien_n_est_a_nous_9207() {
        assert_eq!(
            id_de_piste_de_cette_instance(
                "http://127.0.0.1:8888/api/v1/library/tracks/1/audio",
                0,
                |_| true
            ),
            None
        );
    }

    /// Un nom de machine vide n'accepte pas n'importe quel nom.
    #[test]
    fn nom_de_machine_vide_9207() {
        let url = reqwest::Url::parse("http://x:8888/").unwrap();
        assert!(!hote_de_cette_instance(&url, &[], None, ""));
        let url = reqwest::Url::parse("http://local:8888/").unwrap();
        assert!(!hote_de_cette_instance(&url, &[], None, ".local"));
    }

    // ----- Bout en bout : `resolve_stream` et l'avance de file -----

    use std::sync::Arc;
    use tokio::sync::Mutex;

    use crate::db::migrations::run_migrations;
    use crate::db::models::Track;
    use crate::db::play_queue_repo::{PlayQueueRepo, StreamingQueueItem};
    use crate::db::sqlite::SqliteDb;
    use crate::db::track_repo::TrackRepo;
    use crate::db::zone_repo::ZoneRepo;
    use crate::http::streamer::AudioStreamer;
    use crate::outputs::registry::OutputRegistry;
    use crate::playback::PlaybackManager;
    use crate::streaming::registry::ServiceRegistry;

    const AIFF: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/aiff/ref_16_44100_stereo.aiff"
    );

    fn orchestrateur() -> PlaybackOrchestrator {
        let db = SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        run_migrations(&db).unwrap();
        let db: Arc<dyn crate::db::backend::DbBackend> = Arc::new(db);
        PlaybackOrchestrator::new(
            db,
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(PORT)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(Mutex::new(OutputRegistry::new())),
            None,
        )
    }

    fn piste_aiff(orch: &PlaybackOrchestrator, source: &str) -> i64 {
        let mut t = Track::new("Piste AIFF".into());
        t.file_path = Some(AIFF.into());
        t.format = Some("aiff".into());
        t.sample_rate = Some(44_100);
        t.bit_depth = Some(16);
        t.channels = 2;
        t.duration_ms = 1_000;
        t.source = source.into();
        TrackRepo::with_backend(orch.db.clone()).create(&t).unwrap()
    }

    fn requete_upnp(zone_id: i64, uri: &str) -> PlayRequest {
        PlayRequest {
            zone_id,
            source: Some("upnp".into()),
            source_id: Some(uri.into()),
            ..Default::default()
        }
    }

    /// Le cas du rapport : l'URI de notre propre bibliothèque est résolue
    /// comme la piste locale (une session du streamer, pas l'URI brute), et
    /// garde `source = "upnp"` pour la session du renderer.
    #[tokio::test]
    async fn notre_uri_aiff_se_resout_en_piste_locale_9207() {
        let orch = orchestrateur();
        let id = piste_aiff(&orch, "local");
        let uri = crate::upnp_server::track_audio_url("http://127.0.0.1:8888", id);
        assert_eq!(orch.piste_locale_de_notre_uri(&uri), Some(id));

        let resolu = orch
            .resolve_stream(&requete_upnp(41, &uri))
            .await
            .expect("la piste locale doit se résoudre");
        assert_eq!(
            resolu.source, "upnp",
            "la session du renderer se reconnaît à sa source"
        );
        assert_ne!(
            resolu.url, uri,
            "l'URI HTTP brute ne doit plus partir vers la sortie"
        );
        assert!(
            resolu.stream_id.is_some(),
            "le chemin local sert la piste par une session du streamer : {}",
            resolu.url
        );
    }

    /// Contre-épreuve : même chemin, AUTRE port — ce n'est pas nous, le flux
    /// distant d'avant demeure (l'URI part telle quelle).
    #[tokio::test]
    async fn une_uri_d_un_autre_serveur_reste_un_flux_distant_9207() {
        let orch = orchestrateur();
        let id = piste_aiff(&orch, "local");
        let uri = format!("http://127.0.0.1:9/api/v1/library/tracks/{id}/audio");
        assert_eq!(orch.piste_locale_de_notre_uri(&uri), None);
        let resolu = orch.resolve_stream(&requete_upnp(42, &uri)).await;
        if let Ok(resolu) = resolu {
            let vue = resolu
                .origin_url
                .clone()
                .unwrap_or_else(|| resolu.url.clone());
            assert_eq!(vue, uri, "le flux distant part tel quel");
        }
    }

    /// Une ligne indexée depuis un serveur UPnP n'a pas de fichier ici :
    /// jamais résolue en local, même derrière notre adresse.
    #[tokio::test]
    async fn une_ligne_indexee_upnp_n_est_pas_locale_9207() {
        let orch = orchestrateur();
        let id = piste_aiff(&orch, "upnp");
        let uri = crate::upnp_server::track_audio_url("http://127.0.0.1:8888", id);
        assert_eq!(orch.piste_locale_de_notre_uri(&uri), None);
        let absente = crate::upnp_server::track_audio_url("http://127.0.0.1:8888", id + 1000);
        assert_eq!(
            orch.piste_locale_de_notre_uri(&absente),
            None,
            "ligne absente"
        );
    }

    /// La suivante (`SetNextAVTransportURI`) entre dans la file de la zone en
    /// `upnp` + URI : son pré-armement sans blanc suit la même règle.
    #[tokio::test]
    async fn la_suivante_de_notre_bibliotheque_est_pre_armee_en_local_9207() {
        let orch = orchestrateur();
        let id = piste_aiff(&orch, "local");
        let uri = crate::upnp_server::track_audio_url("http://127.0.0.1:8888", id);
        let zone_id = ZoneRepo::with_backend(orch.db.clone())
            .create("Pink", Some("local"), None)
            .unwrap();
        let file: Vec<StreamingQueueItem> = vec![(
            uri.clone(),
            String::new(),
            String::new(),
            None,
            None,
            0i64,
            Some("upnp".to_string()),
            Some(1),
            None,
        )];
        PlayQueueRepo::with_backend(orch.db.clone())
            .set_streaming_queue(zone_id, &file)
            .unwrap();
        let item = orch
            .resolve_queue_item_url(zone_id, 0)
            .await
            .expect("la suivante doit se pré-armer");
        assert_ne!(item.url, uri, "la suivante ne part plus en flux HTTP brut");
        assert!(item.stream_id.is_some());
        assert_eq!(item.source.as_deref(), Some("upnp"));
        assert_eq!(item.source_id.as_deref(), Some(uri.as_str()));
    }

    /// Le complément : symphonia reconnaît l'AIFF (fonction `aiff`), pour les
    /// URI AIFF d'un serveur tiers que la sortie locale sonde.
    #[test]
    fn symphonia_reconnait_l_aiff_9207() {
        use symphonia::core::formats::probe::Hint;
        use symphonia::core::formats::{FormatOptions, TrackType};
        use symphonia::core::io::MediaSourceStream;
        use symphonia::core::meta::MetadataOptions;

        let fichier = std::fs::File::open(AIFF).unwrap();
        let mss = MediaSourceStream::new(Box::new(fichier), Default::default());
        // Sans indice d'extension : un flux HTTP n'en porte pas toujours.
        let mut format = symphonia::default::get_probe()
            .probe(
                &Hint::new(),
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .expect("AIFF reconnu par symphonia");
        assert!(format.default_track(TrackType::Audio).is_some());
        assert!(format.next_packet().is_ok(), "un paquet se lit");
    }
}
