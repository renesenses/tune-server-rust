//! #3479 — activer l'égaliseur laisse enfin une trace qui NOMME la zone, le
//! type de sortie, le format et le premier échec.
//!
//! # Ce que le silence a coûté
//!
//! Le 06/09/2026, Reivax66 signale depuis Windows, en 0.9.138 : « l'activation
//! de l'égaliseur coupe le son mais n'interrompt pas la lecture qui continue en
//! tâche de fond. Lorsqu'on quitte l'égaliseur le son revient. » Un rapport de
//! diagnostic de 36 Ko est joint au ticket.
//!
//! Deux familles connues produisent MOT POUR MOT cette phrase :
//!
//! - la zone **RÉSEAU** — le fichier entier décodé, traité, encodé et écrit
//!   avant le premier octet (46 à 62 s), ou pire, servi sous une étiquette qui
//!   ne correspond pas à la charge utile, auquel cas le renderer reste muet
//!   (#3357) ;
//! - la sortie **LOCALE** — le marqueur DoP réécrit par le traitement, et le
//!   DAC qui quitte le mode DSD (#1735, corrigé par #1740).
//!
//! Ces deux familles vivent dans deux fichiers différents. Rien, dans aucun
//! journal, ne disait laquelle regarder : `apply_eq_change` rendait un `bool`,
//! et les six sorties anticipées de `refresh_zone_eq` rendaient toutes le même
//! `false`, sans une ligne. Le seul évènement existant, `zone_eq_refreshed_live`,
//! n'est émis QUE sur le chemin qui a réussi — c'est-à-dire jamais dans le cas
//! qu'on cherche à instruire.
//!
//! # Les deux bords, ensemble
//!
//! 1. **Tout changement d'égaliseur PARLE**, au niveau INFO — donc dans
//!    l'export de diagnostic qu'un testeur joint à son fil — en nommant la
//!    zone, la famille de sortie, le format avant et après, et le premier
//!    échec quand il y en a un.
//! 2. **Une ligne, pas deux.** Ce point d'entrée est appelé par cinq routes et
//!    par chaque cran d'un curseur de 31 bandes ; l'export borne chaque module
//!    à un quart de sa fenêtre (`QUOTA_PAR_MODULE`, #1974), donc un émetteur
//!    bavard arrache ses lignes à tous les autres.
//!
//! # Pourquoi un binaire de test à lui seul
//!
//! Même leçon que #2665, #2890 et #3180, déjà payée trois fois : `tracing` met
//! en cache POUR TOUT LE PROCESSUS la décision « ce point d'appel intéresse-t-il
//! quelqu'un ? ». Un abonné posé au milieu d'une suite qui tourne en parallèle
//! se voit priver d'évènements de façon imprévisible. Ici l'abonné est GLOBAL et
//! ce fichier ne contient QU'UN test. `autotests = false` dans
//! `tune-core/Cargo.toml` — la cible y est déclarée, sans quoi ce fichier ne
//! serait jamais compilé.

use std::sync::{Arc, Mutex};
use tokio::sync::Mutex as MutexAsync;

use tune_core::db::migrations::run_migrations;
use tune_core::db::sqlite::SqliteDb;
use tune_core::db::zone_repo::ZoneRepo;
use tune_core::http::streamer::AudioStreamer;
use tune_core::orchestrator::PlaybackOrchestrator;
use tune_core::outputs::registry::OutputRegistry;
use tune_core::playback::PlaybackManager;
use tune_core::streaming::registry::ServiceRegistry;

/// Recueille la sortie `tracing` : c'est le journal, et lui seul, qu'on aura
/// entre les mains la prochaine fois.
#[derive(Clone, Default)]
struct JournalCapture(Arc<Mutex<Vec<u8>>>);

impl JournalCapture {
    fn vider(&self) -> String {
        let mut tampon = self.0.lock().unwrap();
        let texte = String::from_utf8_lossy(&tampon).into_owned();
        tampon.clear();
        texte
    }
}

impl std::io::Write for JournalCapture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for JournalCapture {
    type Writer = JournalCapture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

fn lignes_eq(journal: &str) -> Vec<&str> {
    journal
        .lines()
        .filter(|l| l.contains("eq_change_journal"))
        .collect()
}

#[tokio::test]
async fn un_changement_d_eq_dit_la_zone_la_sortie_le_format_et_le_premier_echec() {
    let capture = JournalCapture::default();
    let abonne = tracing_subscriber::fmt()
        .with_writer(capture.clone())
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .finish();
    tracing::subscriber::set_global_default(abonne)
        .expect("ce binaire ne contient qu'un test : l'abonné global est libre");

    let db = SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    run_migrations(&db).unwrap();
    let db: Arc<dyn tune_core::db::backend::DbBackend> = Arc::new(db);

    #[cfg_attr(not(feature = "local-audio"), allow(unused_mut))]
    let mut registre = OutputRegistry::new();
    #[cfg(feature = "local-audio")]
    registre.register(Box::new(tune_core::outputs::local::LocalOutput::new(
        "DAC".to_string(),
    )));

    let orch = Arc::new(PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(PlaybackManager::new()),
        Arc::new(AudioStreamer::new(0)),
        Arc::new(MutexAsync::new(ServiceRegistry::new())),
        Arc::new(MutexAsync::new(registre)),
        None,
    ));

