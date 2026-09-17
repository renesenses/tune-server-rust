//! Cadrage de messaging.md, revision cd9330ef (type 1 + flags).
use super::MAX_CLAIR;
use crate::sendspin::ErreurSendspin;

/// Limite locale, corps sans l'octet de type. Aucun tampon sans borne.
pub const MAX_CORPS_MESSAGE: usize = 1024 * 1024;
const FRAGMENT: u8 = 1;
const PREMIER: u8 = 2;
const DERNIER: u8 = 1;

fn erreur(m: &'static str) -> ErreurSendspin {
    ErreurSendspin::EtatInattendu(m)
}

#[derive(Default)]
pub(super) struct Reassemblage {
    message: Option<(u8, Vec<u8>)>,
    invalide: bool,
}
impl Reassemblage {
    pub(super) fn invalider(&mut self) {
        self.message = None;
        self.invalide = true;
    }

    pub(super) fn accepter(
        &mut self,
        typ: u8,
        corps: &[u8],
    ) -> Result<Option<(u8, Vec<u8>)>, ErreurSendspin> {
        if self.invalide {
            return Err(erreur(
                "transport invalide apres une erreur de fragmentation",
            ));
        }
        let r = self.ajouter(typ, corps);
        if r.is_err() {
            self.invalider();
        }
        r
    }

    fn ajouter(&mut self, typ: u8, corps: &[u8]) -> Result<Option<(u8, Vec<u8>)>, ErreurSendspin> {
        if typ != FRAGMENT {
            if self.message.is_some() {
                return Err(erreur("message entrelace dans une fragmentation"));
            }
            return Ok(Some((typ, corps.to_vec())));
        }
        let (&flags, suite) = corps
            .split_first()
            .ok_or_else(|| erreur("fragment sans drapeaux"))?;
        if flags & !(PREMIER | DERNIER) != 0 {
            return Err(erreur("drapeaux de fragment reserves"));
        }
        let donnees = if flags & PREMIER != 0 {
            if self.message.is_some() {
                return Err(erreur("deux premiers fragments entrelaces"));
            }
            let (&original, donnees) = suite
                .split_first()
                .ok_or_else(|| erreur("premier fragment sans type original"))?;
            if original == FRAGMENT {
                return Err(erreur("fragmentation imbriquee interdite"));
            }
            self.message = Some((original, Vec::new()));
            donnees
        } else {
            suite
        };
        let (_, tampon) = self
            .message
            .as_mut()
            .ok_or_else(|| erreur("fragment sans premier fragment"))?;
        let taille = tampon
            .len()
            .checked_add(donnees.len())
            .filter(|n| *n <= MAX_CORPS_MESSAGE)
            .ok_or_else(|| erreur("message reassemble au-dela de 1 Mio"))?;
        // Croissance amortie pour les petits fragments, plafonnee avant
        // allocation : ni copie quadratique ni capacite au-dela de 1 Mio.
        if taille > tampon.capacity() {
            let capacite = taille.max(4096).next_power_of_two().min(MAX_CORPS_MESSAGE);
            tampon
                .try_reserve_exact(capacite - tampon.len())
                .map_err(|_| erreur("memoire de reassemblage indisponible"))?;
        }
        tampon.extend_from_slice(donnees);
        if flags & DERNIER != 0 {
            Ok(self.message.take())
        } else {
            Ok(None)
        }
    }
}

pub(super) fn decouper(typ: u8, corps: &[u8]) -> Result<Vec<Vec<u8>>, ErreurSendspin> {
    if typ == FRAGMENT {
        return Err(erreur("type applicatif de fragmentation interdit"));
    }
    if corps.len() > MAX_CORPS_MESSAGE {
        return Err(erreur("message a emettre au-dela de 1 Mio"));
    }
    if corps.len() < MAX_CLAIR {
        return Ok(vec![[&[typ], corps].concat()]);
    }
    let mut fragments = Vec::new();
    let mut reste = corps;
    let mut premier = true;
    while !reste.is_empty() {
        let capacite = MAX_CLAIR - if premier { 3 } else { 2 };
        let n = reste.len().min(capacite);
        let dernier = n == reste.len();
        let flags = if premier { PREMIER } else { 0 } | if dernier { DERNIER } else { 0 };
        let mut fragment = Vec::with_capacity(n + if premier { 3 } else { 2 });
        fragment.extend_from_slice(&[FRAGMENT, flags]);
        if premier {
            fragment.push(typ);
        }
        fragment.extend_from_slice(&reste[..n]);
        fragments.push(fragment);
        reste = &reste[n..];
        premier = false;
    }
    Ok(fragments)
}
