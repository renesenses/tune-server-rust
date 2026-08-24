//! Les préréglages d'égaliseur, définis UNE FOIS, côté serveur.
//!
//! ## Le défaut que ce module répare
//!
//! `POST /zones/{id}/eq` accepte un champ `preset`. Il ne l'a jamais appliqué :
//! il le **renvoyait** simplement dans sa réponse.
//!
//! ```text
//! "preset": body.preset.unwrap_or_else(|| "custom".into())
//! ```
//!
//! L'écran « En cours de lecture » envoie exactement cela — un nom, sans
//! bandes. Le serveur répondait donc `200 OK` avec `"preset": "rock"`, sans
//! avoir rien changé au son. L'utilisateur choisit « Rock », l'interface le
//! confirme, et rien ne bouge.
//!
//! ## Pourquoi les courbes vivent ICI
//!
//! Elles n'existaient que dans le client web, en dur. Les porter côté serveur
//! règle le défaut **et** supprime une triplication à venir : iPadOS et Flutter
//! auraient chacun recopié la même table, et elles auraient divergé — c'est
//! déjà arrivé à la grille de fréquences.
//!
//! Les valeurs sont celles du client web, reprises à l'identique : ce sont
//! elles que les utilisateurs connaissent, et ce correctif ne doit pas changer
//! le son d'un préréglage déjà en usage.

use super::eq::EqBandSpec;

/// La grille ISO à 10 bandes (une octave), celle des préréglages.
pub const GRILLE_10: [f64; 10] = [
    31.0, 63.0, 125.0, 250.0, 500.0, 1000.0, 2000.0, 4000.0, 8000.0, 16000.0,
];

/// Le Q de cette grille. Une octave — le même que celui du client web.
const Q_GRILLE_10: f64 = 1.0;

/// Les gains de chaque préréglage, sur [`GRILLE_10`].
const PREREGLAGES: [(&str, [f64; 10]); 7] = [
    ("flat", [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    (
        "bass_boost",
        [8.0, 6.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
    ),
    (
        "treble_boost",
        [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 3.0, 5.0, 7.0, 8.0],
    ),
    (
        "loudness",
        [6.0, 4.0, 0.0, -2.0, -1.0, 0.0, 2.0, 4.0, 5.0, 6.0],
    ),
    ("rock", [5.0, 3.0, 0.0, -2.0, -1.0, 2.0, 4.0, 5.0, 5.0, 4.0]),
    ("jazz", [3.0, 2.0, 0.0, 2.0, -1.0, -1.0, 0.0, 2.0, 4.0, 5.0]),
    (
        "classical",
        [0.0, 0.0, 0.0, 0.0, 0.0, 0.0, -2.0, -3.0, -2.0, -1.0],
    ),
];

/// Les bandes d'un préréglage nommé, ou `None` si le nom est inconnu.
///
/// `None` compte : l'appelant doit refuser un nom qu'il ne connaît pas plutôt
/// que de répondre `200` sans rien faire. C'est précisément ce silence qui a
/// rendu ces préréglages inertes pendant des mois.
///
/// « custom » n'est pas un préréglage : c'est le nom que la réponse porte quand
/// l'utilisateur a réglé ses bandes à la main. Il rend donc `None` lui aussi.
pub fn bandes(nom: &str) -> Option<Vec<EqBandSpec>> {
    let gains = PREREGLAGES
        .iter()
        .find(|(cle, _)| *cle == nom)
        .map(|(_, gains)| gains)?;

    Some(
        GRILLE_10
            .iter()
            .zip(gains.iter())
            .map(|(freq, gain)| EqBandSpec {
                freq: *freq,
                gain: *gain,
                q: Q_GRILLE_10,
                // Un préréglage graphique n'agit sur AUCUN canal en
                // particulier : le champ reste absent, donc « les deux ».
                ..Default::default()
            })
            .collect(),
    )
}

/// Les noms connus, dans l'ordre où les clients les affichent.
pub fn noms() -> Vec<&'static str> {
    PREREGLAGES.iter().map(|(nom, _)| *nom).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn un_prereglage_connu_rend_dix_bandes_sur_la_grille() {
        let b = bandes("rock").expect("« rock » est un prereglage connu");
        assert_eq!(b.len(), 10);
        assert_eq!(
            b.iter().map(|x| x.freq).collect::<Vec<_>>(),
            GRILLE_10.to_vec()
        );
    }

    /// Les gains sont ceux que les utilisateurs connaissent deja.
    ///
    /// Ce correctif rend les prereglages AGISSANTS ; il ne doit pas en changer
    /// le son. Les valeurs viennent du client web, ou elles vivaient en dur.
    #[test]
    fn les_gains_sont_ceux_du_client_web() {
        let rock = bandes("rock").unwrap();
        assert_eq!(
            rock.iter().map(|b| b.gain).collect::<Vec<_>>(),
            vec![5.0, 3.0, 0.0, -2.0, -1.0, 2.0, 4.0, 5.0, 5.0, 4.0]
        );
        let bass = bandes("bass_boost").unwrap();
        assert_eq!(bass[0].gain, 8.0);
        assert_eq!(bass[9].gain, 0.0);
    }

    #[test]
    fn flat_est_plat() {
        assert!(bandes("flat").unwrap().iter().all(|b| b.gain == 0.0));
    }

    /// Le coeur du defaut : un nom inconnu doit se VOIR.
    #[test]
    fn un_nom_inconnu_rend_none() {
        assert!(bandes("rockk").is_none());
        assert!(bandes("").is_none());
        assert!(bandes("ROCK").is_none(), "la casse n'est pas normalisee");
    }

    /// « custom » n'est pas un prereglage : c'est l'absence de prereglage.
    #[test]
    fn custom_nest_pas_un_prereglage() {
        assert!(bandes("custom").is_none());
    }

    /// Un prereglage graphique ne vise aucun canal : le champ doit rester
    /// ABSENT, sinon il ferait taire l'autre cote.
    #[test]
    fn aucun_prereglage_ne_vise_un_canal() {
        for nom in noms() {
            for b in bandes(nom).unwrap() {
                assert_eq!(b.channel, None, "« {nom} » vise un canal");
            }
        }
    }

    #[test]
    fn les_sept_noms_sont_tous_resolubles() {
        assert_eq!(noms().len(), 7);
        for nom in noms() {
            assert!(bandes(nom).is_some(), "« {nom} » ne se resout pas");
        }
    }
}
