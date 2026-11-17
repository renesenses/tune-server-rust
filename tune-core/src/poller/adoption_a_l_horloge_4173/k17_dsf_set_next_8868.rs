//! #8868 (fil 2323, rc12, Windows 11) — FiiO K17 en DLNA : une piste DSF
//! (DSD256) est coupée ~30 s avant sa fin, à CHAQUE redémarrage de Tune.
//!
//! ## Ce que montrent les rapports du testeur
//!
//! ```text
//! 23:08:05 gapless_dsd_next_dlna_arme … dlna_set_next device=FIIO K17 url=…/stream/….dsf
//! 23:08:13 dlna_set_next_coupe_la_piste_detecte … prev_pos=153000 track_dur=182130
//! 23:08:13 dlna_set_next_coupe_la_piste_appris device_id=uuid:FiiORender-1_0-0-204
//! 00:02:38.946 known_renderer_reregistered id=uuid:FiiORender-1_0-0-204   (redémarrage)
//! 00:02:39.004 demarrage_etape etape="découverte réseau"                 (base branchée ici)
//! 00:04:52 gapless_dsd_next_dlna_arme … dlna_set_next device=FIIO K17     (la règle est ignorée)
//! 00:04:54 dlna_set_next_coupe_la_piste_appris                           (réapprise)
//! ```
//!
//! La règle #6477 EST consultée sur le chemin DSD (`armer_le_flux_suivant`,
//! avant la garde DSD) et elle EST apprise en DSD. Elle était perdue au
//! redémarrage : la resonde des renderers connus rattachait le K17 avant que
//! la mémoire de compatibilité ne soit branchée sur la base, et sa première
//! lecture, faite sans base, marquait l'appareil « chargé » pour tout le
//! processus. La règle rangée en base n'était plus jamais relue.
//!
//! ## Le faux K17
//!
//! Un renderer DLNA simulé qui annonce le DSD et `SetNextAVTransportURI`, et
//! qui, dès qu'il TIENT une suivante, lâche la piste en cours : flux tiré en
//! partie, position qui repart à 0 sur la suivante. Le renderer conforme du
//! même banc va, lui, au bout de la piste.
//!
//! ## Ce que le banc ne prouve pas
//!
//! Que le vrai K17 joue la suivante sans blanc par `SetAVTransportURI` +
//! `Play` : le faux renderer acquitte, il ne joue rien. Aucun son réel.

use super::*;
use crate::outputs::dlna::DsdCapability;
use crate::outputs::dlna_repli_set_uri as compat;
use crate::outputs::traits::{AnnonceSuivante, TransportState};

const DSF: &str = "ref_dsd64_stereo.dsf";
/// Le fichier de banc de la piste en cours fait 35 324 octets : le K17 en a
/// tiré une partie seulement (444 Mo sur 514 Mo dans le rapport de 23:08).
const OCTETS_EN_PARTIE: u64 = 30_000;

fn annonce_dsd() -> DsdCapability {
    DsdCapability {
        supports_dsf: true,
        supports_dff: false,
        dsf_mime: Some("audio/x-dsd".to_string()),
    }
}

impl Banc {
    /// `ARMEE` et `SUIVANTE` deviennent des DSF ; la zone passe au mode DSD
    /// `mode` (« auto » : DSD natif, « dop » : DSD dans du PCM) ; le renderer
    /// annonce le DSD (GetProtocolInfo) et `SetNextAVTransportURI` (SCPD),
    /// comme le K17 du rapport (`supports_dsf=true`, `annonce=Annoncee`).
    async fn monter_le_k17(appareil: &str, mode: &str) -> Self {
        let mut banc = Self::monter_sur(appareil).await;
        for titre in [ARMEE, SUIVANTE] {
            let f = tempfile::Builder::new().suffix(".dsf").tempfile().unwrap();
            std::fs::copy(
                format!("{}/tests/fixtures/dsd/{DSF}", env!("CARGO_MANIFEST_DIR")),
                f.path(),
            )
            .unwrap();
            let chemin = f.path().to_string_lossy().into_owned();
            banc.orchestrator
                .db
                .execute(
                    "UPDATE tracks SET file_path = ?, format = 'dsf', sample_rate = 2822400, \
                     bit_depth = 1 WHERE title = ?",
                    &[
                        &chemin as &dyn crate::db::backend::ToSqlValue,
                        &titre as &dyn crate::db::backend::ToSqlValue,
                    ],
                )
                .unwrap();
            banc._fichiers.push(f);
        }
        ZoneRepo::with_backend(banc.orchestrator.db.clone())
            .update_dsd_mode(banc.zone_id, mode)
            .unwrap();
        banc.orchestrator
            .retenir_annonce_dsd_pour_banc(appareil, annonce_dsd())
            .await;
        {
            let reg = banc.outputs.lock().await;
            let arc = reg.get(appareil).unwrap();
            let sortie = arc.lock().await;
            sortie
                .as_any()
                .downcast_ref::<MockOutput>()
                .unwrap()
                .annoncer_la_suivante(AnnonceSuivante::Annoncee);
        }
        banc
    }

