//! 🔴 #6721 / #7119 — bout en bout, sur le VRAI sondeur (`tick`), le VRAI
//! orchestrateur et une sortie factice `dlna` : l'appareil joue plus bas que
//! la mémoire de la zone, le sondeur le relève sans l'adopter (#358), et un
//! geste pour BAISSER au curseur ne doit pas le faire sauter.
use super::volume_pure_5695_tests::Banc;
use crate::audio::garde_de_hausse::SEUIL_DE_HAUSSE_DB;
use crate::audio::volume_scale::PLAGE_PERCEPTIVE_DB;

const POINTS_DU_SEUIL: f64 = SEUIL_DE_HAUSSE_DB / PLAGE_PERCEPTIVE_DB;

/// Tune redémarre : mémoire restaurée à 80 % depuis la base, l'ampli a été
/// baissé à 10 % entre-temps. Premier relevé du sondeur : 10 %, non adopté
/// (aucun front). L'auditeur voit 80 % et saisit le curseur pour baisser à 60.
#[tokio::test]
async fn premier_releve_bas_puis_curseur_baisse_l_appareil_ne_saute_pas_6721() {
    let mut banc = Banc::monter(false).await;
    banc.zone_a(80.0).await;
    let deja = banc.appareil_a(0.10).await;
    banc.tic().await;
    assert!(
        (banc.volume_en_memoire().await - 0.80).abs() < 1e-9,
        "#358 : le premier relevé n'est jamais adopté"
    );
    banc.consigne_gardee(0.60).await;
    let recus = banc.commandes_depuis(deja).await;
    let plafond = 0.10 + POINTS_DU_SEUIL;
    assert!(
        !recus.is_empty() && recus.iter().all(|v| *v <= plafond + 1e-9),
        "#6721 : l'appareil relevé à 10 % (zone à 80 % en mémoire) a reçu {recus:?} pour un \
         geste qui BAISSE le curseur à 60 % — saut de plus de {SEUIL_DE_HAUSSE_DB} dB perçus"
    );
}

/// Trim −6 dB : le relevé est ramené à l'échelle de la consigne avant de
/// servir de repère (0,10 relu = consigne ≈ 0,20).
#[tokio::test]
async fn le_repere_retire_le_trim_negatif_6721() {
    let mut banc = Banc::monter(false).await;
    banc.poser_reglage(&format!("zone_{}_gain_trim_db", banc.zone_id), "-6");
    banc.zone_a(80.0).await;
    let deja = banc.appareil_a(0.10).await;
    banc.tic().await;
    banc.consigne_gardee(0.60).await;
    let recus = banc.commandes_depuis(deja).await;
    let facteur = 10f64.powf(-6.0 / 20.0);
    let repere = 0.10 / facteur;
    let plafond_appareil = (repere + POINTS_DU_SEUIL) * facteur;
    assert!(
        !recus.is_empty() && recus.iter().all(|v| *v <= plafond_appareil + 1e-9),
        "#6721 : trim −6 dB, appareil à 10 % : reçu {recus:?}, plafond {plafond_appareil}"
    );
}
