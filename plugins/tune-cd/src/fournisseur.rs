//! Le fournisseur PCM que le greffon inscrit auprès de l'hôte sous la source
//! `cd`. L'orchestrateur l'appelle pour chaque ligne de file « cd » : première
//! lecture, piste suivante, pré-armement gapless, avance dans la piste.
//!
//! Le `source_id` d'une ligne est `<disc_id>/<piste>`. L'identifiant de disque
//! y figure pour qu'une file construite sur un disque ne joue jamais les
//! pistes d'un AUTRE disque inséré entre-temps : elle échoue en le disant.

use std::sync::Arc;

use tune_core::source_pcm::{FluxPcm, FormatPcm, FournisseurPcm};

use crate::discid::disc_id;
use crate::flux::FluxPiste;
use crate::lecteur::LecteurDisque;
use crate::toc::{OCTETS_PAR_SECTEUR, duree_ms_de_secteurs, secteur_de_position};

/// Le nom de la source, dans la file et dans `now_playing.source`.
pub const SOURCE: &str = "cd";

pub fn source_id(disc: &str, piste: u8) -> String {
    format!("{disc}/{piste}")
}

pub fn lire_source_id(source_id: &str) -> Option<(&str, u8)> {
    let (disc, piste) = source_id.rsplit_once('/')?;
    Some((disc, piste.parse().ok()?))
}

/// Où commence et finit la lecture d'une piste depuis une position : la
/// plage de secteurs `[debut, fin)` en LBA.
pub fn plage(debut_piste: u32, secteurs: u32, depuis_ms: u64) -> (u32, u32) {
    let saut = secteur_de_position(depuis_ms).min(secteurs as u64) as u32;
    (debut_piste + saut, debut_piste + secteurs)
}

pub struct FournisseurCd {
    pub lecteur: Arc<dyn LecteurDisque>,
}

impl FournisseurPcm for FournisseurCd {
    fn ouvrir(&self, source_id: &str, depuis_ms: u64) -> Result<FluxPcm, String> {
        let (disc_demande, numero) = lire_source_id(source_id)
            .ok_or_else(|| format!("piste de CD illisible : {source_id}"))?;
        let toc = self.lecteur.lire_toc().map_err(|e| e.to_string())?;
        if disc_id(&toc) != disc_demande {
            return Err("le disque a changé depuis la mise en file".into());
        }
        let piste = toc
            .piste(numero)
            .filter(|p| p.audio)
            .ok_or_else(|| format!("la piste {numero} n'est pas une piste audio de ce disque"))?;
        let secteurs = toc.secteurs(numero).unwrap_or(0);
        let (debut, fin) = plage(piste.debut, secteurs, depuis_ms);
        tracing::info!(
            piste = numero,
            depuis_ms,
            premier_secteur = debut,
            fin_exclue = fin,
            "cd_piste_ouverte"
        );
        Ok(FluxPcm {
            format: FormatPcm::CD,
            octets: (fin - debut) as u64 * OCTETS_PAR_SECTEUR as u64,
            duree_ms: duree_ms_de_secteurs(secteurs),
            lecteur: Box::new(FluxPiste::new(self.lecteur.clone(), debut, fin)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discid::tests::{ATTENDU, toc_du_vecteur};
    use crate::simule::{LecteurSimule, contenu_des_secteurs};
    use std::io::Read;

    fn fournisseur() -> (FournisseurCd, Arc<LecteurSimule>) {
        let l = Arc::new(LecteurSimule::new(toc_du_vecteur()));
        (FournisseurCd { lecteur: l.clone() }, l)
    }

    fn lire(f: &FournisseurCd, piste: u8, depuis_ms: u64) -> (FluxPcm, Vec<u8>) {
        let mut flux = f.ouvrir(&source_id(ATTENDU, piste), depuis_ms).unwrap();
        let mut v = Vec::new();
        flux.lecteur.read_to_end(&mut v).unwrap();
        (flux, v)
    }

    /// Témoin 3 — les octets servis pour une piste sont exactement ses
    /// secteurs, sans un octet de la piste voisine.
    #[test]
    fn les_octets_d_une_piste_sont_exactement_ses_secteurs() {
        let (f, _) = fournisseur();
        let toc = toc_du_vecteur();
        let (debut, n) = (toc.piste(2).unwrap().debut, toc.secteurs(2).unwrap());
        let (flux, v) = lire(&f, 2, 0);
        assert_eq!(flux.octets, v.len() as u64);
        assert_eq!(v.len(), n as usize * 2_352);
        assert_eq!(
            &v[..2_352],
            &contenu_des_secteurs(debut, 1)[..],
            "premier secteur"
        );
        assert_eq!(
            &v[v.len() - 2_352..],
            &contenu_des_secteurs(debut + n - 1, 1)[..],
            "dernier secteur : celui d'avant la piste 3"
        );
        assert_eq!(v, contenu_des_secteurs(debut, n));
        assert_eq!(flux.duree_ms, toc.duree_ms(2).unwrap());
    }

    /// Témoin 4 — l'enchaînement est sans blanc, à l'octet près : la fin de
    /// la piste 2 suivie du début de la piste 3 est la suite exacte des
    /// secteurs du disque, sans trou ni recouvrement.
    #[test]
    fn l_enchainement_des_pistes_est_continu_a_l_octet_pres() {
        let (f, _) = fournisseur();
        let toc = toc_du_vecteur();
        let (_, a) = lire(&f, 2, 0);
        let (_, b) = lire(&f, 3, 0);
        let debut = toc.piste(2).unwrap().debut;
        let fin = toc.fin_de_piste(3).unwrap();
        let mut joint = a.clone();
        joint.extend_from_slice(&b);
        assert_eq!(joint.len(), (fin - debut) as usize * 2_352);
        // La jonction elle-même, 4 secteurs de part et d'autre.
        let j = a.len();
        assert_eq!(
            &joint[j - 4 * 2_352..j + 4 * 2_352],
            &contenu_des_secteurs(toc.piste(3).unwrap().debut - 4, 8)[..]
        );
        assert_eq!(joint, contenu_des_secteurs(debut, fin - debut));
    }

    /// Témoin 5 — l'avance dans la piste tombe sur le bon secteur.
    #[test]
    fn l_avance_dans_la_piste_tombe_sur_le_bon_secteur() {
        let (f, _) = fournisseur();
        let toc = toc_du_vecteur();
        let debut = toc.piste(4).unwrap().debut;
        let n = toc.secteurs(4).unwrap();
        // 61,5 s = 4 612,5 secteurs : on reprend au secteur 4 612.
        let (flux, v) = lire(&f, 4, 61_500);
        assert_eq!(&v[..2_352], &contenu_des_secteurs(debut + 4_612, 1)[..]);
        assert_eq!(v.len(), (n - 4_612) as usize * 2_352);
        assert_eq!(flux.octets, v.len() as u64);
        // La durée affichée reste celle de la piste entière.
        assert_eq!(flux.duree_ms, toc.duree_ms(4).unwrap());
        // Une avance au-delà de la fin ne lit rien de la piste suivante.
        let (_, rien) = lire(&f, 4, 10_000_000);
        assert!(rien.is_empty());
    }

    #[test]
    fn un_autre_disque_ne_joue_pas_la_file_du_precedent() {
        let (f, _) = fournisseur();
        let e = f.ouvrir(&source_id("autre-disque", 2), 0).err().unwrap();
        assert!(e.contains("disque a changé"), "{e}");
        assert!(f.ouvrir(&source_id(ATTENDU, 42), 0).is_err());
        assert!(f.ouvrir("n-importe-quoi", 0).is_err());
    }
}
