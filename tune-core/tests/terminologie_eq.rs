//! Le vocabulaire de l'égaliseur doit décrire ce que le code fait — #2213.
//!
//! Deux mots précis étaient employés pour autre chose que ce qu'ils désignent :
//!
//! 1. « bit-perfect ». L'entête de `audio/eq.rs` annonçait « Processing is done
//!    in f64 for bit-perfect quality ». La précision interne du calcul et
//!    l'identité binaire du signal sont deux propriétés différentes : un
//!    égaliseur actif modifie les échantillons, quelle que soit la largeur de
//!    ses accumulateurs. Le f64 est un vrai argument — il évite le bruit de
//!    quantification du calcul — mais ce n'est pas celui-là.
//!
//! 2. « room correction ». `room_correction_preset()` choisissait trois gains
//!    d'après deux énumérations, `Small/Medium/Large` et
//!    `NearWall/FreeStanding`. Aucune mesure n'y entre. Dans le vocabulaire
//!    audiophile, « correction de pièce » désigne un traitement calculé à
//!    partir d'une mesure acoustique — c'est ce que Tune fait par ailleurs,
//!    dans `room_correction.rs` et `audio/convolver.rs`, à partir d'un fichier
//!    d'impulsion exporté par REW, Acourate ou Audiolense.
//!
//! Les tests ci-dessous lient le LIBELLÉ au COMPORTEMENT, dans les deux sens :
//! le mot ne doit être employé que là où la chose existe, et il doit rester là
//! où elle existe vraiment.

use tune_core::audio::eq::{EqProcessor, EqProfile, ListeningMode, RoomSize, SpeakerPlacement};

/// Le source du module d'égalisation, lu à la compilation.
const SOURCE_EQ: &str = include_str!("../src/audio/eq.rs");

/// Le source de la VRAIE correction de pièce, celle qui part d'une mesure.
const SOURCE_ROOM_CORRECTION: &str = include_str!("../src/room_correction.rs");

