use super::LocalOutput;

// -----------------------------------------------------------------------
// #1725 — un curseur bouge pendant la lecture, le son doit suivre.
//
// `set_eq` n'etait appele qu'au demarrage d'une piste, faute de connaitre
// le couple (taux, canaux) auquel batir les biquads. `current_format` le
// memorise ; ces tests verrouillent l'empaquetage, dont depend la
// reconstruction a chaud.
// -----------------------------------------------------------------------

#[test]
fn empaquetage_aller_retour_sur_les_formats_courants() {
    for (taux, canaux) in [
        (44_100u32, 2u16),
        (48_000, 2),
        (96_000, 2),
        (192_000, 2),
        (352_800, 2),
        (768_000, 2),
        (44_100, 1),
        (48_000, 8),
    ] {
        let empaquete = LocalOutput::pack_format(taux, canaux);
        assert_ne!(empaquete, 0, "{taux}/{canaux} doit s'empaqueter");
        assert_eq!(empaquete >> 8, taux, "taux perdu pour {taux}/{canaux}");
        assert_eq!(
            (empaquete & 0xFF) as u16,
            canaux,
            "canaux perdus pour {taux}/{canaux}"
        );
    }
}

/// Zero = « aucun flux ». Batir un EqProcessor pour un format inconnu
/// donnerait des coefficients faux, donc mieux vaut ne rien pousser.
#[test]
fn un_format_absent_ou_aberrant_ne_s_empaquette_pas() {
    assert_eq!(LocalOutput::pack_format(0, 2), 0, "taux nul");
    assert_eq!(LocalOutput::pack_format(44_100, 0), 0, "zero canal");
    assert_eq!(
        LocalOutput::pack_format(0x0100_0000, 2),
        0,
        "un taux qui deborde les 24 bits doit dire « pas de flux » plutot \
         que de rendre un taux tronque"
    );
    assert_eq!(LocalOutput::pack_format(44_100, 256), 0, "trop de canaux");
}

/// Une sortie neuve n'a pas de flux : rien a rebatir.
#[test]
fn une_sortie_neuve_n_annonce_aucun_format() {
    let sortie = LocalOutput::new("format-test".to_string());
    assert_eq!(sortie.current_format(), None);
}
