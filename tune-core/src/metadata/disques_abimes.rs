//! Réparer le numéro de disque des coffrets ripés en `CD1/`, `CD2/` — #4471.
//!
//! # Pourquoi une réparation rétroactive
//!
//! Corriger l'arbitrage du scan ([`super::disque_arbitre`]) ne suffit pas : un
//! rescan relit les MÊMES tags, qui n'ont pas changé, et ne visite que les
//! fichiers dont la date a bougé. Les bibliothèques déjà constituées gardent
//! donc leur dommage indéfiniment. Il faut aller le chercher.
//!
//! # La signature du dommage
//!
//! Deux pistes du même album qui partagent disque ET numéro de piste. C'est
//! exactement ce que produit un coffret dont tous les fichiers se déclarent
//! « disque 1 » :
//!
//! ```text
//! n°1 disc=1 [CD 1] Rock And Roll
//! n°1 disc=1 [CD 2] No Quarter
//! ```
//!
//! 🔴 On ne part PAS de « tous les albums à plusieurs dossiers de disque ».
//! Un coffret correctement tagué en porte aussi, et il n'a rien à réparer —
//! le toucher serait du bruit, et le moindre risque pris sur un album sain est
//! un risque de trop. On part du dommage, pas de la forme.
//!
//! # Ce qu'on ne répare pas
//!
//! Une collision que le dossier n'explique pas. Si les chemins ne portent
//! aucun numéro de disque, ou s'ils donnent le même pour les deux pistes, la
//! cause est ailleurs — deux fois le même fichier, un tag de piste faux — et
//! cette réparation-ci n'a rien à en dire. Elle laisse l'album tel quel plutôt
//! que d'inventer une numérotation.

use std::path::Path;

/// Une piste telle que la réparation a besoin de la voir.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PisteAExaminer {
    pub id: i64,
    pub album_id: i64,
    pub file_path: String,
    pub disc_number: Option<u32>,
    pub track_number: Option<u32>,
}

/// Un changement proposé, jamais appliqué sans qu'on le demande.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Correction {
    pub track_id: i64,
    pub album_id: i64,
    pub avant: Option<u32>,
    pub apres: u32,
}

/// Le disque qu'annonce le DOSSIER contenant ce fichier, s'il en annonce un.
pub fn disque_du_dossier(chemin: &str) -> Option<u32> {
    Path::new(chemin)
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
        .and_then(super::numero_de_disque)
}

/// Le couple qui doit rester unique dans un album : disque et numéro de piste.
/// Un disque absent vaut 1 — c'est ce que fait l'affichage, et c'est ce qui
/// crée la collision.
fn couple(p: &PisteAExaminer) -> (u32, Option<u32>) {
    (p.disc_number.unwrap_or(1), p.track_number)
}

/// Cet album est-il ABÎMÉ — deux pistes au même disque et au même numéro ?
///
/// Une piste sans numéro n'entre pas dans le compte : deux pistes sans numéro
/// ne se marchent pas dessus, elles ne sont simplement pas numérotées.
pub fn album_abime(pistes: &[PisteAExaminer]) -> bool {
    let mut vus = std::collections::HashSet::new();
    for p in pistes {
        let (d, n) = couple(p);
        let Some(n) = n else { continue };
        if !vus.insert((d, n)) {
            return true;
        }
    }
    false
}

/// Ce qu'il faut écrire pour réparer cet album, ou rien.
///
/// Rend une liste VIDE quand l'album n'est pas abîmé, quand le dossier ne dit
/// rien, ou quand suivre le dossier ne réglerait pas la collision : on ne
/// déplace pas un problème pour le plaisir d'écrire.
pub fn corrections_pour_album(pistes: &[PisteAExaminer]) -> Vec<Correction> {
    if !album_abime(pistes) {
        return Vec::new();
    }
    let mut proposees: Vec<Correction> = Vec::new();
    let mut apres: Vec<PisteAExaminer> = Vec::with_capacity(pistes.len());
    for p in pistes {
        match disque_du_dossier(&p.file_path) {
            Some(d) if Some(d) != p.disc_number => {
                proposees.push(Correction {
                    track_id: p.id,
                    album_id: p.album_id,
                    avant: p.disc_number,
                    apres: d,
                });
                apres.push(PisteAExaminer {
                    disc_number: Some(d),
                    ..p.clone()
                });
            }
            _ => apres.push(p.clone()),
        }
    }
    // 🔴 La contre-épreuve, AVANT d'écrire : si le dossier ne lève pas la
    // collision, il ne sait pas mieux que le tag et on ne touche à rien.
    if proposees.is_empty() || album_abime(&apres) {
        return Vec::new();
    }
    proposees
}