/// L'entête `//!` du module : la partie qui décrit le module à qui l'ouvre.
fn entete_du_module(source: &str) -> String {
    source
        .lines()
        .take_while(|l| l.trim_start().starts_with("//!") || l.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Un profil dont les gains sont audibles.
fn profil_actif() -> EqProfile {
    EqProfile {
        enabled: true,
        bass_gain_db: 6.0,
        treble_gain_db: -6.0,
        ..EqProfile::default()
    }
}

/// Une sinusoïde stéréo entrelacée.
fn sinus_stereo(freq: f64, trames: usize, taux: u32) -> Vec<f32> {
    let mut v = vec![0.0f32; trames * 2];
    for f in 0..trames {
        let s = (2.0 * std::f64::consts::PI * freq * f as f64 / taux as f64).sin() * 0.5;
        v[2 * f] = s as f32;
        v[2 * f + 1] = s as f32;
    }
    v
}

// ---------------------------------------------------------------------------
// 1. « bit-perfect »
// ---------------------------------------------------------------------------

/// Sens 1 : un égaliseur actif MODIFIE les échantillons. L'entête du module n'a
/// donc pas le droit de présenter le bit-perfect comme une qualité de son
/// traitement.
///
/// Sens 2 : le chemin qui EST réellement intact — égaliseur désactivé — doit le
/// rester. Si quelqu'un rendait l'EQ inaudible, le premier assert tomberait et
/// c'est la fonction, pas le mot, qu'il faudrait corriger.
#[test]
fn un_egaliseur_actif_modifie_le_signal_donc_l_entete_ne_promet_pas_le_bit_perfect() {
    // --- comportement : actif => le signal change
    let mut eq = EqProcessor::new(&profil_actif(), 44_100, 2);
    assert!(
        eq.is_enabled(),
        "le profil de test doit produire des filtres audibles"
    );
    let avant = sinus_stereo(1_000.0, 1_024, 44_100);
    let mut apres = avant.clone();
    eq.process_interleaved(&mut apres);
    assert_ne!(
        apres, avant,
        "un égaliseur actif doit modifier les échantillons — sinon le mot \
         « bit-perfect » redeviendrait légitime et ce test n'a plus lieu d'être"
    );

    // --- comportement : désactivé => le signal est strictement intact
    let mut neutre = EqProcessor::new(&EqProfile::default(), 44_100, 2);
    let mut intact = avant.clone();
    neutre.process_interleaved(&mut intact);
    assert_eq!(
        intact, avant,
        "égaliseur désactivé : le signal doit sortir bit à bit identique"
    );

    // --- libellé : l'entête ne doit plus vendre le f64 comme du bit-perfect
    let entete = entete_du_module(SOURCE_EQ);
    let claim = concat!("bit-", "perfect quality");
    assert!(
        !entete.contains(claim),
        "l'entête de audio/eq.rs promet encore « {claim} » alors que le \
         traitement modifie les échantillons :\n{entete}"
    );

    // --- libellé, sens inverse : elle doit DIRE que le traitement modifie le
    // signal. Retirer la fausse promesse sans la remplacer laisserait le
    // lecteur sans réponse.
    let bas = entete.to_lowercase();
    assert!(
        bas.contains("modifie") || bas.contains("alters") || bas.contains("modifies"),
        "l'entête de audio/eq.rs ne dit nulle part que l'égaliseur modifie le \
         signal — la précision f64 doit être présentée pour ce qu'elle est, \
         pas comme une identité binaire :\n{entete}"
    );
}

// ---------------------------------------------------------------------------
// 2. « correction de pièce »
// ---------------------------------------------------------------------------

/// Sens 1 : le preset d'environnement ne dépend QUE des trois énumérations.
/// Aucune mesure n'y entre, donc le module ne doit pas employer le vocabulaire
/// de la correction de pièce.
///
/// Sens 2 : la correction de pièce existe bel et bien dans Tune, fondée sur une
/// mesure importée. Le module qui la porte doit garder le mot ET la mesure —
/// sinon on aurait « corrigé » le vocabulaire en effaçant la vraie
/// fonctionnalité.
#[test]
fn le_preset_d_environnement_ne_mesure_rien_donc_ne_s_appelle_pas_correction_de_piece() {
    // --- comportement : deux profils aux mêmes énumérations donnent exactement
    // les mêmes décalages, quels que soient les curseurs. Le preset est une
    // table à six entrées, pas le résultat d'une analyse.
    let a = EqProfile {
        listening: ListeningMode::Speakers,
        room_size: RoomSize::Small,
        speaker_placement: SpeakerPlacement::NearWall,
        bass_gain_db: 0.0,
        ..EqProfile::default()
    };
    let b = EqProfile {
        bass_gain_db: 9.0,
        treble_gain_db: -3.0,
        ..a.clone()
    };
    let (ba, ma, ta) = a.effective_gains();
    let (bb, mb, tb) = b.effective_gains();
    assert_eq!(
        (bb - ba, mb - ma, tb - ta),
        (9.0, 0.0, -3.0),
        "l'écart entre les deux profils doit être EXACTEMENT celui des curseurs : \
         la part « environnement » ne dépend que des énumérations"
    );

    // Les six combinaisons enceintes sont bien six constantes distinctes de
    // toute mesure : changer la taille de pièce change le résultat, et c'est
    // tout ce qui le change.
    let petite = EqProfile {
        room_size: RoomSize::Small,
        ..a.clone()
    };
    let grande = EqProfile {
        room_size: RoomSize::Large,
        ..a.clone()
    };
    assert_ne!(
        petite.effective_gains(),
        grande.effective_gains(),
        "la taille déclarée de la pièce doit rester le seul levier de ce preset"
    );

    // --- libellé : plus rien, dans audio/eq.rs, ne PORTE le nom d'une
    // correction de pièce.
    //
    // On vise le vocabulaire en position de NOM, pas la prose : le commentaire
    // qui explique précisément que ce preset n'est PAS une correction de pièce
    // doit rester lisible, et il a besoin de prononcer le mot pour le récuser.
    // Interdire le mot partout punirait la phrase qui fait le travail.
    for identifiant in [
        concat!("room_", "correction_preset"),
        concat!("fn room_", "correction"),
        concat!("self.room_", "correction"),
    ] {
        assert!(
            !SOURCE_EQ.contains(identifiant),
            "audio/eq.rs nomme encore « {identifiant} » un preset qui ne repose \
             sur aucune mesure — trois questions et deux énumérations"
        );
    }

    // L'entête du module, elle, est le résumé qu'on lit sans descendre dans le
    // code : elle ne doit pas laisser croire à une correction de pièce.
    let entete = entete_du_module(SOURCE_EQ).to_lowercase();
    for terme in [
        concat!("room ", "correction"),
        concat!("room_", "correction"),
        concat!("correction de ", "piece"),
        concat!("correction de ", "pièce"),
    ] {
        assert!(
            !entete.contains(terme),
            "l'entête de audio/eq.rs annonce « {terme} » : ce module ne mesure rien"
        );
    }

    // --- libellé, sens inverse : la VRAIE correction de pièce garde son nom et
    // sa mesure.
    assert!(
        SOURCE_ROOM_CORRECTION.contains("measurement_data"),
        "room_correction.rs ne porte plus de données de mesure : le mot \
         « correction de pièce » ne serait alors mérité nulle part dans Tune"
    );
    assert!(
        SOURCE_ROOM_CORRECTION
            .to_lowercase()
            .contains(concat!("room ", "correction")),
        "room_correction.rs a perdu son propre vocabulaire : c'est pourtant le \
         seul module fondé sur une mesure, et le seul qui y ait droit"
    );
    assert!(
        SOURCE_ROOM_CORRECTION.contains("derived from measurement"),
        "room_correction.rs ne dit plus que ses filtres viennent d'une mesure"
    );
}
