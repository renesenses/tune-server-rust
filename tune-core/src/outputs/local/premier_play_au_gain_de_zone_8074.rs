//! #8074 — le premier morceau après le démarrage ne part jamais à pleine
//! échelle.
//!
//! Un testeur sous Windows (sortie locale en WASAPI exclusif, volume logiciel,
//! loi `db60`, hors PURE) : zone réglée vers 30 %, le premier morceau après le
//! démarrage du serveur est parti « au-delà de 80 % ». Une `LocalOutput` naît
//! à pleine échelle (`user_volume` à l'unité) et le rendu WASAPI pondère par
//! ce gain-là (`GainAuRendu`, #7264) : tant qu'aucune graine n'est posée, les
//! premiers tampons sortent à 1.0.
//!
//! Ces témoins rejouent le démarrage sans carte son : premier Play sur une
//! sortie neuve, gain de zone à 30 % du curseur `db60`, et le flux rouvert
//! entre deux pistes. Le gain rendu se lit sur les mots que le fil de rendu
//! produirait (`GainAuRendu::rendre`, mots 32 bits : pas de dither), rapporté
//! au mot d'entrée : la mesure ne dépend pas de l'unité des atomiques.
use super::LocalOutput;
use super::gain_au_rendu_7264::GainAuRendu;
use crate::audio::volume_scale::gain_perceptif;
use crate::outputs::traits::OutputTarget;

/// −6 dBFS sur 32 bits, aligné à gauche.
const MOT: i32 = 0x4000_0000;
const CADENCE: u32 = 44_100;
const VOIES: u32 = 2;
/// Une adresse où personne n'écoute : `play_url` passe sa garde, ouvre son
/// fil, et la lecture échoue sans jamais rien jouer.
const URL_MORTE: &str = "http://127.0.0.1:9/8074.wav";

/// Le gain des `trames` premières trames que rendrait un flux ouvert MAINTENANT
/// sur `sortie` (ouverture WASAPI exclusif : `GainAuRendu::nouveau` sur le
/// gain partagé de la sortie). Rend le plus fort des gains observés.
fn gain_des_premiers_tampons(sortie: &LocalOutput, trames: u64) -> f64 {
    let mut rendu = GainAuRendu::nouveau(sortie.gain_de_rendu(), CADENCE, VOIES);
    let mut pire = 0.0_f64;
    for position in 0..trames * u64::from(VOIES) {
        let mot = rendu.rendre(MOT, 32, position);
        pire = pire.max(f64::from(mot) / f64::from(MOT));
    }
    pire
}

/// 30 % du curseur `db60` : −42 dB.
fn trente_pour_cent_db60() -> f64 {
    gain_perceptif(0.30)
}

fn en_db(gain: f64) -> f64 {
    20.0 * gain.max(1e-12).log10()
}

/// LE défaut, au premier Play : une sortie née sans graine ne rend pas ses
/// premiers tampons à pleine échelle. `play_url` pose d'abord le niveau de
/// départ sans repère (30 % du curseur `db60`).
#[tokio::test]
async fn premier_play_sur_une_sortie_sans_graine_ne_sort_pas_a_pleine_echelle_8074() {
    let sortie = LocalOutput::with_options("Haut-parleurs".into(), true, "wasapi");
    assert!(
        !sortie.volume_est_pose(),
        "une sortie neuve n'a reçu aucun volume"
    );

    sortie
        .play_url(URL_MORTE, "audio/wav", None, None)
        .await
        .ok();
    sortie.stop().await.ok();

    let gain = gain_des_premiers_tampons(&sortie, 4096);
    let attendu = trente_pour_cent_db60();
    assert!(
        (en_db(gain) - en_db(attendu)).abs() < 0.1,
        "#8074 : premier Play sur une sortie locale sans graine, les premiers tampons \
         sortent à {:.1} dB ({gain:.4}) au lieu de {:.1} dB : le gain de naissance (1.0, \
         pleine échelle) n'a pas été remplacé avant l'ouverture du flux",
        en_db(gain),
        en_db(attendu),
    );
}

/// Premier Play après une graine de zone à 30 % (`db60`) : les premiers
/// tampons sortent à ce gain, et la garde ne le remplace pas.
#[tokio::test]
async fn premier_play_apres_la_graine_de_zone_sort_au_gain_de_la_zone_8074() {
    let sortie = LocalOutput::with_options("Haut-parleurs".into(), true, "wasapi");
    let zone = trente_pour_cent_db60();
    OutputTarget::set_volume(&sortie, zone).await.unwrap();

    sortie
        .play_url(URL_MORTE, "audio/wav", None, None)
        .await
        .ok();
    sortie.stop().await.ok();

    let gain = gain_des_premiers_tampons(&sortie, 4096);
    assert!(
        (en_db(gain) - en_db(zone)).abs() < 0.1,
        "graine à 30 % db60 : les premiers tampons doivent sortir à {:.1} dB, \
         mesuré {:.1} dB",
        en_db(zone),
        en_db(gain),
    );
}

