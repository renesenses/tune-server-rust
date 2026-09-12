//! R5 de #2219 — **un bloc PCM ne se réétiquette pas.**
//!
//! # Ce que ces témoins gardent, et ce que le COMPILATEUR garde déjà
//!
//! L'essentiel de cette tranche n'est pas ici : il est dans les types. Trois
//! choses ne compilent plus du tout, et aucun témoin n'a à les surveiller.
//!
//! * `spec.profondeur = …` sur un [`AudioSpec`] — champ privé, caisse
//!   étrangère : `error[E0616]`. Une étiquette ne se change pas après coup ;
//!   on en construit une neuve, entière.
//! * `etage.frame_bytes = …` — le champ n'existe plus : `error[E0609]`. Les
//!   octets par trame sont **déduits** de la profondeur et des canaux à chaque
//!   demande. Le défaut qu'ils fermaient était réel : à une frontière gapless,
//!   quatre affectations de suite posaient cadence, canaux, profondeur, puis
//!   `frame_bytes` recalculé à la main. Oublier la quatrième, c'était lire un
//!   flux 24 bits par trames de 16 — tout le reste de la piste décalé d'un
//!   octet, le bruit blanc de #3849 — et cela compilait.
//! * `AudioSpec::nouvelle(cadence, canaux, profondeur)` — l'interversion des
//!   deux derniers arguments est une erreur de type, là où deux `u16` voisins
//!   s'échangeaient sans un mot. `depuis_entete` est l'exception, et la seule :
//!   un en-tête WAV rend deux nombres, donc elle prend deux `u16`. Là le
//!   compilateur ne peut rien, et c'est le jeu fermé qui rattrape — un témoin
//!   ci-dessous le mesure.
//!
//! Ce qui RESTE à garder par un témoin est ce que le typage ne peut pas
//! atteindre : l'arithmétique dérivée, le jeu fermé des profondeurs, et la
//! partition des octets. C'est ce fichier.
//!
//! # 🔴 Ce que ces témoins NE voient PAS
//!
//! * **Une `AudioSpec` qui ment.** Rien ici ne lit un en-tête. Étiqueter du
//!   16 bits en 24 bits est cohérent de bout en bout et parfaitement faux : ce
//!   que le type garantit, c'est qu'à partir du moment où l'étiquette est
//!   posée, plus personne en aval ne la contredit. Ce que le puits reçoit
//!   RÉELLEMENT est gardé ailleurs, contre le décodeur de référence du format
//!   (`outputs/local/capture_bout_en_bout_2218.rs`, sous `ci:full`).
//! * **Que `outputs::local` s'en serve.** Cette caisse ne voit pas tune-core.
//!   Le fait que le chemin de conversion passe bien par ces types est gardé par
//!   les relevés de R1 (`outputs/local/empreinte_du_puits_r1.rs`) : ils
//!   tombent sur la même empreinte, ce qui ne serait pas le cas si un octet
//!   avait changé de route.
//! * **Le DSP, le rééchantillonnage, le DoP.** Rien de tout cela n'est ici : ce
//!   fichier ne parle que de l'étiquette et du découpage en trames.

use tune_output_api::{AudioSpec, ProfondeurPcm};

/// Les quatre profondeurs du jeu fermé, et rien d'autre.
const JEU_FERME: [ProfondeurPcm; 4] = [
    ProfondeurPcm::FlottantIeee32,
    ProfondeurPcm::Entier16,
    ProfondeurPcm::Entier24,
    ProfondeurPcm::Entier32,
];

fn spec(profondeur: ProfondeurPcm, canaux: u16) -> AudioSpec {
    AudioSpec::nouvelle(44_100, profondeur, canaux).expect("canaux non nuls")
}

/// **Le témoin central.** Les mêmes octets, lus sous deux étiquettes, ne font
/// pas le même nombre de trames — et chaque bloc garde l'étiquette qui l'a
/// produit, pas celle du voisin.
///
/// 48 octets stéréo : 12 trames en 16 bits, 8 en 24, 6 en 32. Les trois
/// lectures sont plausibles ; une seule est la bonne. C'est exactement pour ça
/// qu'un bloc et son format ne peuvent plus voyager séparément.
#[test]
fn le_meme_bloc_ne_rend_pas_les_memes_trames_selon_son_etiquette() {
    let octets = [0u8; 48];

    let attendu = [
        (ProfondeurPcm::Entier16, 12usize),
        (ProfondeurPcm::Entier24, 8),
        (ProfondeurPcm::Entier32, 6),
        (ProfondeurPcm::FlottantIeee32, 6),
    ];

    for (profondeur, trames) in attendu {
        let s = spec(profondeur, 2);
        let bloc = s.bloc(&octets);
        assert_eq!(
            bloc.trames(),
            trames,
            "48 octets stéréo en {profondeur:?} ne font plus {trames} trames : \
             la profondeur ne commande plus le découpage en trames"
        );
        assert_eq!(
            bloc.spec(),
            s,
            "un bloc a pris une étiquette qui n'est pas celle qui l'a produit"
        );
    }
}

