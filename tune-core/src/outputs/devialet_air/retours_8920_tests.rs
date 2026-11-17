//! #8920 — retours d'un testeur Devialet Expert (rc12) : volume tenu par
//! l'ampli, boutons −1 / +1 dB, ni volume fixe ni plein volume en PURE.
//!
//! La sécurité se prouve sur ce que l'AMPLI reçoit : la sortie réelle face au
//! simulateur, sur la boucle locale, derrière l'orchestrateur (le point de
//! passage de toute consigne). Aucun son ne sort.

use std::sync::Arc;
use std::time::Duration;

use super::super::protocole::opcode;
use super::super::reglages::{self, BornesDb};
use super::super::simulateur::{Simulateur, annonce_de_test};
use super::pieces_du_banc;
use crate::audio::volume_limites::{self, cle_confort, cle_securite};
use crate::db::backend::DbBackend;
use crate::db::settings_repo::SettingsRepo;
use crate::db::zone_repo::ZoneRepo;
use crate::orchestrator::{PlaybackOrchestrator, consigne_du_pas};
use crate::outputs::traits::OutputTarget;

/// −45 dB sur la course par défaut (−80 … −10 dB) : la sécurité des bancs.
const SECURITE: f64 = 0.5;
const SECURITE_DB: f64 = -45.0;
/// −59 dB : le confort des bancs.
const CONFORT: f64 = 0.3;
const CONFORT_DB: f64 = -59.0;

fn base() -> Arc<dyn DbBackend> {
    let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
    db.init_schema().unwrap();
    crate::db::migrations::run_migrations(&db).unwrap();
    Arc::new(db)
}

struct BancZone {
    sim: Simulateur,
    orch: PlaybackOrchestrator,
    zone: i64,
    did: String,
    _relais: tokio::task::JoinHandle<()>,
}

impl Drop for BancZone {
    fn drop(&mut self) {
        self._relais.abort();
    }
}

/// Une zone Devialet AIR, sa sortie RÉELLE face au simulateur, l'ampli
/// annonçant `ampli_db`, avec les limites données (fractions de la course).
async fn banc_zone(ampli_db: f64, securite: Option<f64>, confort: Option<f64>) -> BancZone {
    let mut annonce = annonce_de_test();
    annonce.volume_demi_db = (ampli_db * 2.0) as i16;
    let (sim, sortie, _drapeau, relais) = pieces_du_banc(annonce, true, BornesDb::PAR_DEFAUT).await;
    let db = base();
    let orch = PlaybackOrchestrator::new(
        db.clone(),
        Arc::new(crate::playback::PlaybackManager::new()),
        Arc::new(crate::http::streamer::AudioStreamer::new(0)),
        Arc::new(tokio::sync::Mutex::new(
            crate::streaming::registry::ServiceRegistry::new(),
        )),
        Arc::new(tokio::sync::Mutex::new(
            crate::outputs::registry::OutputRegistry::new(),
        )),
        None,
    );
    let did = sortie.device_id().to_string();
    let zone = ZoneRepo::with_backend(db.clone())
        .create("Expert", Some("devialet_air"), Some(&did))
        .unwrap();
    let v = BornesDb::PAR_DEFAUT.db_vers_volume(ampli_db);
    ZoneRepo::with_backend(db.clone())
        .update_volume(zone, v * 100.0)
        .unwrap();
    orch.playback.set_volume(zone, v).await;
    let r = SettingsRepo::with_backend(db.clone());
    if let Some(s) = securite {
        r.set(&cle_securite(zone), &s.to_string()).unwrap();
    }
    if let Some(c) = confort {
        r.set(&cle_confort(zone), &c.to_string()).unwrap();
    }
    orch.outputs.lock().await.register(Box::new(sortie));
    BancZone {
        sim,
        orch,
        zone,
        did,
        _relais: relais,
    }
}

impl BancZone {
    /// Les volumes (dB) que l'ampli a reçus.
    fn volumes_recus(&self) -> Vec<f32> {
        self.sim
            .etat()
            .commandes
            .iter()
            .filter(|c| c.opcode == opcode::VOLUME)
            .map(|c| c.valeur)
            .collect()
    }

    /// Le volume que l'ampli annonce, en dB.
    fn volume_annonce(&self) -> f64 {
        self.sim.etat().annonce.as_ref().unwrap().volume_db()
    }

