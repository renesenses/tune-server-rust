//! #6915 (Tades, fil 2372) — un CD chargé en mémoire, sur un renderer DLNA :
//! après un changement d'égaliseur, la lecture ne reprend pas au même endroit.
//!
//! La relance (`replay_zone_at_position`) rouvre la piste À la position : le
//! fournisseur PCM sert un flux qui COMMENCE à `depuis_ms`. Le renderer compte
//! sa position à partir du début de CE flux, et le sondeur la reprenait telle
//! quelle : cinq secondes après la relance, la zone se croyait à 0:05. La
//! relance suivante — la comparaison suivante — repartait donc près du début.
//!
//! Le témoin : l'URL du flux décalé porte son départ, et la position qu'un
//! renderer rapporte sur ce flux est rapportée à la piste.

use std::sync::Arc;

use tokio::sync::Mutex;
use tune_output_api::{OutputStatus, TransportState};

use crate::db::migrations::run_migrations;
use crate::db::sqlite::SqliteDb;
use crate::http::streamer::AudioStreamer;
use crate::outputs::dlna_depart_natif::{corriger_le_statut, depart_de_l_url, noter_url_posee};
use crate::outputs::registry::OutputRegistry;
use crate::playback::PlaybackManager;
use crate::source_pcm::{FluxPcm, FormatPcm, FournisseurPcm};
use crate::streaming::registry::ServiceRegistry;

use super::{PlayRequest, PlaybackOrchestrator};

const DUREE_MS: u64 = 300_000;

/// Une piste de cinq minutes qui s'ouvre à la position, comme `tune-cd`.
struct DisqueEnMemoire;

impl FournisseurPcm for DisqueEnMemoire {
    fn ouvrir(&self, _source_id: &str, depuis_ms: u64) -> Result<FluxPcm, String> {
        let restant_ms = DUREE_MS.saturating_sub(depuis_ms);
        let octets = restant_ms * FormatPcm::CD.octets_par_seconde() / 1000;
        Ok(FluxPcm {
            format: FormatPcm::CD,
            octets,
            duree_ms: DUREE_MS,
            lecteur: Box::new(std::io::Read::take(std::io::repeat(0), octets)),
        })
    }
}

fn orchestrateur() -> PlaybackOrchestrator {
    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let orch = PlaybackOrchestrator::new(
        Arc::new(db),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(Mutex::new(ServiceRegistry::new())),
        Arc::new(Mutex::new(OutputRegistry::new())),
        Some("127.0.0.1".into()),
    );
    orch.sources_pcm()
        .inscrire("cd-6915", Arc::new(DisqueEnMemoire));
    orch
}

fn piste(seek_ms: Option<u64>) -> PlayRequest {
    PlayRequest {
        zone_id: 6915,
        source: Some("cd-6915".into()),
        source_id: Some("disque/3".into()),
        title: Some("Piste 3".into()),
        seek_ms,
        ..Default::default()
    }
}

fn statut(uri: &str, position_ms: u64, duration_ms: u64) -> OutputStatus {
    OutputStatus {
        state: TransportState::Playing,
        position_ms,
        duration_ms,
        volume: 0.5,
        muted: false,
        current_uri: Some(uri.to_string()),
        track_title: None,
        track_artist: None,
        ended_naturally: false,
        realtime: true,
        dop_active: false,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cd_6915_la_relance_a_la_position_garde_la_position_de_la_piste() {
    let orch = orchestrateur();

    // Relance après un changement d'égaliseur, à 2:13.
    let r = orch.resolve_stream(&piste(Some(133_000))).await.unwrap();
    assert_eq!(
        depart_de_l_url(&r.url),
        Some(133_000),
        "le flux du CD rouvert à 2:13 commence à 2:13 : son URL doit le dire, sinon le \
         renderer le compte depuis 0:00 et la relance suivante repart près du début (#6915) — {}",
        r.url
    );
    assert_eq!(r.duration_ms, Some(DUREE_MS as i64));

    // Le renderer DLNA reçoit cette URL et rapporte 0:05 de CE flux.
    let appareil = "uuid:diretta-c19-6915";
    noter_url_posee(appareil, &r.url);
    let restant = DUREE_MS - 133_000;
    let mut s = statut(&r.url, 5_000, restant);
    corriger_le_statut(appareil, &mut s);
    assert_eq!(s.position_ms, 138_000, "0:05 du flux = 2:18 de la piste");
    assert_eq!(s.duration_ms, DUREE_MS, "la durée de la piste entière");

    // Une lecture depuis le début ne porte aucun départ.
    let r0 = orch.resolve_stream(&piste(None)).await.unwrap();
    assert_eq!(depart_de_l_url(&r0.url), None, "{}", r0.url);
}