/// Le sentinelle `0` du flottant et les 32 bits entiers font la MÊME largeur et
/// ne se décodent pas du tout pareil.
///
/// C'est le piège que le `u16` laissait ouvert : `bit_depth == 32` était FAUX
/// pour du flottant 32 bits, et `bit_depth / 8` lui rendait zéro octet. Les
/// deux variantes doivent s'accorder sur la largeur et rester **distinctes**.
#[test]
fn le_flottant_32_et_l_entier_32_font_la_meme_largeur_sans_etre_le_meme_format() {
    let flottant = spec(ProfondeurPcm::FlottantIeee32, 2);
    let entier = spec(ProfondeurPcm::Entier32, 2);

    assert_eq!(
        flottant.octets_par_trame(),
        entier.octets_par_trame(),
        "les deux formats 32 bits doivent découper les trames pareil"
    );
    assert_ne!(
        flottant, entier,
        "le flottant IEEE et l'entier 32 bits sont devenus indiscernables : \
         c'est exactement la confusion que le sentinelle `0` fabriquait, et \
         elle rend du bruit à pleine échelle vers un amplificateur"
    );
    assert_eq!(
        flottant.profondeur().bits_declares(),
        0,
        "le flottant se déclare `0` dans un en-tête WAV de ce dépôt"
    );
    assert_eq!(entier.profondeur().bits_declares(), 32);
}

/// La partition ne perd ni ne duplique un octet, pour **toute** longueur.
///
/// Le reste non aligné est la trame coupée en deux par la frontière du tampon
/// réseau. Le jeter décale tout le flux qui suit (#3849). Il doit donc être un
/// suffixe EXACT, plus court qu'une trame, et le préfixe doit faire un nombre
/// entier de trames.
#[test]
fn la_partition_en_trames_ne_perd_ni_ne_duplique_un_octet() {
    let octets: Vec<u8> = (0..=255u8).cycle().take(200).collect();

    for profondeur in JEU_FERME {
        for canaux in [1u16, 2, 6, 8] {
            let s = spec(profondeur, canaux);
            let trame = s.octets_par_trame();
            for longueur in 0..=120usize {
                let bloc = s.bloc(&octets[..longueur]);
                let alignes = bloc.octets_alignes();
                let reste = bloc.reste_non_aligne();

                assert_eq!(
                    bloc.octets(),
                    &octets[..longueur],
                    "{profondeur:?}/{canaux}ch, {longueur} octets : le bloc ne rend plus les octets qu'on lui a confiés"
                );
                assert_eq!(
                    alignes.len() + reste.len(),
                    longueur,
                    "{profondeur:?}/{canaux}ch, {longueur} octets : la partition \
                     perd ou duplique des octets"
                );
                assert_eq!(
                    alignes,
                    &octets[..alignes.len()],
                    "{profondeur:?}/{canaux}ch, {longueur} octets : la partie \
                     alignée n'est pas le PRÉFIXE du bloc"
                );
                assert_eq!(
                    reste,
                    &octets[alignes.len()..longueur],
                    "{profondeur:?}/{canaux}ch, {longueur} octets : le reste \
                     n'est pas le SUFFIXE du bloc"
                );
                assert_eq!(
                    alignes.len() % trame,
                    0,
                    "{profondeur:?}/{canaux}ch, {longueur} octets : la partie \
                     alignée ne fait pas un nombre entier de trames"
                );
                assert!(
                    reste.len() < trame,
                    "{profondeur:?}/{canaux}ch, {longueur} octets : le reste \
                     contient une trame complète — elle aurait dû être décodée"
                );
                assert_eq!(
                    alignes.len(),
                    bloc.trames() * trame,
                    "{profondeur:?}/{canaux}ch, {longueur} octets : le compte de \
                     trames et la longueur alignée se contredisent"
                );
            }
        }
    }
}

/// Réétiqueter le préfixe aligné avec sa PROPRE étiquette ne change rien.
///
/// Ce que ce témoin attrape et que la partition seule ne voit pas : un
/// `octets_alignes` qui arrondirait à la trame SUPÉRIEURE, ou qui compterait
/// les trames avec une largeur figée. Repasser le préfixe dans le même format
/// doit être un point fixe — même compte de trames, plus aucun reste.
#[test]
fn le_prefixe_aligne_est_un_point_fixe_de_son_propre_format() {
    let octets: Vec<u8> = (0..=255u8).cycle().take(200).collect();

    for profondeur in JEU_FERME {
        for canaux in [1u16, 2, 6] {
            let s = spec(profondeur, canaux);
            for longueur in 0..=120usize {
                let bloc = s.bloc(&octets[..longueur]);
                let rebloc = s.bloc(bloc.octets_alignes());
                assert!(
                    rebloc.reste_non_aligne().is_empty(),
                    "{profondeur:?}/{canaux}ch, {longueur} octets : le préfixe \
                     dit aligné laisse encore un reste"
                );
                assert_eq!(
                    rebloc.trames(),
                    bloc.trames(),
                    "{profondeur:?}/{canaux}ch, {longueur} octets : le compte de \
                     trames change quand on repasse le préfixe dans le MÊME format"
                );
            }
        }
    }
}