    let zones = ZoneRepo::with_backend(db.clone());
    let reseau = zones
        .create("Denon", Some("dlna"), Some("dlna:uuid-1234"))
        .unwrap();
    let locale = zones
        .create("Salon", Some("local"), Some("local:DAC"))
        .unwrap();

    // Un profil audible, tel qu'une activation depuis l'écran en pose un.
    let profil = tune_core::audio::eq::EqProfile {
        enabled: true,
        bands: vec![tune_core::audio::eq::EqBandSpec {
            freq: 80.0,
            gain: 8.0,
            q: 0.71,
            band_type: "low_shelf".into(),
            ..Default::default()
        }],
        ..Default::default()
    };
    let reglages = tune_core::db::settings_repo::SettingsRepo::with_backend(db.clone());
    for id in [reseau, locale] {
        reglages
            .set(
                &format!("zone_{id}_eq_profile"),
                &serde_json::to_string(&profil).unwrap(),
            )
            .unwrap();
    }
    // Rien de ce qui précède n'a le droit de parler au nom de l'égaliseur.
    let _ = capture.vider();

    // ── La zone RÉSEAU ───────────────────────────────────────────────────────
    assert!(
        !orch.apply_eq_change(reseau).await,
        "une zone réseau n'a pas de chemin local à servir à chaud"
    );
    let journal = capture.vider();
    let lignes = lignes_eq(&journal);
    assert_eq!(
        lignes.len(),
        1,
        "un changement d'égaliseur doit laisser UNE ligne, pas {} — ce point \
         d'entrée est appelé à chaque cran d'un curseur de 31 bandes :\n{journal}",
        lignes.len()
    );
    let ligne = lignes[0];
    // Le motif du premier échec DÉPEND du jeu de fonctionnalités, et c'est
    // légitime : sans `local-audio`, la caisse ne porte aucune sortie locale, et
    // c'est là le premier échec rencontré — avant même de regarder le type de la
    // zone. Le témoin exige donc le motif EXACT de chaque jeu, jamais un « l'un
    // ou l'autre » qui le rendrait vert par indulgence.
    let motif_reseau = if cfg!(feature = "local-audio") {
        "premier_echec=\"sortie_non_locale\""
    } else {
        "premier_echec=\"caisse_sans_audio_local\""
    };
    for attendu in [
        "zone=Denon",
        "famille=\"reseau\"",
        "output_type=\"dlna\"",
        motif_reseau,
        "profil_actif=true",
        "bandes=1",
        "chemin=\"rien_ne_joue\"",
        "format_avant=-",
        "format_apres=-",
    ] {
        assert!(
            ligne.contains(attendu),
            "la ligne ne porte pas `{attendu}` :\n{ligne}"
        );
    }
    // L'identifiant de l'appareil, lui, NE dépend d'aucune fonctionnalité : il
    // vient de la colonne `zones.output_device_id`. La CI du jeu
    // `--no-default-features --features oaat,cloud-relay,bandcamp` l'a trouvé
    // VIDE, parce que le bras « caisse sans audio local » ne lisait pas la zone.
    // Sans lui, la trace ne désigne plus l'appareil chez le testeur : c'est
    // exactement ce qu'on ne peut pas perdre, et sur le binaire allégé encore
    // moins qu'ailleurs. Cette assertion est hors de la boucle pour porter son
    // propre message.
    assert!(
        ligne.contains("device_id=dlna:uuid-1234"),
        "le journal a perdu l'identifiant de l'appareil — `device_id` est une \
         colonne de zone, il doit être le MÊME quel que soit le jeu de \
         fonctionnalités compilé :\n{ligne}"
    );

    // ── La sortie LOCALE, sortie présente mais rien qui joue ─────────────────
    assert!(!orch.apply_eq_change(locale).await);
    let journal = capture.vider();
    let lignes = lignes_eq(&journal);
    assert_eq!(lignes.len(), 1, "une ligne, ici aussi :\n{journal}");
    let ligne = lignes[0];
    assert!(
        ligne.contains("famille=\"locale\""),
        "la famille de sortie doit être un MOT dans le journal :\n{ligne}"
    );
    assert!(
        ligne.contains("zone=Salon") && ligne.contains("device_id=local:DAC"),
        "la ligne ne nomme pas la zone et son périphérique :\n{ligne}"
    );
    // C'est LE motif qui sépare les deux familles sur une sortie locale : la
    // sortie est là, elle est locale, et pourtant aucun format n'a encore été
    // vu. Sans `local-audio`, la caisse n'a pas de sortie locale du tout — et
    // le journal le dit aussi, plutôt que de se taire.
    let motif = if cfg!(feature = "local-audio") {
        "premier_echec=\"format_inconnu\""
    } else {
        "premier_echec=\"caisse_sans_audio_local\""
    };
    assert!(
        ligne.contains(motif),
        "la ligne ne nomme pas le premier échec ({motif}) :\n{ligne}"
    );
    assert!(
        ligne.contains("preamp_db_g=0") && ligne.contains("preamp_db_d=0"),
        "le pré-gain réservé doit être chiffré, même à zéro :\n{ligne}"
    );
}
