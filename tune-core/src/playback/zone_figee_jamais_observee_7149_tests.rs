//! #7149 — la zone d'écoute à distance rattrapée sans avoir jamais été
//! observée ne se dit plus « immobile depuis 0 s ».
//!
//! Séquence du journal du fil 2469 (rc8, Windows, 26/10, heure locale), zone
//! navigateur 48 jouée par le téléphone, sans appareil de sortie :
//!
//! ```text
//! 16:01:33.622 orchestrator_play zone_id=48 title=Introduction output_sent=false
//! 16:01:38.680 api_next_requested zone_id=48
//! 16:01:38.725 orchestrator_play zone_id=48 title=Love Can Move Mountains output_sent=false
//! 16:01:44.334 api_next_requested zone_id=48
//! 16:01:44.353 orchestrator_play zone_id=48 title=Show Some Emotion output_sent=false
//! 16:01:48.219 service_fichier_termine … complet=true fin="consommateur_parti"
//!              (rien d'autre pour la zone 48 pendant quinze minutes)
//! 16:16:33.653 zone_figee_rattrapee zone_id=48 immobile_secs=0 seuil_secs=600
//! ```
//!
//! 16:16:33.653 − 16:01:44.353 = 889 s. La durée de « Show Some Emotion »
//! n'est pas journalisée ; le détecteur tourne à la minute, et il ne mord
//! qu'une fois `durée + 600 s` écoulées, donc le journal la borne entre 230
//! et 289 s. On prend 280 s.

use super::*;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ZONE_DU_TELEPHONE: i64 = 48;

#[derive(Clone, Default)]
struct Journal(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Journal {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Journal {
    type Writer = Journal;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn piste(titre: &str, duree_ms: i64, stream_id: &str) -> NowPlaying {
    NowPlaying {
        title: titre.into(),
        source: "local".into(),
        duration_ms: duree_ms,
        stream_id: Some(stream_id.into()),
        ..Default::default()
    }
}

/// Les trois départs du journal, puis 889 s sans rien : ni position, ni
/// octets, ni `play()` de la piste suivante.
async fn sequence_du_fil_2469(pm: &PlaybackManager) {
    pm.play(ZONE_DU_TELEPHONE, piste("Introduction", 75_000, "flux-1"))
        .await;
    pm.play(
        ZONE_DU_TELEPHONE,
        piste("Love Can Move Mountains", 301_000, "flux-2"),
    )
    .await;
    pm.play(
        ZONE_DU_TELEPHONE,
        piste("Show Some Emotion", 280_000, "flux-3"),
    )
    .await;
    let mut zones = pm.zones.lock().await;
    let etat = zones.get_mut(&ZONE_DU_TELEPHONE).unwrap();
    etat.observation_rouverte_a = Instant::now().checked_sub(Duration::from_secs(889));
}

/// 🔴 Le témoin : la ligne `zone_figee_rattrapee` de la zone du téléphone
/// dit qu'elle n'a jamais été observée et chiffre ce qui a mené au verdict,
/// au lieu d'un `immobile_secs=0`.
#[tokio::test]
async fn la_zone_du_telephone_jamais_observee_ne_se_dit_pas_immobile_7149() {
    crate::journal_de_test::fiabiliser_la_capture();
    let pm = PlaybackManager::new();
    sequence_du_fil_2469(&pm).await;

    let journal = Journal::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(journal.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let rattrapees = {
        let _garde = tracing::subscriber::set_default(abonne);
        pm.arreter_les_zones_figees(SILENCE_AVANT_ZONE_FIGEE).await
    };
    assert_eq!(
        rattrapees,
        vec![ZONE_DU_TELEPHONE],
        "la piste est finie depuis dix minutes : la zone doit être rattrapée"
    );

    let texte = String::from_utf8(journal.0.lock().unwrap().clone()).unwrap();
    let ligne = texte
        .lines()
        .find(|l| l.contains("zone_figee_rattrapee"))
        .unwrap_or_else(|| panic!("aucune ligne zone_figee_rattrapee : {texte}"));
    assert!(
        !ligne.contains("immobile_secs="),
        "#7149 : une zone jamais observée est encore dite immobile, \
         ce qui se lit comme une position qui vient de bouger : {ligne}"
    );
    assert!(
        ligne.contains("motif=\"piste_finie_sans_observation\""),
        "le motif du verdict manque : {ligne}"
    );
    assert!(
        ligne.contains("fenetre_ouverte_secs=889"),
        "le temps écoulé depuis le départ de la piste manque : {ligne}"
    );
    assert!(
        ligne.contains("duree_piste_secs=280"),
        "la durée de la piste manque : {ligne}"
    );
    assert!(ligne.contains("zone_id=48"), "{ligne}");
}

/// Le versant observé ne change pas : une zone dont une avance a été vue,
/// puis plus rien, garde son âge d'immobilité.
#[tokio::test]
async fn une_zone_observee_garde_son_immobilite_7149() {
    let pm = PlaybackManager::new();
    pm.play(12, piste("Serenade", 240_000, "flux-12")).await;
    pm.update_position(12, 1_000).await;
    {
        let mut zones = pm.zones.lock().await;
        let etat = zones.get_mut(&12).unwrap();
        etat.derniere_avance_de_position = Instant::now().checked_sub(Duration::from_secs(643));
        etat.observation_rouverte_a = Instant::now().checked_sub(Duration::from_secs(700));
    }
    let constat = constat_de_zone_figee(&pm.get_state(12).await);
    assert_eq!(constat.motif, "immobile");
    assert_eq!(constat.immobile_secs, Some(643));
    assert_eq!(constat.fenetre_ouverte_secs, Some(700));
    assert_eq!(constat.duree_piste_secs, Some(240));
}
