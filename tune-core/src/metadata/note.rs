//! #6123 — la NOTE d'une piste, lue dans ses balises et ramenée sur 0 à 5.
//!
//! Signalé sur forum-hifi.fr (PhilActiveAudio75, venant de Roon) : ses
//! fichiers portent une note par piste, Tune ne la lisait pas. Aucune balise
//! de note n'était lue au scan, et `tracks` n'avait pas de colonne pour la
//! recevoir (migration SQLite 124 / PG 088 : `tracks.rating`).
//!
//! # L'échelle retenue : 0 à 5, entier
//!
//! Aucun format n'a d'échelle normalisée ; chaque logiciel a la sienne. Tune
//! ramène tout sur des étoiles entières, de 0 à 5, d'après la table
//! ci-dessous — la seule règle de conversion du dépôt.
//!
//! | balise | écrite par | valeur brute | note Tune |
//! |---|---|---|---|
//! | Vorbis `RATING` (FLAC, Ogg, Opus) | foobar2000, Mp3tag, Roon | 0 à 5 | telle quelle |
//! | Vorbis `RATING`, MP4 `rate`, ID3 `TXXX:RATING` | MusicBee, MediaMonkey | 6 à 100 | valeur ÷ 20, arrondie |
//! | ID3 `POPM` (MP3, DSF, AIFF, WAV) | Windows Media Player, MusicBee, Picard | 1 à 255 | table WMP (ci-dessous) |
//! | `FMPS_RATING` (Vorbis), `TXXX:FMPS_Rating` (ID3) | Quod Libet, Amarok, Clementine | 0.0 à 1.0 | valeur × 5, arrondie |
//!
//! Table WMP du `POPM` : 1 → 1, 2 à 64 → 2, 65 à 128 → 3, 129 à 196 → 4,
//! 197 à 255 → 5 — les paliers que WMP et MusicBee écrivent (1, 64, 128,
//! 196, 255) et que lofty applique à tout `POPM` dont l'adresse n'est pas
//! celle de Picard. Picard (`users@musicbrainz.org`) écrit 51 par étoile :
//! 1 à 51 → 1, 52 à 102 → 2, 103 à 153 → 3, 154 à 204 → 4, 205 à 255 → 5.
//!
//! Une valeur de 0 à 5 avec décimale (« 3.5 », ou « 3,5 ») s'arrondit à
//! l'étoile la plus proche, demi-étoile vers le haut.
//!
//! # Ce qui n'est PAS une note
//!
//! * `POPM` à **0** : la norme ID3 le définit comme « note inconnue ». Les
//!   lecteurs l'écrivent pour ne garder que le compteur d'écoutes : ce n'est
//!   pas « zéro étoile », la piste reste sans note (`None`).
//! * une valeur négative, non numérique, ou au-delà de 100 (hors `POPM`) :
//!   aucune échelle connue ne la produit, on ne devine pas. `None`.
//!
//! En revanche un `RATING=0` explicite est gardé : 0 étoile est une note.
//!
//! # Ordre de lecture
//!
//! La première balise qui donne une note gagne : `POPM` / `RATING` / `rate`
//! (ce que lofty range sous `ItemKey::Popularimeter`), puis `TXXX:RATING`,
//! puis `FMPS_RATING`. Voir `note_de_la_balise` dans `metadata/mod.rs`.

/// La note la plus haute.
pub const NOTE_MAX: u8 = 5;

/// Une valeur lue en TEXTE — Vorbis `RATING`, MP4 `rate`, ID3 `TXXX:RATING`
/// —, sur l'échelle 0–5 ou 0–100. Voir la table du module.
pub fn note_depuis_valeur(brut: &str) -> Option<u8> {
    let v = nombre(brut)?;
    if v <= f64::from(NOTE_MAX) {
        Some(arrondi(v))
    } else if v <= 100.0 {
        Some(arrondi(v / 20.0))
    } else {
        None
    }
}

/// L'octet d'un `POPM` ID3 (0–255), table WMP. 0 = note inconnue.
pub fn note_depuis_popm(octet: u8) -> Option<u8> {
    Some(match octet {
        0 => return None,
        1 => 1,
        2..=64 => 2,
        65..=128 => 3,
        129..=196 => 4,
        _ => 5,
    })
}

/// L'adresse sous laquelle MusicBrainz Picard écrit son `POPM`.
pub const ADRESSE_PICARD: &str = "users@musicbrainz.org";

/// L'octet d'un `POPM` selon l'adresse qui l'a écrit : Picard écrit 51 par
/// étoile (51, 102, 153, 204, 255), tous les autres la table WMP. Mêmes
/// paliers que lofty, pour qu'un `POPM` relu brut donne la même note que
/// celui que lofty a converti.
pub fn note_depuis_popm_de(adresse: &str, octet: u8) -> Option<u8> {
    if adresse != ADRESSE_PICARD {
        return note_depuis_popm(octet);
    }
    Some(match octet {
        0 => return None,
        1..=51 => 1,
        52..=102 => 2,
        103..=153 => 3,
        154..=204 => 4,
        _ => 5,
    })
}