/// Une profondeur hors du jeu fermé est **refusée**, jamais approchée.
///
/// Le repli silencieux était le vrai danger : `pcm_bytes_to_f32` retombe sur la
/// lecture 16 bits et consomme deux octets là où l'appelant en a compté
/// `bit_depth / 8`. Chaque trame part alors au mauvais décalage — du bruit
/// blanc avec la musique derrière.
#[test]
fn une_profondeur_hors_du_jeu_ferme_est_refusee_pas_approchee() {
    for bits in [1u16, 4, 8, 12, 20, 31, 33, 48, 64, 65_535] {
        assert_eq!(
            ProfondeurPcm::depuis_bits_declares(bits),
            None,
            "{bits} bits a été accepté : une profondeur que le chemin de lecture \
             n'énumère pas ne doit pas se glisser sous un repli"
        );
        assert_eq!(
            AudioSpec::depuis_entete(44_100, bits, 2),
            None,
            "{bits} bits a été accepté par la porte d'entrée du format"
        );
    }

    // L'interversion RÉELLE : `depuis_entete` prend deux `u16` voisins, et un
    // flux stéréo qui les échange demande « 2 bits ». Le jeu fermé le refuse au
    // lieu de le lire de travers — c'est le filet sous la seule signature de
    // cette caisse où le compilateur ne peut plus rien.
    for canaux in [1u16, 2, 6, 8] {
        assert_eq!(
            AudioSpec::depuis_entete(44_100, canaux, 16),
            None,
            "profondeur et canaux intervertis ({canaux} canaux lus comme une profondeur) : le format doit être REFUSÉ, pas lu de travers"
        );
    }

    for profondeur in JEU_FERME {
        let bits = profondeur.bits_declares();
        assert_eq!(
            ProfondeurPcm::depuis_bits_declares(bits),
            Some(profondeur),
            "{profondeur:?} ne fait plus l'aller-retour par ses bits déclarés"
        );
    }
}

/// Zéro canal est refusé parce qu'il serait un **diviseur nul**.
///
/// `octets_par_trame` vaudrait `0`, et le calcul d'alignement — une division
/// par ce nombre — abattrait le fil de lecture. L'invariant est posé une fois,
/// au constructeur, et c'est lui qui rend `trames()` total.
#[test]
fn zero_canal_est_refuse_parce_qu_il_serait_un_diviseur_nul() {
    for profondeur in JEU_FERME {
        assert_eq!(
            AudioSpec::nouvelle(44_100, profondeur, 0),
            None,
            "{profondeur:?} : zéro canal a été accepté — le calcul d'alignement \
             divisera par zéro"
        );
        assert_eq!(
            AudioSpec::depuis_entete(44_100, profondeur.bits_declares(), 0),
            None,
            "{profondeur:?} : zéro canal a été accepté par la porte d'entrée"
        );
        for canaux in [1u16, 2, 6, 8, 64, u16::MAX] {
            let s = spec(profondeur, canaux);
            assert!(
                s.octets_par_trame() >= 1,
                "{profondeur:?}/{canaux}ch : une trame sans octet"
            );
            assert_eq!(
                s.octets_par_trame(),
                profondeur.octets() * canaux as usize,
                "{profondeur:?}/{canaux}ch : les octets par trame ne suivent plus \
                 la profondeur ET les canaux"
            );
        }
    }
}

/// Les trois étiquettes voyagent ensemble, et se relisent telles qu'on les a
/// posées.
///
/// Sans ce témoin, un constructeur qui rangerait la cadence dans les canaux
/// passerait : les autres témoins ne lisent jamais la cadence.
#[test]
fn les_trois_etiquettes_se_relisent_telles_qu_on_les_a_posees() {
    for cadence in [44_100u32, 48_000, 96_000, 352_800] {
        for profondeur in JEU_FERME {
            for canaux in [1u16, 2, 8] {
                let s = AudioSpec::nouvelle(cadence, profondeur, canaux)
                    .expect("canaux non nuls, cadence quelconque");
                assert_eq!(s.cadence(), cadence, "la cadence ne se relit plus");
                assert_eq!(s.profondeur(), profondeur, "la profondeur ne se relit plus");
                assert_eq!(s.canaux(), canaux, "les canaux ne se relisent plus");
                assert_eq!(
                    AudioSpec::depuis_entete(cadence, profondeur.bits_declares(), canaux),
                    Some(s),
                    "les deux constructeurs ne rendent plus le même format"
                );
            }
        }
    }
}