/// Le flux est rouvert à chaque piste (le journal du testeur : un
/// `wasapi_exclusive_initialized` par morceau). Le gain posé survit à chaque
/// réouverture, et la garde ne le ramène ni à l'unité ni au départ bas.
#[tokio::test]
async fn flux_rouvert_entre_deux_pistes_garde_le_gain_de_la_zone_8074() {
    let sortie = LocalOutput::with_options("Haut-parleurs".into(), true, "wasapi");
    // Le volume de zone du rapport (`device_set_volume_sending volume=0.0588…`).
    let zone = 0.058_884_365_535_558_9;
    OutputTarget::set_volume(&sortie, zone).await.unwrap();

    for piste in 1..=3 {
        sortie
            .play_url(URL_MORTE, "audio/wav", None, None)
            .await
            .ok();
        sortie.stop().await.ok();
        let gain = gain_des_premiers_tampons(&sortie, 4096);
        assert!(
            (en_db(gain) - en_db(zone)).abs() < 0.1,
            "piste {piste} : le flux rouvert rend {:.1} dB au lieu de {:.1} dB",
            en_db(gain),
            en_db(zone),
        );
    }
}

/// La sourdine posée avant la première lecture est un niveau posé : la garde
/// ne la lève pas.
#[tokio::test]
async fn sourdine_posee_avant_le_premier_play_reste_muette_8074() {
    let sortie = LocalOutput::with_options("Haut-parleurs".into(), true, "wasapi");
    OutputTarget::set_mute(&sortie, true).await.unwrap();

    sortie
        .play_url(URL_MORTE, "audio/wav", None, None)
        .await
        .ok();
    sortie.stop().await.ok();

    assert_eq!(gain_des_premiers_tampons(&sortie, 512), 0.0);
}

/// PURE et bit-perfect strict : la garde ne touche pas au signal. Leur
/// volume (100 %) vient de la graine ; sans graine, elle ne le réécrit pas.
#[tokio::test]
async fn pure_et_bit_perfect_strict_ne_sont_pas_touches_par_la_garde_8074() {
    let pure = LocalOutput::with_options("DAC".into(), true, "wasapi");
    pure.set_pure_bypass(true);
    assert!(!pure.partir_bas_si_aucun_volume_pose().await);
    assert_eq!(
        gain_des_premiers_tampons(&pure, 512),
        1.0,
        "PURE : signal intact"
    );

    let strict = LocalOutput::with_options("DAC".into(), true, "wasapi");
    strict.set_strict_bitperfect(true);
    assert!(!strict.partir_bas_si_aucun_volume_pose().await);
    assert_eq!(
        gain_des_premiers_tampons(&strict, 512),
        1.0,
        "bit-perfect strict : signal intact"
    );

    // Graine PURE (100 %) puis lecture hors PURE : 100 % est un niveau
    // posé, la garde ne le rabaisse pas.
    let graine_pleine = LocalOutput::with_options("DAC".into(), true, "wasapi");
    OutputTarget::set_volume(&graine_pleine, 1.0).await.unwrap();
    assert!(!graine_pleine.partir_bas_si_aucun_volume_pose().await);
    assert_eq!(gain_des_premiers_tampons(&graine_pleine, 512), 1.0);
}

/// Le niveau de départ sans repère n'est jamais l'unité, et il est bas.
#[test]
fn le_depart_sans_repere_est_bas_8074() {
    let g = crate::audio::reglages_de_volume::gain_de_depart_sans_repere_local();
    assert!(
        (en_db(g) - (-42.0)).abs() < 0.01,
        "attendu −42 dB, {:.2} dB",
        en_db(g)
    );
}

/// Le bras ASIO : une sortie ASIO neuve naît, elle aussi, à l'unité, et le
/// bras ASIO (`bras_asio.rs`, Windows seulement) pondère au rendu par le même
/// gain partagé (#7934). La garde de `play_url` passe AVANT l'aiguillage vers
/// le bras : sans graine, la sortie ASIO part bas, comme la WASAPI.
#[tokio::test]
async fn premier_play_sur_une_sortie_asio_sans_graine_ne_sort_pas_a_pleine_echelle_8074() {
    let sortie = LocalOutput::with_options("HoloAudio ASIO Driver".into(), true, "asio");
    assert!(
        !sortie.volume_est_pose(),
        "une sortie ASIO neuve n'a reçu aucun volume"
    );
    sortie
        .play_url(URL_MORTE, "audio/wav", None, None)
        .await
        .ok();
    sortie.stop().await.ok();
    let gain = gain_des_premiers_tampons(&sortie, 4096);
    let attendu = trente_pour_cent_db60();
    assert!(
        (en_db(gain) - en_db(attendu)).abs() < 0.1,
        "#8074 : premier Play sur une sortie ASIO sans graine, les premiers tampons \
         sortent à {:.1} dB ({gain:.4}) au lieu de {:.1} dB : le gain de naissance (1.0, \
         pleine échelle) n'a pas été remplacé avant le bras ASIO",
        en_db(gain),
        en_db(attendu),
    );
}
