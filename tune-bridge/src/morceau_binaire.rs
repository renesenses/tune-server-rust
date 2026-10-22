//! Les morceaux de flux en trames WebSocket binaires (#6564).
//!
//! Un serveur recent envoie les morceaux d'un flux (ecoute a distance, Tune
//! Circle) en trames binaires des que ce pont a annonce
//! `protocol::CAPACITE_MORCEAUX_BINAIRES` dans `relay.registered`. Le base64
//! des trames texte `BINARY:<id>:<base64>` coutait un tiers de plus sur le
//! trafic entrant du VPS.
//!
//! Format (version 1), ecrit par `tune-core/src/cloud/relay.rs` :
//!
//! ```text
//! [version = 1][n = longueur de l'id, 1 octet][id, n octets UTF-8]
//! [seq, 8 octets gros-boutiste][octets du morceau]
//! ```
//!
//! `seq` compte les morceaux d'un flux a partir de 0. Les messages de
//! controle (`relay.stream_start`, `relay.stream_end`…) restent du JSON
//! texte : la fin d'un flux est toujours `relay.stream_end`.

use std::collections::HashMap;

/// Seule version de format lue par ce pont.
pub const VERSION: u8 = 1;

/// Un morceau lu dans une trame binaire. Emprunte la trame : pas de copie
/// avant de savoir si un puits l'attend.
#[derive(Debug, PartialEq, Eq)]
pub struct Morceau<'a> {
    pub id: &'a str,
    pub seq: u64,
    pub octets: &'a [u8],
}

/// Lit une trame binaire de morceau. `None` pour toute trame qui ne suit pas
/// le format : autre version, identifiant vide ou pas en UTF-8, trame
/// tronquee. Mieux vaut jeter une trame qu'inventer un morceau.
pub fn lire(trame: &[u8]) -> Option<Morceau<'_>> {
    let (&version, reste) = trame.split_first()?;
    if version != VERSION {
        return None;
    }
    let (&n, reste) = reste.split_first()?;
    let n = n as usize;
    if n == 0 || reste.len() < n + 8 {
        return None;
    }
    let (id, reste) = reste.split_at(n);
    let id = std::str::from_utf8(id).ok()?;
    let (seq, octets) = reste.split_at(8);
    let seq = u64::from_be_bytes(seq.try_into().ok()?);
    Some(Morceau { id, seq, octets })
}

/// Le rang attendu de chaque flux binaire en cours, pour UNE connexion de
/// serveur. Le fil WebSocket est ordonne, et le serveur numerote ses
/// morceaux : un ecart ne peut venir que d'un defaut, et il se refuse.
#[derive(Debug, Default)]
pub struct Rangs {
    attendus: HashMap<String, u64>,
}

impl Rangs {
    /// `true` si `seq` est bien le rang attendu pour `id` (0 ouvre un flux).
    /// Un ecart oublie le flux : ses morceaux suivants sont refuses aussi.
    pub fn accepter(&mut self, id: &str, seq: u64) -> bool {
        if seq == 0 {
            self.attendus.insert(id.to_string(), 1);
            return true;
        }
        match self.attendus.get_mut(id) {
            Some(attendu) if *attendu == seq => {
                *attendu += 1;
                true
            }
            _ => {
                self.attendus.remove(id);
                false
            }
        }
    }

    pub fn oublier(&mut self, id: &str) {
        self.attendus.remove(id);
    }

    /// Ne garde que les flux encore ouverts : la carte reste bornee par le
    /// nombre de flux en vol, sans attendre `relay.stream_end`.
    pub fn ne_garder_que(&mut self, ouvert: impl Fn(&str) -> bool) {
        self.attendus.retain(|id, _| ouvert(id));
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.attendus.len()
    }
}

#[cfg(test)]
#[path = "morceaux_binaires_tests.rs"]
mod bout_en_bout;

#[cfg(test)]
mod tests {
    use super::*;

    /// Le vecteur de reference du format, recopie A L'IDENTIQUE de
    /// `tune-core/src/cloud/relay.rs` (`morceaux_binaires_6564_tests`) : les
    /// deux bouts du fil lisent le meme contrat. Id `req-1`, rang 2, `fLaC`.
    pub(super) const VECTEUR: &[u8] = &[
        1, 5, b'r', b'e', b'q', b'-', b'1', 0, 0, 0, 0, 0, 0, 0, 2, b'f', b'L', b'a', b'C',
    ];

    #[test]
    fn le_vecteur_de_reference_se_lit() {
        assert_eq!(
            lire(VECTEUR),
            Some(Morceau {
                id: "req-1",
                seq: 2,
                octets: b"fLaC",
            })
        );
    }

    #[test]
    fn un_morceau_vide_est_lisible() {
        let m = lire(&VECTEUR[..VECTEUR.len() - 4]).unwrap();
        assert_eq!(m.octets, b"");
    }

    #[test]
    fn ce_qui_ne_suit_pas_le_format_est_refuse() {
        assert_eq!(lire(&[]), None, "trame vide");
        let mut autre_version = VECTEUR.to_vec();
        autre_version[0] = 2;
        assert_eq!(lire(&autre_version), None, "autre version");
        assert_eq!(lire(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0]), None, "id vide");
        assert_eq!(lire(&VECTEUR[..10]), None, "rang tronque");
        let mut pas_utf8 = VECTEUR.to_vec();
        pas_utf8[2] = 0xFF;
        assert_eq!(lire(&pas_utf8), None, "id hors UTF-8");
    }

    #[test]
    fn les_rangs_refusent_un_trou_ou_un_desordre() {
        let mut r = Rangs::default();
        assert!(r.accepter("a", 0));
        assert!(r.accepter("a", 1));
        assert!(!r.accepter("a", 3), "morceau 2 perdu");
        assert!(!r.accepter("a", 4), "le flux reste refuse apres un trou");

        assert!(r.accepter("b", 0));
        assert!(r.accepter("b", 1));
        assert!(!r.accepter("b", 1), "morceau rejoue");

        assert!(!r.accepter("c", 5), "flux jamais ouvert");
    }

    #[test]
    fn les_flux_fermes_sont_oublies() {
        let mut r = Rangs::default();
        assert!(r.accepter("a", 0));
        assert!(r.accepter("b", 0));
        r.ne_garder_que(|id| id == "b");
        assert_eq!(r.len(), 1);
        r.oublier("b");
        assert_eq!(r.len(), 0);
    }
}