    /// La télécommande infrarouge : l'ampli change, AUCUN message réseau.
    async fn telecommande(&self, db: f64) {
        self.sim.etat().annonce.as_mut().unwrap().volume_demi_db = (db * 2.0) as i16;
        tokio::time::sleep(Duration::from_millis(80)).await;
    }

    /// Un pas, puis le temps que l'ampli l'applique et le ré-annonce.
    async fn pas(&self, monter: bool) -> crate::orchestrator::PasAir {
        let avant = self.volumes_recus().len();
        let p = self
            .orch
            .pas_de_volume_air(self.zone, Some(&self.did), monter, 1.0)
            .await
            .unwrap();
        if p.envoye {
            for _ in 0..100 {
                if self.volumes_recus().len() > avant {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
        p
    }

    fn jamais_au_dessus(&self, plafond_db: f64) {
        for v in self.volumes_recus() {
            assert!(
                f64::from(v) <= plafond_db + 1e-6,
                "l'ampli a reçu {v} dB, au-dessus de {plafond_db} dB : {:?}",
                self.volumes_recus()
            );
        }
    }
}

/// Témoin : le pas part du niveau de l'ampli, monte d'un dB, franchit le
/// CONFORT (le curseur, lui, n'y passe pas) et s'arrête net à la SÉCURITÉ.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_8920_le_plus_franchit_le_confort_et_bute_sur_la_securite() {
    let b = banc_zone(CONFORT_DB - 1.0, Some(SECURITE), Some(CONFORT)).await;
    // −60 → −59 (confort) → −58 : au-dessus du confort, au bouton.
    for attendu in [CONFORT_DB, CONFORT_DB + 1.0] {
        let p = b.pas(true).await;
        assert!(p.envoye, "{p:?}");
        assert_eq!(b.volume_annonce(), attendu);
    }
    // Près de la sécurité : −47 → −46 → −45, puis plus rien.
    b.telecommande(SECURITE_DB - 2.0).await;
    for attendu in [SECURITE_DB - 1.0, SECURITE_DB] {
        assert!(b.pas(true).await.envoye);
        assert_eq!(b.volume_annonce(), attendu);
    }
    let envoyes = b.volumes_recus().len();
    for _ in 0..3 {
        let p = b.pas(true).await;
        assert!(!p.envoye, "+ à la sécurité : rien ne part ({p:?})");
    }
    assert_eq!(b.volumes_recus().len(), envoyes);
    assert_eq!(b.volume_annonce(), SECURITE_DB);
    b.jamais_au_dessus(SECURITE_DB);
}

/// L'ampli monté à la télécommande AU-DESSUS de la sécurité : son niveau
/// fait foi pour l'affichage, mais un + ne commande RIEN (ni plus haut, ni
/// le « plus » qui baisserait), et un − le ramène sous la sécurité.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_8920_ampli_au_dessus_de_la_securite_rien_ne_monte() {
    let b = banc_zone(-20.0, Some(SECURITE), Some(CONFORT)).await;
    let o = {
        let reg = b.orch.outputs.lock().await;
        let arc = reg.get(&b.did).unwrap();
        let s = arc.lock().await;
        s.as_any()
            .downcast_ref::<super::super::DevialetAirOutput>()
            .unwrap()
            .observation_volume()
            .unwrap()
    };
    assert_eq!(o.volume_db, -20.0, "le niveau rapporté, tel quel");
    assert_eq!(b.orch.volume_rapporte_air(Some(&b.did)).await, Some(-20.0));
    for _ in 0..3 {
        let p = b.pas(true).await;
        assert!(!p.envoye, "{p:?}");
        assert_eq!(p.depart_db, -20.0);
    }
    assert!(b.volumes_recus().is_empty(), "{:?}", b.volumes_recus());
    // Une consigne absolue au plus haut ne passe pas non plus la sécurité.
    b.orch.set_volume(b.zone, 1.0, Some(&b.did)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    b.jamais_au_dessus(SECURITE_DB);
    // Le − redescend (à la sécurité au plus).
    b.telecommande(-20.0).await;
    assert!(b.pas(false).await.envoye);
    assert!(b.volume_annonce() <= SECURITE_DB);
    b.jamais_au_dessus(SECURITE_DB);
}

/// Contre-épreuve : SANS sécurité, le même banc fait bien monter l'ampli
/// au-dessus de −45 dB — le témoin sait voir un envoi —, et le plafond de
/// −10 dB de la sortie reste le dernier mot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_8920_contre_epreuve_sans_securite_le_pas_monte_jusqu_au_plafond() {
    let b = banc_zone(-20.0, None, None).await;
    assert!(b.pas(true).await.envoye);
    assert_eq!(b.volume_annonce(), -19.0);
    b.telecommande(-10.5).await;
    assert!(b.pas(true).await.envoye);
    assert_eq!(b.volume_annonce(), reglages::PLAFOND_DB);
    let envoyes = b.volumes_recus().len();
    assert!(!b.pas(true).await.envoye, "au plafond : rien");
    assert_eq!(b.volumes_recus().len(), envoyes);
    b.jamais_au_dessus(reglages::PLAFOND_DB);
    assert!(
        b.volumes_recus()
            .iter()
            .any(|v| f64::from(*v) > SECURITE_DB),
        "contre-épreuve : sans sécurité, l'ampli passe au-dessus de −45 dB"
    );
}

/// « Plein volume en PURE » : sur Devialet AIR, ni le réglage global ni la
/// surcharge de zone ne s'appliquent ; les limites tiennent EN PURE.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_8920_pure_ni_plein_volume_ni_limites_suspendues() {
    let b = banc_zone(-60.0, Some(SECURITE), None).await;
    let r = SettingsRepo::with_backend(b.orch.db.clone());
    r.set(crate::audio::audiophile::SETTING_LOCK_VOLUME, "true")
        .unwrap();
    r.set(
        &format!("zone_{}_audiophile", b.zone),
        r#"{"enabled":true,"lock_volume":true}"#,
    )
    .unwrap();
    assert!(crate::audio::audiophile::zone_enabled(&b.orch.db, b.zone));
    assert!(!crate::audio::audiophile::volume_lock_enabled(
        &b.orch.db, b.zone
    ));
    assert_eq!(volume_limites::mode_sans_limite(&b.orch.db, b.zone), None);
    let envoi = b
        .orch
        .set_volume_pure_force(b.zone, Some(&b.did))
        .await
        .unwrap();
    assert_eq!(envoi, crate::orchestrator::EnvoiDeVolume::AucunVolumeFixe);
    b.orch.set_volume(b.zone, 1.0, Some(&b.did)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    b.orch
        .set_volume_au_pas(b.zone, 1.0, Some(&b.did))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    b.jamais_au_dessus(SECURITE_DB);

    // Contre-épreuve : la même zone en DLNA, le verrou s'applique et PURE
    // suspend les limites (comportement inchangé hors Devialet AIR).
    ZoneRepo::with_backend(b.orch.db.clone())
        .update_output_type(b.zone, "dlna")
        .unwrap();
    assert!(crate::audio::audiophile::volume_lock_enabled(
        &b.orch.db, b.zone
    ));
    assert_eq!(
        volume_limites::mode_sans_limite(&b.orch.db, b.zone),
        Some(volume_limites::ModeSansLimite::ZonePure)
    );
}

/// Le volume fixe n'existe pas sur Devialet AIR : un drapeau hérité en base
/// est lu faux, l'armement est refusé sans rien envoyer, et la consigne
/// reste commandée (et bornée) au lieu d'être court-circuitée.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn air_8920_volume_fixe_lu_faux_et_armement_refuse() {
    let b = banc_zone(-60.0, Some(SECURITE), None).await;
    let repo = ZoneRepo::with_backend(b.orch.db.clone());
    repo.update_fixed_volume(b.zone, true).unwrap();
    assert!(!repo.get(b.zone).unwrap().unwrap().fixed_volume);
    assert!(b.orch.arm_fixed_volume(b.zone, Some(&b.did)).await.is_err());
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(b.volumes_recus().is_empty(), "{:?}", b.volumes_recus());
    b.orch.set_volume(b.zone, 0.3, Some(&b.did)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert_eq!(b.volumes_recus().last().copied(), Some(-59.0));
    // Contre-épreuve : hors Devialet AIR, le drapeau est lu tel quel.
    repo.update_output_type(b.zone, "dlna").unwrap();
    assert!(repo.get(b.zone).unwrap().unwrap().fixed_volume);
}

/// La règle du pas, sans réseau.
#[test]
fn air_8920_consigne_du_pas() {
    let c = BornesDb::PAR_DEFAUT;
    // +1 dB depuis −60, sécurité −45 : −59.
    let (_, v) = consigne_du_pas(c, -60.0, true, 1.0, Some(SECURITE)).unwrap();
    assert_eq!(c.volume_vers_db(v), -59.0);
    // Depuis −45.5 : bornée à −45.
    let (_, v) = consigne_du_pas(c, -45.5, true, 1.0, Some(SECURITE)).unwrap();
    assert_eq!(c.volume_vers_db(v), -45.0);
    // À ou au-dessus de la sécurité : un + ne commande rien.
    for depart in [-45.0, -30.0, -12.0, -3.0, 5.0] {
        assert!(
            consigne_du_pas(c, depart, true, 1.0, Some(SECURITE)).is_none(),
            "{depart}"
        );
    }
    // Sans sécurité, au plafond : rien ; au-dessus du plafond : rien.
    assert!(consigne_du_pas(c, -10.0, true, 1.0, None).is_none());
    assert!(consigne_du_pas(c, -5.0, true, 1.0, None).is_none());
    // Le − descend, jamais ne monte ; au plancher, rien.
    let (_, v) = consigne_du_pas(c, -20.0, false, 1.0, Some(SECURITE)).unwrap();
    assert!(c.volume_vers_db(v) <= SECURITE_DB);
    assert!(consigne_du_pas(c, -80.0, false, 1.0, None).is_none());
    // Entrées absurdes : rien.
    assert!(consigne_du_pas(c, f64::NAN, true, 1.0, None).is_none());
    assert!(consigne_du_pas(c, -40.0, true, 0.0, None).is_none());
    assert!(consigne_du_pas(c, -40.0, true, f64::INFINITY, None).is_none());
    // Aucune consigne, sur toute la plage, au-dessus de la sécurité.
    let mut d = -90.0;
    while d <= 10.0 {
        for monter in [true, false] {
            if let Some((_, v)) = consigne_du_pas(c, d, monter, 1.0, Some(SECURITE)) {
                assert!(c.volume_vers_db(v) <= SECURITE_DB, "{d} {monter}");
            }
        }
        d += 0.5;
    }
}

/// 192 kHz au plus, et seulement si la zone le règle : sans réglage, 96 kHz
/// sans perte comme avant.
#[test]
fn air_8920_cadence_sous_le_plafond_de_la_zone() {
    use reglages::cadence_envoyee_sous as c;
    // Sans plafond : inchangé.
    assert_eq!(c(192_000, None), 96_000);
    assert_eq!(c(176_400, None), 96_000);
    assert_eq!(c(44_100, None), 44_100);
    // Plafond 192 kHz : la source passe telle quelle jusqu'à 192 kHz.
    assert_eq!(c(192_000, Some(192_000)), 192_000);
    assert_eq!(c(176_400, Some(192_000)), 176_400);
    assert_eq!(c(352_800, Some(192_000)), 176_400);
    assert_eq!(c(384_000, Some(192_000)), 192_000);
    // Un plafond au-delà de 192 kHz est ramené à 192 kHz.
    assert_eq!(c(384_000, Some(768_000)), 192_000);
    // Plafond 96 kHz : la famille est gardée.
    assert_eq!(c(176_400, Some(96_000)), 88_200);
    assert_eq!(c(192_000, Some(96_000)), 96_000);
    // Plafond 48 kHz.
    assert_eq!(c(96_000, Some(48_000)), 48_000);
    assert_eq!(c(88_200, Some(48_000)), 44_100);
    // Jamais au-dessus de 192 kHz, jamais hors des cadences AIR.
    for source in [
        8_000, 22_050, 32_000, 44_100, 48_000, 88_200, 96_000, 176_400, 192_000, 352_800, 384_000,
        705_600,
    ] {
        for plafond in [
            None,
            Some(44_100),
            Some(48_000),
            Some(96_000),
            Some(192_000),
            Some(384_000),
        ] {
            let r = c(source, plafond);
            assert!(r <= reglages::FREQUENCE_MAX_AIR, "{source} {plafond:?}");
            assert!(
                super::super::protocole::FREQUENCES.contains(&r),
                "{source} {plafond:?} → {r}"
            );
        }
    }
}
