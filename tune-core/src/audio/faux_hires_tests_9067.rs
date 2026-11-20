//! #9067 (décision de Bertrand du 19/11/2026, fil 2915) — « Source altérée »
//! couvre tous les formats, CD compris. Un CD (16 bits, 44,1/48 kHz) n'est
//! altéré QUE sur la signature d'un codec avec perte : une coupure EN MUR
//! ([`en_mur`]) à partir de [`COUPURE_MUR_CD_MIN_HZ`] et sous
//! [`COUPURE_MUR_CD_MAX_HZ`] (20 kHz depuis le 19/11, après #9139) ; jamais
//! sur une pente douce, ni sur un filtre qui n'est pas un mur, ni sur un mur
//! à 20 kHz ou au-dessus (le filtre d'un vrai CD). Les
//! règles hi-res ne changent pas. Signaux SYNTHÉTIQUES produits en mémoire
//! (aucun fichier, aucun ffmpeg, aucun son joué).

use super::tests::{quantifier, rose};
use super::tests_9035::{codec_simule, par_spectre, sombre_coupe};
use super::*;

fn juge(pcm: &[i32], sample_rate: u32, bits: u32) -> (Mesure, Verdict) {
    let m = mesurer(pcm, 1, sample_rate, bits);
    let v = juger(&m);
    (m, v)
}

fn coupure_vers(m: &Mesure, attendue: f64) {
    let f = m
        .coupure_hz
        .unwrap_or_else(|| panic!("coupure attendue : {m:?}"));
    assert!(
        (f - attendue).abs() < 400.0,
        "coupure {f:.0} Hz, attendue vers {attendue} Hz : {m:?}"
    );
}

/// 🔴 TÉMOIN — un CD 16/44,1 coupé EN MUR à 16 kHz (le passe-bas d'un MP3 à
/// 128 kbit/s) : source altérée, raison `mur_de_codec`, coupure vers 16 kHz.
#[test]
fn un_cd_avec_un_mur_a_16_khz_est_altere_9067() {
    for (sample_rate, graine) in [(44_100, 9067), (48_000, 9068)] {
        let pcm = quantifier(&rose(sample_rate, 16_000.0, graine), 16);
        let (m, v) = juge(&pcm, sample_rate, 16);
        assert!(m.coupure_en_mur, "{sample_rate} Hz : {m:?}");
        assert!(
            matches!(v, Verdict::Douteux(Raison::MurDeCodec { .. })),
            "{sample_rate} Hz : {v:?} {m:?}"
        );
        assert_eq!(v.code(), "douteux");
        assert_eq!(v.code_raison(), Some(RAISON_MUR_DE_CODEC));
        coupure_vers(&m, 16_000.0);
    }
}

/// 🔴 TÉMOIN — le mur d'un MP3 à 320 kbit/s, vers 19,5 kHz, dans un CD : altéré.
#[test]
fn un_cd_avec_un_mur_a_19_5_khz_est_altere_9067() {
    let pcm = quantifier(&rose(44_100, 19_500.0, 9069), 16);
    let (m, v) = juge(&pcm, 44_100, 16);
    assert!(
        matches!(v, Verdict::Douteux(Raison::MurDeCodec { .. })),
        "{v:?} {m:?}"
    );
    coupure_vers(&m, 19_500.0);
}

/// 🔴 TÉMOIN — un MP3 simulé (passe-bas à 16 kHz ET bandes vidées par
/// intermittence) décodé en 16/44,1 : altéré, par son mur.
#[test]
fn un_mp3_simule_en_16_44_est_altere_9067() {
    let source = par_spectre(44_100, 9070, |f| 1.0 / f.sqrt());
    let pcm = quantifier(&codec_simule(&source, 44_100, 16_000.0, 9071), 16);
    let (m, v) = juge(&pcm, 44_100, 16);
    assert!(
        matches!(v, Verdict::Douteux(Raison::MurDeCodec { .. })),
        "{v:?} {m:?}"
    );
    coupure_vers(&m, 16_000.0);
}

/// 🟢 CONTRE-ÉPREUVE — un CD dont l'aigu tombe en PENTE DOUCE dès 18 kHz
/// (24 dB par octave, une vieille bande) puis le filtre du CD : conforme.
#[test]
fn un_cd_en_pente_douce_a_18_khz_est_conforme_9067() {
    for ordre in [4, 8] {
        let pcm = quantifier(
            &par_spectre(44_100, 9072 + ordre as u64, |f| {
                if f < 21_800.0 {
                    (1.0 / f.sqrt()) / (1.0 + (f / 18_000.0).powi(2 * ordre)).sqrt()
                } else {
                    0.0
                }
            }),
            16,
        );
        let (m, v) = juge(&pcm, 44_100, 16);
        assert_eq!(v, Verdict::Conforme, "pente d'ordre {ordre} : {m:?}");
    }
}

