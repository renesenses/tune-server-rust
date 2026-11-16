//! SÉCURITÉ volume — une seule conversion curseur → dB pour AirPlay 1 et
//! AirPlay 2 (#8855, fil 2749).
//!
//! ## Le défaut
//!
//! L'échelle de volume AirPlay va de -30 dB (minimum audible) à 0 dB
//! (maximum), -144 dB valant sourdine. La plupart des récepteurs projettent
//! cette plage **linéairement** sur leur propre curseur 0–100 %. Or :
//!
//! - le démon AirPlay 2 (`airplay2-rs`, `build_set_volume`) convertissait le
//!   niveau reçu par `20·log10`, sans plancher : une zone à 65 % partait à
//!   -3,7 dB, soit ~88 % de l'enceinte ; le « départ prudent » de 30 % à
//!   -10,5 dB, ~65 % ;
//! - la sortie AirPlay 1 de Tune employait `30·log10` : une autre courbe,
//!   donc un autre niveau pour le même curseur.
//!
//! ## La courbe
//!
//! `dB = -30 × (1 − curseur)`, 0 → -144 (sourdine), 1 → 0 dB. C'est la
//! projection qu'emploient les émetteurs d'Apple (le curseur d'iOS et de
//! macOS va linéairement de -30 à 0 dB) : un récepteur qui projette
//! -30..0 dB linéairement affiche alors **le curseur de la zone**. Une zone
//! à 65 % part à 65 % de l'enceinte, pas à 88 %.
//!
//! ## AirPlay 2 sans toucher au démon
//!
//! Le démon n'accepte qu'un niveau 0..1 (`{"cmd":"volume","level":…}`) et
//! écrit `20·log10(level)` dans le `SET_PARAMETER`. Tune lui donne donc
//! `10^(dB/20)` ([`niveau_pour_le_demon`]) : le démon retombe exactement
//! sur le dB voulu, plancher compris, sans nouvelle rev d'`airplay2-rs`.
//! L'événement `volume_set` qu'il renvoie est ignoré par la sortie.

/// Le plancher de l'échelle AirPlay : le plus bas niveau audible.
pub const PLANCHER_DB: f64 = -30.0;

/// La sourdine AirPlay.
pub const SOURDINE_DB: f64 = -144.0;

/// Le dB AirPlay d'un curseur de zone (0..1). 0, un curseur négatif ou non
/// fini : sourdine. Au-delà de 1 : 0 dB. Entre les deux : linéaire sur
/// [-30, 0] dB, jamais sous le plancher.
pub fn curseur_vers_db(curseur: f64) -> f64 {
    if !curseur.is_finite() || curseur <= 0.0 {
        SOURDINE_DB
    } else if curseur >= 1.0 {
        0.0
    } else {
        PLANCHER_DB * (1.0 - curseur)
    }
}

/// Le niveau (0..1) à donner au démon AirPlay 2 pour qu'il envoie
/// [`curseur_vers_db`] : l'inverse de son `20·log10`. 0 garde la sourdine
/// du démon (-144 dB), 1 son 0 dB.
pub fn niveau_pour_le_demon(curseur: f64) -> f64 {
    let db = curseur_vers_db(curseur);
    if db <= SOURDINE_DB {
        0.0
    } else if db >= 0.0 {
        1.0
    } else {
        10f64.powf(db / 20.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ce que le démon (`airplay-rtsp::build_set_volume`, rev `d58df6265`)
    /// écrit dans le corps du `SET_PARAMETER` pour un niveau reçu : copie
    /// fidèle, en `f32` comme lui.
    fn corps_du_demon(niveau: f64) -> String {
        let volume = niveau as f32;
        let volume_db = if volume <= 0.0 {
            -144.0_f32
        } else if volume >= 1.0 {
            0.0_f32
        } else {
            20.0 * volume.log10()
        };
        format!("volume: {volume_db:.2}\r\n")
    }

    #[test]
    fn bornes_zero_et_un_8855() {
        assert_eq!(curseur_vers_db(0.0), SOURDINE_DB);
        assert_eq!(curseur_vers_db(-0.5), SOURDINE_DB);
        assert_eq!(curseur_vers_db(f64::NAN), SOURDINE_DB);
        assert_eq!(curseur_vers_db(f64::INFINITY), SOURDINE_DB);
        assert_eq!(curseur_vers_db(1.0), 0.0);
        assert_eq!(curseur_vers_db(1.7), 0.0);
        assert_eq!(niveau_pour_le_demon(0.0), 0.0);
        assert_eq!(niveau_pour_le_demon(1.0), 1.0);
    }

    /// Le tableau de la PR : curseur → dB → niveau du récepteur (-30..0 dB
    /// projeté linéairement) = le curseur.
    #[test]
    fn table_de_conversion_8855() {
        for (curseur, db) in [
            (0.01, -29.7),
            (0.10, -27.0),
            (0.20, -24.0),
            (0.30, -21.0),
            (0.50, -15.0),
            (0.65, -10.5),
            (0.70, -9.0),
            (0.90, -3.0),
        ] {
            let obtenu = curseur_vers_db(curseur);
            assert!(
                (obtenu - db).abs() < 1e-9,
                "{curseur} → {obtenu} dB, attendu {db}"
            );
            let recepteur = (obtenu - PLANCHER_DB) / -PLANCHER_DB;
            assert!(
                (recepteur - curseur).abs() < 1e-9,
                "récepteur {recepteur} pour {curseur}"
            );
        }
    }

    /// Strictement croissante sur ]0, 1], jamais sous le plancher.
    #[test]
    fn monotone_et_bornee_par_le_plancher_8855() {
        let mut precedent = SOURDINE_DB;
        for pas in 1..=10_000 {
            let curseur = f64::from(pas) / 10_000.0;
            let db = curseur_vers_db(curseur);
            assert!(db > precedent, "non croissante en {curseur}");
            assert!((PLANCHER_DB..=0.0).contains(&db), "{curseur} → {db} dB");
            let niveau = niveau_pour_le_demon(curseur);
            assert!(niveau > 0.0 && niveau <= 1.0);
            precedent = db;
        }
    }

    /// Ce que le démon envoie vraiment au récepteur, au centième de dB près,
    /// pour tout curseur au millième : la courbe de Tune, pas la sienne.
    #[test]
    fn le_demon_envoie_le_db_de_tune_8855() {
        for pas in 0..=1000 {
            let curseur = f64::from(pas) / 1000.0;
            let attendu = format!("volume: {:.2}\r\n", curseur_vers_db(curseur) as f32);
            assert_eq!(
                corps_du_demon(niveau_pour_le_demon(curseur)),
                attendu,
                "curseur {curseur}"
            );
        }
        assert_eq!(
            corps_du_demon(niveau_pour_le_demon(0.30)),
            "volume: -21.00\r\n"
        );
        assert_eq!(
            corps_du_demon(niveau_pour_le_demon(0.65)),
            "volume: -10.50\r\n"
        );
        assert_eq!(
            corps_du_demon(niveau_pour_le_demon(0.50)),
            "volume: -15.00\r\n"
        );
        assert_eq!(
            corps_du_demon(niveau_pour_le_demon(1.0)),
            "volume: 0.00\r\n"
        );
        assert_eq!(
            corps_du_demon(niveau_pour_le_demon(0.0)),
            "volume: -144.00\r\n"
        );
        // Avant #8855 : le curseur brut donnait -3,74 dB à 65 %.
        assert_eq!(corps_du_demon(0.65), "volume: -3.74\r\n");
    }
}