    /// La piste en cours entre dans sa fenêtre d'armement (~30 s avant la
    /// fin). Le faux K17, s'il vient de recevoir un `SetNext`, lâche la piste
    /// en cours aussitôt : flux tiré en partie, position qui repart à 0.
    async fn fenetre_d_armement_chez_le_k17(&mut self) {
        let avant = self.armees().await.len();
        self.renderer_a(207_000, 210).await;
        self.tic().await;
        if self.armees().await.len() > avant {
            let finie = self.flux_finie.clone();
            self.le_renderer_tire(&finie, OCTETS_EN_PARTIE).await;
            let reg = self.outputs.lock().await;
            let arc = reg.get(&self.appareil).unwrap();
            let sortie = arc.lock().await;
            let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
            mock.set_state(TransportState::Playing).await;
            mock.set_position(1_000);
        }
        self.tic().await;
    }

    /// La piste armée approche de sa fin : quelques sondages dans la fenêtre.
    async fn la_piste_d_apres_approche_de_sa_fin(&mut self) {
        for horloge in 205..212 {
            self.renderer_a(205_000 + (horloge - 205) * 1_000, horloge)
                .await;
            self.tic().await;
        }
    }
}

/// Simule un redémarrage de Tune pour `appareil`, dans l'ordre du rapport de
/// 00:02 : la mémoire process est vidée, puis la resonde des renderers connus
/// rattache l'appareil AVANT que la base ne soit branchée.
fn redemarrage_avec_resonde_avant_la_base(appareil: &str, firmware: Option<&str>) {
    compat::oublier_en_memoire_seulement(appareil);
    compat::charger_pour_appareil_avant_la_persistance(appareil, firmware);
}