/// Une valeur FMPS (`FMPS_RATING`, `TXXX:FMPS_Rating`) : 0.0 à 1.0, × 5.
pub fn note_depuis_fmps(brut: &str) -> Option<u8> {
    let v = nombre(brut)?;
    (v <= 1.0).then(|| arrondi(v * f64::from(NOTE_MAX)))
}

/// Le texte qu'écrit lofty pour un `POPM` (ou un `RATING:adresse` Vorbis)
/// qu'il a déjà converti : `adresse|étoiles|compteur`. Les étoiles y sont
/// déjà sur 1–5 (lofty ne produit pas de 0 : voir [`note_depuis_popm`] pour
/// ce que cache son « 1 »).
pub fn etoiles_du_popularimetre(texte: &str) -> Option<u8> {
    let mut parties = texte.splitn(3, '|');
    let _adresse = parties.next()?;
    let etoiles = parties.next()?.trim().parse::<u8>().ok()?;
    (1..=NOTE_MAX).contains(&etoiles).then_some(etoiles)
}

fn nombre(brut: &str) -> Option<f64> {
    let v = brut.trim().replace(',', ".").parse::<f64>().ok()?;
    (v.is_finite() && v >= 0.0).then_some(v)
}

/// Arrondi à l'étoile la plus proche, demi vers le haut, borné à 0–5.
fn arrondi(v: f64) -> u8 {
    (v + 0.5).floor().clamp(0.0, f64::from(NOTE_MAX)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn l_echelle_de_zero_a_cinq_est_gardee_telle_quelle() {
        for n in 0..=5u8 {
            assert_eq!(note_depuis_valeur(&n.to_string()), Some(n));
        }
        assert_eq!(note_depuis_valeur(" 4 "), Some(4));
        assert_eq!(note_depuis_valeur("3.5"), Some(4));
        assert_eq!(note_depuis_valeur("3,4"), Some(3));
    }

    #[test]
    fn l_echelle_sur_cent_se_divise_par_vingt() {
        assert_eq!(note_depuis_valeur("20"), Some(1));
        assert_eq!(note_depuis_valeur("40"), Some(2));
        assert_eq!(note_depuis_valeur("60"), Some(3));
        assert_eq!(note_depuis_valeur("80"), Some(4));
        assert_eq!(note_depuis_valeur("100"), Some(5));
        // MediaMonkey écrit des demi-étoiles : 90 = 4,5 → 5.
        assert_eq!(note_depuis_valeur("90"), Some(5));
        assert_eq!(note_depuis_valeur("6"), Some(0));
    }

    #[test]
    fn ce_qui_n_est_pas_une_note_ne_devient_pas_une_note() {
        for brut in ["", "abc", "-1", "101", "255", "NaN", "inf"] {
            assert_eq!(note_depuis_valeur(brut), None, "{brut:?}");
        }
    }

    #[test]
    fn le_popm_suit_la_table_wmp_et_zero_reste_inconnu() {
        assert_eq!(note_depuis_popm(0), None);
        assert_eq!(note_depuis_popm(1), Some(1));
        assert_eq!(note_depuis_popm(64), Some(2));
        assert_eq!(note_depuis_popm(128), Some(3));
        assert_eq!(note_depuis_popm(196), Some(4));
        assert_eq!(note_depuis_popm(255), Some(5));
        assert_eq!(note_depuis_popm(65), Some(3));
        assert_eq!(note_depuis_popm(197), Some(5));
    }

    #[test]
    fn le_popm_de_picard_suit_sa_propre_table() {
        assert_eq!(note_depuis_popm_de(ADRESSE_PICARD, 51), Some(1));
        assert_eq!(note_depuis_popm_de(ADRESSE_PICARD, 102), Some(2));
        assert_eq!(note_depuis_popm_de(ADRESSE_PICARD, 204), Some(4));
        assert_eq!(note_depuis_popm_de(ADRESSE_PICARD, 0), None);
        // La même valeur sous une autre adresse suit la table WMP.
        assert_eq!(note_depuis_popm_de("MusicBee", 51), Some(2));
    }

    #[test]
    fn le_fmps_se_multiplie_par_cinq() {
        assert_eq!(note_depuis_fmps("0"), Some(0));
        assert_eq!(note_depuis_fmps("0.2"), Some(1));
        assert_eq!(note_depuis_fmps("0.6"), Some(3));
        assert_eq!(note_depuis_fmps("0.9"), Some(5));
        assert_eq!(note_depuis_fmps("1.0"), Some(5));
        assert_eq!(note_depuis_fmps("1.5"), None);
        assert_eq!(note_depuis_fmps("x"), None);
    }

    #[test]
    fn le_texte_de_lofty_rend_ses_etoiles() {
        assert_eq!(
            etoiles_du_popularimetre("Windows Media Player 9 Series|4|12"),
            Some(4)
        );
        assert_eq!(etoiles_du_popularimetre("|5|0"), Some(5));
        assert_eq!(etoiles_du_popularimetre("4"), None);
        assert_eq!(etoiles_du_popularimetre("x|9|0"), None);
    }
}