/// Les corrections de TOUS les albums d'un lot de pistes, groupées par album.
pub fn corrections(pistes: &[PisteAExaminer]) -> Vec<Correction> {
    let mut par_album: std::collections::BTreeMap<i64, Vec<PisteAExaminer>> = Default::default();
    for p in pistes {
        par_album.entry(p.album_id).or_default().push(p.clone());
    }
    par_album
        .values()
        .flat_map(|v| corrections_pour_album(v))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn piste(id: i64, chemin: &str, disc: Option<u32>, n: u32) -> PisteAExaminer {
        PisteAExaminer {
            id,
            album_id: 991,
            file_path: chemin.to_string(),
            disc_number: disc,
            track_number: Some(n),
        }
    }

    /// Les chemins RÉELS de l'album #991 sur le .18, relevés le 19/09/2026.
    fn song_remains_the_same() -> Vec<PisteAExaminer> {
        let base = "/data/music/NEW_FLAC/POP-ROCK/L/Led Zeppelin/1976-The Song Remains The Same";
        vec![
            piste(1, &format!("{base}/CD 1/01 Rock And Roll.flac"), Some(1), 1),
            piste(
                2,
                &format!("{base}/CD 1/02 Celebration Day.flac"),
                Some(1),
                2,
            ),
            piste(3, &format!("{base}/CD 2/01 No Quarter.flac"), Some(1), 1),
            piste(
                4,
                &format!("{base}/CD 2/02 Stairway To Heaven.flac"),
                Some(1),
                2,
            ),
        ]
    }

    #[test]
    fn le_dossier_annonce_son_disque() {
        assert_eq!(disque_du_dossier("/m/Album/CD2/01.flac"), Some(2));
        assert_eq!(disque_du_dossier("/m/Album/CD 1/01.flac"), Some(1));
        assert_eq!(disque_du_dossier("/m/Album/Disc 3/01.flac"), Some(3));
        assert_eq!(disque_du_dossier("/m/Album/Disque 2/01.flac"), Some(2));
        // Un dossier ordinaire ne dit rien, et c'est le cas général.
        assert_eq!(disque_du_dossier("/m/Artiste/Album/01.flac"), None);
        assert_eq!(disque_du_dossier("01.flac"), None);
    }

    #[test]
    fn le_dommage_se_reconnait_a_la_collision() {
        assert!(album_abime(&song_remains_the_same()));
        // Le même coffret, correctement tagué : rien à signaler.
        let sain = vec![
            piste(1, "/m/A/CD 1/01.flac", Some(1), 1),
            piste(2, "/m/A/CD 2/01.flac", Some(2), 1),
        ];
        assert!(!album_abime(&sain));
    }

    #[test]
    fn deux_pistes_sans_numero_ne_se_marchent_pas_dessus() {
        let sans = vec![
            PisteAExaminer {
                track_number: None,
                ..piste(1, "/m/A/01.flac", Some(1), 1)
            },
            PisteAExaminer {
                track_number: None,
                ..piste(2, "/m/A/02.flac", Some(1), 1)
            },
        ];
        assert!(!album_abime(&sans));
    }

    #[test]
    fn le_cas_mesure_de_led_zeppelin_est_repare() {
        let c = corrections_pour_album(&song_remains_the_same());
        // Seules les pistes du CD 2 bougent — celles du CD 1 étaient déjà justes.
        assert_eq!(c.len(), 2, "{c:?}");
        assert!(c.iter().all(|x| x.apres == 2 && x.avant == Some(1)));
        assert_eq!(c.iter().map(|x| x.track_id).collect::<Vec<_>>(), vec![3, 4]);
    }

    #[test]
    fn un_album_sain_n_est_jamais_touche() {
        let sain = vec![
            piste(1, "/m/A/CD 1/01.flac", Some(1), 1),
            piste(2, "/m/A/CD 2/01.flac", Some(2), 1),
        ];
        assert!(corrections_pour_album(&sain).is_empty());
    }

    /// 🔴 La contre-épreuve du module : une collision que le dossier
    /// n'explique pas ne doit RIEN produire.
    #[test]
    fn une_collision_sans_dossier_de_disque_est_laissee_telle_quelle() {
        let deux_fois = vec![
            piste(1, "/m/Artiste/Album/01 Titre.flac", Some(1), 1),
            piste(2, "/m/Artiste/Album/01 Titre (copie).flac", Some(1), 1),
        ];
        assert!(album_abime(&deux_fois));
        assert!(
            corrections_pour_album(&deux_fois).is_empty(),
            "sans dossier de disque, la cause est ailleurs : on n'invente pas une numérotation"
        );
    }

    /// 🔴 Et si suivre le dossier ne LEVAIT PAS la collision, on ne touche à
    /// rien non plus : on ne déplace pas un problème pour le plaisir d'écrire.
    #[test]
    fn un_dossier_qui_ne_leve_pas_la_collision_ne_produit_rien() {
        // Les deux pistes sont dans le MÊME dossier de disque, et se marchent
        // déjà dessus : le dossier dit « 2 » pour les deux.
        let bancal = vec![
            piste(1, "/m/A/CD 2/01 X.flac", Some(1), 1),
            piste(2, "/m/A/CD 2/01 Y.flac", Some(1), 1),
        ];
        assert!(album_abime(&bancal));
        assert!(corrections_pour_album(&bancal).is_empty());
    }

    #[test]
    fn le_lot_groupe_par_album_et_ne_melange_pas() {
        let mut lot = song_remains_the_same();
        lot.extend(vec![
            PisteAExaminer {
                album_id: 844,
                ..piste(10, "/m/P/CD1/01.flac", Some(1), 1)
            },
            PisteAExaminer {
                album_id: 844,
                ..piste(11, "/m/P/CD2/01.flac", Some(1), 1)
            },
        ]);
        let c = corrections(&lot);
        assert_eq!(c.len(), 3, "{c:?}");
        assert_eq!(c.iter().filter(|x| x.album_id == 991).count(), 2);
        assert_eq!(c.iter().filter(|x| x.album_id == 844).count(), 1);
    }
}