/// 🟢 CONTRE-ÉPREUVE — un master SOMBRE en 16/44,1 (l'aigu décroît dès
/// 2 kHz, 24 dB par octave de plus que le rose) avec le filtre du CD : rien
/// n'est inventé sous 20 kHz, conforme.
#[test]
fn un_cd_sombre_est_conforme_9067() {
    let pcm = quantifier(&sombre_coupe(44_100, 2_000.0, 21_800.0, 9080), 16);
    let (m, v) = juge(&pcm, 44_100, 16);
    assert_eq!(v, Verdict::Conforme, "{m:?}");
}

/// 🟢 CONTRE-ÉPREUVE du critère du mur — un filtre RAIDE mais pas un mur
/// (23 dB par kHz à partir de 17 kHz sur un spectre plat, jusqu'au plancher
/// des 16 bits) : l'ancienne règle y voyait une coupure suspecte (falaise
/// sous 20,5 kHz) ; ce n'est pas le passe-bas d'un encodeur, le CD reste
/// conforme. Le même spectre dans un 24/44,1 garde la règle hi-res
/// (douteux), qui ne change pas.
#[test]
fn un_filtre_raide_qui_n_est_pas_un_mur_laisse_le_cd_conforme_9067() {
    let rampe = |f: f64| {
        if f <= 17_000.0 {
            1.0
        } else {
            10f64.powf(-23.0 * (f - 17_000.0) / 1_000.0 / 20.0)
        }
    };
    let pcm = quantifier(&par_spectre(44_100, 9081, rampe), 16);
    let (m, v) = juge(&pcm, 44_100, 16);
    assert!(
        m.coupure_hz.is_some_and(|f| f < COUPURE_LOSSY_MAX_HZ)
            && m.part_coupee() >= PART_COUPEE_MIN,
        "la falaise doit être vue (l'ancienne règle la jugeait suspecte) : {m:?}"
    );
    assert!(!m.coupure_en_mur, "{m:?}");
    assert_eq!(v, Verdict::Conforme, "{m:?}");
    let pcm24 = quantifier(&par_spectre(44_100, 9081, rampe), 24);
    let (_, v24) = juge(&pcm24, 44_100, 24);
    assert!(matches!(v24, Verdict::Douteux(_)), "{v24:?}");
}

/// 🟢 CONTRE-ÉPREUVE — un mur trop BAS pour un encodeur courant (12 kHz :
/// un filtre de restauration de 78 tours) laisse le CD conforme.
#[test]
fn un_cd_avec_un_mur_a_12_khz_reste_conforme_9067() {
    let pcm = quantifier(&rose(44_100, 12_000.0, 9082), 16);
    let (m, v) = juge(&pcm, 44_100, 16);
    assert!(m.coupure_en_mur, "c'est bien un mur : {m:?}");
    assert_eq!(v, Verdict::Conforme, "{m:?}");
}

/// 🔴 Les règles hi-res ne changent pas — un 24/96 coupé en mur à 22 kHz
/// (un CD suréchantillonné) : altéré, raison `bande_de_cd`.
#[test]
fn un_24_96_avec_un_mur_a_22_khz_est_altere_9067() {
    let pcm = quantifier(&rose(96_000, 22_000.0, 9083), 24);
    let (m, v) = juge(&pcm, 96_000, 24);
    assert!(
        matches!(v, Verdict::Douteux(Raison::BandeDeCd { .. })),
        "{v:?} {m:?}"
    );
    coupure_vers(&m, 22_000.0);
}

/// 🟢 CONTRE-ÉPREUVE — un vrai 24/96 (contenu jusqu'à 47 kHz) reste vérifié.
#[test]
fn un_vrai_24_96_reste_verifie_9067() {
    let pcm = quantifier(&rose(96_000, 47_000.0, 9084), 24);
    assert_eq!(juge(&pcm, 96_000, 24).1, Verdict::Verifie);
}

/// Le verdict effectif en SQL : un CD n'est `douteux` que par un mur de codec.
#[test]
fn le_verdict_effectif_d_un_cd_suit_le_mur_9067() {
    let sql = verdict_effectif_sql("h");
    assert!(sql.contains("'mur_de_codec'"), "{sql}");
    assert!(raison_de_coupure("mur_de_codec"));
    assert!(raison_de_coupure("coupure_de_codec"));
    assert!(!raison_de_coupure("bits_de_remplissage"));
}

