//! CPACE-X25519-SHA512, role serveur A, confirmation mutuelle (draft-21).
//!
//! Sendspin/spec@8a8b1cbd6764ea116dcaa07e41544a97bc13080c fixe les domaines,
//! AD et le SID. Les etats sont consommes : ni scalaire rejoue, ni cle finale
//! accessible avant verification de Tb et, en dynamique, de la liaison du code.
//! Les primitives viennent de Dalek/RustCrypto ; aucune arithmetique de champ
//! ni implementation de HMAC/AEAD n'est recopiee ici.

use aes_gcm::aead::{Aead, KeyInit};
use curve25519_elligator2::{MontgomeryPoint, elligator2::Legacy};
use hmac::{Hmac, Mac};
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256, Sha512};
use subtle::ConstantTimeEq;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use super::psk::{CategoriePsk, PskPair};
use super::suite::Suite;

const SID_LABEL: &[u8] = b"sendspin-pair-pake-v1";
const PSK_WRAP: &[u8] = b"sendspin-pair-psk-wrap-v1";
const NONCE_WRAP: &[u8] = b"sendspin-pair-nonce-wrap-v1";
const COMMIT_LABEL: &[u8] = b"sendspin-pair-commit-v1";
const CODE_LABEL: &[u8] = b"sendspin-pairing-code-derive-v1";

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ErreurAppairage {
    #[error("code d'appairage incorrect")]
    CodeIncorrect,
    #[error("protocole d'appairage invalide : {0}")]
    Protocole(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormatCode {
    Statique,
    Dynamique,
    Qr,
}

/// Les octets du code ne sont ni journalisables ni serialisables.
pub struct CodeAppairage {
    format: FormatCode,
    octets: Zeroizing<Vec<u8>>,
}

impl CodeAppairage {
    pub fn nouveau(format: FormatCode, octets: &[u8]) -> Result<Self, ErreurAppairage> {
        let taille = match format {
            FormatCode::Statique => 8,
            FormatCode::Dynamique => 6,
            FormatCode::Qr => 24,
        };
        if octets.len() != taille
            || (format != FormatCode::Qr && !octets.iter().all(u8::is_ascii_digit))
        {
            return Err(ErreurAppairage::Protocole("format du code"));
        }
        Ok(Self {
            format,
            octets: Zeroizing::new(octets.to_vec()),
        })
    }
}

/// Le compteur est gere par la connexion ; aucun SID ne doit etre reutilise.
#[derive(Debug, Clone, Copy)]
pub struct ContextePake {
    h: [u8; 32],
    index: std::num::NonZeroU32,
    tour: std::num::NonZeroU32,
}

impl ContextePake {
    pub fn nouveau(h: [u8; 32], index: u32, tour: u32) -> Result<Self, ErreurAppairage> {
        Ok(Self {
            h,
            index: std::num::NonZeroU32::new(index)
                .ok_or(ErreurAppairage::Protocole("pairing_index nul"))?,
            tour: std::num::NonZeroU32::new(tour).ok_or(ErreurAppairage::Protocole("round nul"))?,
        })
    }

    pub fn sid(&self) -> Vec<u8> {
        [
            SID_LABEL,
            &self.h,
            &self.index.get().to_be_bytes(),
            &self.tour.get().to_be_bytes(),
        ]
        .concat()
    }
}

/// Le commitment vient de client/pair-init, avant l'emission de nonce_A.
/// Ces valeurs publiques sont conservees entre les tours du meme essai.
/// Leur copie ne copie pas de scalaire CPace ni de cle de confirmation.
#[derive(Clone)]
pub struct LiaisonDynamique {
    nonce_a: [u8; 32],
    commit_b: [u8; 32],
}

impl LiaisonDynamique {
    pub fn nouvelle(commit_b: [u8; 32]) -> Self {
        let mut nonce_a = [0; 32];
        OsRng.fill_bytes(&mut nonce_a);
        Self { nonce_a, commit_b }
    }

    pub fn nonce_a(&self) -> &[u8; 32] {
        &self.nonce_a
    }
}

pub struct PakeServeur {
    echange: Echange,
    contexte: ContextePake,
    code: CodeAppairage,
    liaison: Option<LiaisonDynamique>,
    suite: Suite,
}

pub struct AttenteConfirmation {
    confirmation: Confirmation,
    contexte: ContextePake,
    code: CodeAppairage,
    liaison: Option<LiaisonDynamique>,
    suite: Suite,
}

/// Cette valeur ne peut etre construite qu'apres les verifications de confiance.
pub struct AppairageAuthentifie {
    cle_psk: Zeroizing<[u8; 32]>,
    suite: Suite,
}

impl PakeServeur {
    pub fn demarrer(
        code: CodeAppairage,
        contexte: ContextePake,
        suite: Suite,
        liaison: Option<LiaisonDynamique>,
    ) -> Result<Self, ErreurAppairage> {
        if (code.format == FormatCode::Statique) != liaison.is_none() {
            return Err(ErreurAppairage::Protocole(
                "liaison incompatible avec le format",
            ));
        }
        if code.format == FormatCode::Statique && contexte.tour.get() != 1 {
            return Err(ErreurAppairage::Protocole("un seul tour en code statique"));
        }
        let echange = Echange::nouveau(
            &code.octets,
            b"",
            contexte.sid(),
            b"server",
            b"client",
            StaticSecret::random_from_rng(OsRng),
        )?;
        Ok(Self {
            echange,
            contexte,
            code,
            liaison,
            suite,
        })
    }

    pub fn partage(&self) -> &[u8; 32] {
        &self.echange.ya
    }

    /// Consomme l'ephemere avant toute erreur de validation.
    pub fn recevoir_partage(self, yb: &[u8]) -> Result<AttenteConfirmation, ErreurAppairage> {
        Ok(AttenteConfirmation {
            confirmation: self.echange.recevoir(yb)?,
            contexte: self.contexte,
            code: self.code,
            liaison: self.liaison,
            suite: self.suite,
        })
    }
}

impl AttenteConfirmation {
    pub fn tag_serveur(&self) -> [u8; 64] {
        self.confirmation.tag_serveur()
    }

    pub fn confirmer(
        self,
        tag_client: &[u8],
        nonce_b_chiffre: Option<&[u8]>,
    ) -> Result<AppairageAuthentifie, ErreurAppairage> {
        self.confirmation.verifier(tag_client)?;
        match (self.liaison, nonce_b_chiffre) {
            (None, None) => (),
            (Some(liaison), Some(chiffre)) => {
                let cle = self.confirmation.cle_wrap(NONCE_WRAP);
                let nonce_b = ouvrir(self.suite, &cle, chiffre)?;
                let commit: [u8; 32] = Sha256::new()
                    .chain_update(COMMIT_LABEL)
                    .chain_update(nonce_b.as_slice())
                    .finalize()
                    .into();
                if !bool::from(commit.ct_eq(&liaison.commit_b)) {
                    return Err(ErreurAppairage::Protocole("commitment du nonce client"));
                }
                let digest: [u8; 32] = Sha256::new()
                    .chain_update(CODE_LABEL)
                    .chain_update(self.contexte.h)
                    .chain_update(liaison.nonce_a)
                    .chain_update(nonce_b.as_slice())
                    .finalize()
                    .into();
                let attendu = match self.code.format {
                    FormatCode::Dynamique => {
                        let valeur = digest
                            .iter()
                            .fold(0u32, |r, b| (r * 256 + u32::from(*b)) % 1_000_000);
                        Zeroizing::new(format!("{valeur:06}").into_bytes())
                    }
                    FormatCode::Qr => Zeroizing::new(digest[..24].to_vec()),
                    FormatCode::Statique => {
                        return Err(ErreurAppairage::Protocole("nonce en code statique"));
                    }
                };
                if !bool::from(attendu.as_slice().ct_eq(self.code.octets.as_slice())) {
                    return Err(ErreurAppairage::Protocole(
                        "code non lie aux nonces et a Noise",
                    ));
                }
            }
            _ => return Err(ErreurAppairage::Protocole("presence du nonce client")),
        }
        Ok(AppairageAuthentifie {
            cle_psk: self.confirmation.cle_wrap(PSK_WRAP),
            suite: self.suite,
        })
    }
}

impl AppairageAuthentifie {
    /// Une seule cle finale, apres confirmation et liaison du code.
    pub fn recevoir_psk(self, client_id: &str, chiffre: &[u8]) -> Result<PskPair, ErreurAppairage> {
        let secret = ouvrir(self.suite, &self.cle_psk, chiffre)?;
        PskPair::pour_pair(client_id, *secret, CategoriePsk::LongueDuree)
            .map_err(|_| ErreurAppairage::Protocole("cle finale ou identite invalide"))
    }
}

struct Echange {
    secret: StaticSecret,
    ya: [u8; 32],
    ada: Vec<u8>,
    adb: Vec<u8>,
    sid: Vec<u8>,
}

struct Confirmation {
    mac_key: Zeroizing<[u8; 64]>,
    isk: Zeroizing<[u8; 64]>,
    message_a: Vec<u8>,
    message_b: Vec<u8>,
    sid: Vec<u8>,
}

impl Echange {
    fn nouveau(
        prs: &[u8],
        ci: &[u8],
        sid: Vec<u8>,
        ada: &[u8],
        adb: &[u8],
        secret: StaticSecret,
    ) -> Result<Self, ErreurAppairage> {
        let g = generateur(prs, ci, &sid)?;
        let ya = *multiplier(&secret, &g)?;
        Ok(Self {
            secret,
            ya,
            ada: ada.to_vec(),
            adb: adb.to_vec(),
            sid,
        })
    }

    fn recevoir(self, yb: &[u8]) -> Result<Confirmation, ErreurAppairage> {
        let yb: [u8; 32] = yb
            .try_into()
            .map_err(|_| ErreurAppairage::Protocole("taille du partage CPace"))?;
        let k = multiplier(&self.secret, &yb)?;
        let message_a = lv_cat(&[&self.ya, &self.ada]);
        let message_b = lv_cat(&[&yb, &self.adb]);
        let prefixe = Zeroizing::new(lv_cat(&[b"CPace255_ISK", &self.sid, k.as_slice()]));
        let isk: [u8; 64] = Sha512::new()
            .chain_update(prefixe.as_slice())
            .chain_update(&message_a)
            .chain_update(&message_b)
            .finalize()
            .into();
        let isk = Zeroizing::new(isk);
        let mac_key = Sha512::new()
            .chain_update(b"CPaceMac")
            .chain_update(&self.sid)
            .chain_update(isk.as_slice())
            .finalize()
            .into();
        Ok(Confirmation {
            mac_key: Zeroizing::new(mac_key),
            isk,
            message_a,
            message_b,
            sid: self.sid,
        })
    }
}

impl Confirmation {
    fn mac(&self, message: &[u8]) -> Hmac<Sha512> {
        let mut mac = <Hmac<Sha512> as Mac>::new_from_slice(self.mac_key.as_slice())
            .expect("HMAC accepte une cle de 64 octets");
        Mac::update(&mut mac, message);
        mac
    }

    fn tag_serveur(&self) -> [u8; 64] {
        self.mac(&self.message_a).finalize().into_bytes().into()
    }

    fn verifier(&self, tag: &[u8]) -> Result<(), ErreurAppairage> {
        if tag.len() != 64 {
            return Err(ErreurAppairage::Protocole("taille de confirmation CPace"));
        }
        if self.message_a == self.message_b {
            return Err(ErreurAppairage::CodeIncorrect);
        }
        self.mac(&self.message_b)
            .verify_slice(tag)
            .map_err(|_| ErreurAppairage::CodeIncorrect)
    }

    fn cle_wrap(&self, label: &[u8]) -> Zeroizing<[u8; 32]> {
        Zeroizing::new(
            Sha256::new()
                .chain_update(label)
                .chain_update(&self.sid)
                .chain_update(self.isk.as_slice())
                .finalize()
                .into(),
        )
    }
}

fn ouvrir(
    suite: Suite,
    cle: &[u8; 32],
    chiffre: &[u8],
) -> Result<Zeroizing<[u8; 32]>, ErreurAppairage> {
    if chiffre.len() != 48 {
        return Err(ErreurAppairage::Protocole("taille du champ chiffre"));
    }
    let nonce = [0u8; 12];
    let clair = match suite {
        Suite::ChaChaPoly => {
            chacha20poly1305::ChaCha20Poly1305::new(cle.into()).decrypt((&nonce).into(), chiffre)
        }
        Suite::AesGcm => aes_gcm::Aes256Gcm::new(cle.into()).decrypt((&nonce).into(), chiffre),
    }
    .map_err(|_| ErreurAppairage::Protocole("authenticite du champ chiffre"))?;
    let clair = Zeroizing::new(clair);
    let octets = clair
        .as_slice()
        .try_into()
        .map_err(|_| ErreurAppairage::Protocole("taille du secret dechiffre"))?;
    Ok(Zeroizing::new(octets))
}

fn multiplier(
    secret: &StaticSecret,
    point: &[u8; 32],
) -> Result<Zeroizing<[u8; 32]>, ErreurAppairage> {
    let k = secret.diffie_hellman(&PublicKey::from(*point));
    if !k.was_contributory() {
        return Err(ErreurAppairage::Protocole("point CPace de faible ordre"));
    }
    Ok(Zeroizing::new(*k.as_bytes()))
}

fn generateur(prs: &[u8], ci: &[u8], sid: &[u8]) -> Result<Zeroizing<[u8; 32]>, ErreurAppairage> {
    let pad = vec![0; 127usize.saturating_sub(longueur_prefixe(prs.len()) + prs.len() + 9)];
    let entree = Zeroizing::new(lv_cat(&[b"CPace255", prs, &pad, ci, sid]));
    let hash = Zeroizing::new(<[u8; 64]>::from(Sha512::digest(entree.as_slice())));
    let mut r = Zeroizing::new(<[u8; 32]>::try_from(&hash[..32]).expect("demi SHA512"));
    // CPace ignore uniquement le bit 255. RFC9380::from_representative et
    // map_to_point effacent aussi le bit 254 : ce n'est pas ce mapping.
    r[31] &= 0x7f;
    let point = MontgomeryPoint::from_representative::<Legacy>(&r)
        .ok_or(ErreurAppairage::Protocole("generateur CPace invalide"))?;
    Ok(Zeroizing::new(point.to_bytes()))
}

fn longueur_prefixe(mut n: usize) -> usize {
    let mut taille = 1;
    while n >= 128 {
        n >>= 7;
        taille += 1;
    }
    taille
}

fn lv_cat(parties: &[&[u8]]) -> Vec<u8> {
    let mut sortie = Vec::with_capacity(
        parties
            .iter()
            .map(|p| p.len() + longueur_prefixe(p.len()))
            .sum(),
    );
    for partie in parties {
        let mut taille = partie.len();
        loop {
            let octet = (taille & 127) as u8;
            taille >>= 7;
            sortie.push(octet | if taille == 0 { 0 } else { 128 });
            if taille == 0 {
                break;
            }
        }
        sortie.extend_from_slice(partie);
    }
    sortie
}

macro_rules! debug_masque {
    ($($t:ty),+) => { $(
        impl std::fmt::Debug for $t {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(concat!(stringify!($t), " { .. }"))
            }
        }
    )+ };
}
debug_masque!(
    CodeAppairage,
    LiaisonDynamique,
    PakeServeur,
    AttenteConfirmation,
    AppairageAuthentifie
);

#[cfg(test)]
#[path = "pake/tests.rs"]
mod tests;