/// La règle apprise est rangée en base par un fil bloquant : on l'attend.
async fn regle_rangee_en_base(appareil: &str) {
    for _ in 0..200 {
        if compat::valeur_en_base(appareil).is_some_and(|v| v.contains("set_next_coupe_la_piste")) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("la règle #6477 n'a pas été rangée en base pour {appareil}");
}

/// ⭐ Le cas du fil 2323 en rc12 : la règle est apprise sur un DSF, Tune
/// redémarre, la resonde rattache le K17 avant la base, et la piste DSF
/// suivante ne doit PAS recevoir de `SetNext`.
///
/// Contre-épreuve : sans le correctif, le DSF part en `SetNext` et le faux
/// K17 coupe la piste (ce test échoue sur `armees`).
#[tokio::test]
async fn k17_regle_apprise_en_dsf_survit_au_redemarrage() {
    let _ = compat::base_de_test();
    let k17 = "uuid:FiiORender-banc-8868-redemarrage";
    let _ = compat::reinitialiser_compatibilite(k17);

    // Première vie de Tune : le DSF part en SetNext, le K17 coupe, Tune apprend.
    let mut banc = Banc::monter_le_k17(k17, "auto").await;
    banc.fenetre_d_armement_chez_le_k17().await;
    assert_eq!(banc.armees().await, vec![ARMEE.to_string()]);
    assert!(
        compat::set_next_coupe_la_piste(k17),
        "#6477 : la coupure au SetNext d'un DSF doit apprendre la règle"
    );
    regle_rangee_en_base(k17).await;
    drop(banc);

    // Redémarrage : resonde avant la base.
    redemarrage_avec_resonde_avant_la_base(k17, None);

    let mut banc = Banc::monter_le_k17(k17, "auto").await;
    banc.fenetre_d_armement_chez_le_k17().await;
    assert_eq!(
        banc.armees().await,
        Vec::<String>::new(),
        "#8868 : la règle apprise avant le redémarrage doit être relue — aucun SetNext \
         DSD vers le K17, sinon il coupe la piste 30 s avant la fin"
    );
    let (position, titre, _) = banc.ecran().await;
    assert_eq!(
        (position, titre.as_str()),
        (0, FINIE),
        "le K17 n'a rien reçu d'avance : la piste en cours n'est pas coupée"
    );

    compat::reinitialiser_compatibilite(k17).unwrap();
}

/// Même cas, zone en DoP (le DSF part en PCM, `audio/wav`) : la règle est
/// consultée avant toute distinction de format.
#[tokio::test]
async fn k17_en_dop_la_regle_survit_au_redemarrage() {
    let _ = compat::base_de_test();
    let k17 = "uuid:FiiORender-banc-8868-dop";
    let _ = compat::reinitialiser_compatibilite(k17);

    let mut banc = Banc::monter_le_k17(k17, "dop").await;
    banc.fenetre_d_armement_chez_le_k17().await;
    assert_eq!(banc.armees().await, vec![ARMEE.to_string()]);
    assert!(compat::set_next_coupe_la_piste(k17));
    regle_rangee_en_base(k17).await;
    drop(banc);

    redemarrage_avec_resonde_avant_la_base(k17, None);

    let mut banc = Banc::monter_le_k17(k17, "dop").await;
    banc.fenetre_d_armement_chez_le_k17().await;
    assert_eq!(banc.armees().await, Vec::<String>::new());

    compat::reinitialiser_compatibilite(k17).unwrap();
}

/// Dans une même vie de Tune, après la coupure d'un DSF, le DSF d'après ne
/// reçoit plus de `SetNext` : la fin de piste le jouera par
/// `SetAVTransportURI` + `Play`.
#[tokio::test]
async fn k17_apres_la_coupure_d_un_dsf_le_dsf_d_apres_n_est_plus_arme() {
    let _ = compat::base_de_test();
    let k17 = "uuid:FiiORender-banc-8868-meme-vie";
    let _ = compat::reinitialiser_compatibilite(k17);

    let mut banc = Banc::monter_le_k17(k17, "auto").await;
    banc.fenetre_d_armement_chez_le_k17().await;
    assert!(compat::set_next_coupe_la_piste(k17));
    let (position, titre, _) = banc.ecran().await;
    assert_eq!((position, titre.as_str()), (1, ARMEE));

    banc.la_piste_d_apres_approche_de_sa_fin().await;
    assert_eq!(
        banc.armees().await,
        vec![ARMEE.to_string()],
        "plus aucun SetNext DSD vers le K17 une fois la règle apprise"
    );

    compat::reinitialiser_compatibilite(k17).unwrap();
}

/// Gapless préservé : un renderer CONFORME (il va au bout de la piste, puis
/// enchaîne) reçoit toujours le `SetNext` du DSF suivant, puis celui d'après.
#[tokio::test]
async fn renderer_conforme_recoit_toujours_le_set_next_dsf() {
    let _ = compat::base_de_test();
    let conforme = "uuid:banc-8868-renderer-conforme";
    let _ = compat::reinitialiser_compatibilite(conforme);

    let mut banc = Banc::monter_le_k17(conforme, "auto").await;
    banc.renderer_a(207_000, 210).await;
    banc.tic().await;
    assert_eq!(banc.armees().await, vec![ARMEE.to_string()]);
    let finie = banc.flux_finie.clone();
    banc.le_renderer_tire(&finie, OCTETS_EN_PARTIE).await;
    banc.renderer_a(236_000, 236).await;
    banc.tic().await;
    {
        let reg = banc.outputs.lock().await;
        let arc = reg.get(conforme).unwrap();
        let sortie = arc.lock().await;
        let mock = sortie.as_any().downcast_ref::<MockOutput>().unwrap();
        mock.set_state(TransportState::Playing).await;
        mock.set_position(1_000);
    }
    banc.tic().await;
    assert!(
        !compat::set_next_coupe_la_piste(conforme),
        "une fin ordinaire n'apprend rien"
    );

    // Et un redémarrage avec resonde avant la base n'invente rien non plus.
    redemarrage_avec_resonde_avant_la_base(conforme, None);
    banc.la_piste_d_apres_approche_de_sa_fin().await;
    assert_eq!(
        banc.armees().await,
        vec![ARMEE.to_string(), SUIVANTE.to_string()],
        "le gapless DSD d'un renderer conforme ne doit pas régresser"
    );

    compat::reinitialiser_compatibilite(conforme).unwrap();
}

/// La relecture tardive garde la règle de #6477 sur les versions logicielles :
/// une découverte d'avant la base qui voit une AUTRE version oublie ce qui fut
/// appris sous l'ancienne ; la même version la garde.
#[tokio::test]
async fn relecture_tardive_oublie_la_regle_si_la_version_a_change() {
    let _ = compat::base_de_test();
    let k17 = "uuid:FiiORender-banc-8868-version";
    let _ = compat::reinitialiser_compatibilite(k17);
    compat::charger_pour_appareil(k17, Some("1.0"));
    assert!(compat::memoriser_set_next_coupe_la_piste(k17));
    regle_rangee_en_base(k17).await;
    for _ in 0..200 {
        if compat::valeur_en_base(k17).is_some_and(|v| v.contains("1.0")) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    redemarrage_avec_resonde_avant_la_base(k17, Some("1.0"));
    assert!(
        compat::set_next_coupe_la_piste(k17),
        "même version : la règle est relue"
    );

    redemarrage_avec_resonde_avant_la_base(k17, Some("2.0"));
    assert!(
        !compat::set_next_coupe_la_piste(k17),
        "autre version : la règle apprise sous l'ancienne est oubliée"
    );

    let _ = compat::reinitialiser_compatibilite(k17);
}