/// La remesure et le verdict effectif d'un CD rangé `mur_de_codec` par #9139
/// avec un mur entre 20,0 et 20,5 kHz : la borne de 20 kHz le rend conforme.
#[test]
fn un_mur_de_codec_range_a_20_khz_ou_plus_se_lit_conforme_9067() {
    let effectif = verdict_effectif_sql("h");
    let remesure = cd_a_remesurer_sql("h");
    for sql in [&effectif, &remesure] {
        assert!(sql.contains("h.coupure_hz, 0) >= 20000"), "{sql}");
    }
}

/// #9067 (décision du 19/11 après #9139) — la frontière HAUTE du mur de codec
/// d'un CD : 20 kHz, plus 20,5 kHz.
#[test]
fn la_borne_haute_du_mur_d_un_cd_est_20_khz_9067() {
    assert_eq!(COUPURE_MUR_CD_MAX_HZ, 20_000.0);
    assert!(COUPURE_MUR_CD_MIN_HZ < COUPURE_MUR_CD_MAX_HZ);
    // Les règles hi-res gardent 20,5 kHz.
    assert_eq!(COUPURE_LOSSY_MAX_HZ, 20_500.0);
}

/// 🟢 FRONTIÈRE — un vrai CD dont le filtre coupe EN MUR à 20,25 kHz (entre
/// 20,0 et 20,5 kHz) : CONFORME, en 44,1 comme en 48 kHz. Ne doit plus jamais
/// être marqué altéré.
#[test]
fn un_cd_avec_un_mur_a_20_25_khz_est_conforme_9067() {
    for (sample_rate, graine) in [(44_100, 9139), (48_000, 9140)] {
        let pcm = quantifier(&rose(sample_rate, 20_250.0, graine), 16);
        let (m, v) = juge(&pcm, sample_rate, 16);
        assert!(
            m.coupure_en_mur,
            "{sample_rate} Hz : c'est bien un mur : {m:?}"
        );
        coupure_vers(&m, 20_250.0);
        assert_eq!(v, Verdict::Conforme, "{sample_rate} Hz : {m:?}");
    }
}

/// 🔴 FRONTIÈRE — un CD coupé EN MUR à 19,5 kHz (sous la borne de 20 kHz) :
/// altéré, raison `mur_de_codec`, en 44,1 comme en 48 kHz.
#[test]
fn un_cd_avec_un_mur_a_19_5_khz_reste_altere_sous_la_borne_9067() {
    for (sample_rate, graine) in [(44_100, 9141), (48_000, 9142)] {
        let pcm = quantifier(&rose(sample_rate, 19_500.0, graine), 16);
        let (m, v) = juge(&pcm, sample_rate, 16);
        assert!(m.coupure_en_mur, "{sample_rate} Hz : {m:?}");
        coupure_vers(&m, 19_500.0);
        assert!(
            m.coupure_hz.is_some_and(|f| f < COUPURE_MUR_CD_MAX_HZ),
            "{sample_rate} Hz : {m:?}"
        );
        assert_eq!(v.code(), "douteux", "{sample_rate} Hz : {v:?} {m:?}");
        assert_eq!(v.code_raison(), Some(RAISON_MUR_DE_CODEC));
    }
}

/// 🔴 CONTRE-ÉPREUVE de la frontière — avec l'ANCIENNE borne de #9139
/// (20,5 kHz), le CD au mur de 20,25 kHz était jugé altéré : la mesure tombe
/// dans l'ancienne plage et hors de la nouvelle. Si la borne remontait à
/// 20,5 kHz, `un_cd_avec_un_mur_a_20_25_khz_est_conforme_9067` échouerait.
#[test]
fn avec_l_ancienne_borne_le_cd_a_20_25_khz_etait_altere_9067() {
    const ANCIENNE_BORNE_HZ: f64 = 20_500.0;
    let pcm = quantifier(&rose(44_100, 20_250.0, 9139), 16);
    let (m, v) = juge(&pcm, 44_100, 16);
    let f = m.coupure_hz.expect("coupure mesurée");
    assert!(m.coupure_en_mur, "{m:?}");
    assert!(
        (COUPURE_MUR_CD_MIN_HZ..ANCIENNE_BORNE_HZ).contains(&f),
        "l'ancienne règle aurait dit `mur_de_codec` : {m:?}"
    );
    assert!(
        !(COUPURE_MUR_CD_MIN_HZ..COUPURE_MUR_CD_MAX_HZ).contains(&f),
        "{m:?}"
    );
    assert_eq!(v, Verdict::Conforme);
}
