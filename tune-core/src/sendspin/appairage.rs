//! Sequence S2-b commune aux trois methodes, independante des I/O.
//! Le pilote WebSocket doit executer les actions dans l'ordre et appeler
//! confirmer_persistance seulement apres le succes durable du magasin.
//! Cette machine n'active jamais de role ni de lecture.
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::identite::{b64url, cle_publique_du_pair, depuis_b64url};
use super::jeton::lire_code;
use super::pake::{
    AppairageAuthentifie, AttenteConfirmation, CodeAppairage, ContextePake, ErreurAppairage,
    FormatCode, LiaisonDynamique, PakeServeur,
};
use super::poignee::InfosPair;
use super::psk::{CategoriePsk, PskPair};

const ATTENTE_GESTE: Duration = Duration::from_secs(300);
const DUREE_ESSAI: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodeAppairage {
    Psk,
    Statique,
    Dynamique,
    Qr,
}

impl MethodeAppairage {
    pub fn categorie_requise(self) -> CategoriePsk {
        if self == Self::Psk {
            CategoriePsk::Appairage
        } else {
            CategoriePsk::Sentinelle
        }
    }

    fn format(self) -> Option<FormatCode> {
        match self {
            Self::Psk => None,
            Self::Statique => Some(FormatCode::Statique),
            Self::Dynamique => Some(FormatCode::Dynamique),
            Self::Qr => Some(FormatCode::Qr),
        }
    }

    fn dynamique(self) -> bool {
        matches!(self, Self::Dynamique | Self::Qr)
    }

    fn activation(self) -> Value {
        let pairing = match self {
            Self::Psk => json!({"method":"pairing_psk"}),
            Self::Statique => json!({"method":"static_pairing_code"}),
            Self::Dynamique => json!({"method":"dynamic_pairing_code","format":"digits"}),
            Self::Qr => json!({"method":"dynamic_pairing_code","format":"qr_code"}),
        };
        json!({"activities":["pairing"],"active_roles":[],"pairing":pairing})
    }
}

/// Aucun secret n'est serialise dans une vue operateur. Les PSK ne sortent
/// que vers le magasin et le prochain transport, avec leur identite liee.
#[derive(Debug)]
pub enum ActionAppairage {
    Envoyer {
        type_message: &'static str,
        payload: Value,
    },
    AttendreGeste {
        message: Option<String>,
    },
    DemanderCode {
        format: FormatCode,
        tour: u32,
    },
    Persister(PskPair),
    Promouvoir(PskPair),
    Abandonne,
    Fermer,
}

enum Etat {
    Init,
    Saisie,
    Partage(PakeServeur),
    Confirmation(AttenteConfirmation),
    FinalePsk,
    FinaleCode(AppairageAuthentifie),
    Persistance(PskPair),
    Termine,
    Abandonne,
    Ferme,
}

pub struct AppairageServeur {
    infos: InfosPair,
    methode: MethodeAppairage,
    index: u32,
    tour: u32,
    liaison: Option<LiaisonDynamique>,
    code_avance: Option<CodeAppairage>,
    ignorer_en_vol: bool,
    echeance: Instant,
    etat: Etat,
}

impl AppairageServeur {
    /// Le pilote a deja effectue, si necessaire, le re-echange vers PR ou SN.
    /// L'index compte les activations d'appairage depuis CE handshake.
    pub fn commencer(
        infos: InfosPair,
        methode: MethodeAppairage,
        index: u32,
        maintenant: Instant,
    ) -> Result<(Self, Vec<ActionAppairage>), ErreurAppairage> {
        cle_publique_du_pair(&infos.client_id).map_err(|_| protocole("identite de connexion"))?;
        if index == 0 || infos.categorie_psk != methode.categorie_requise() {
            return Err(protocole("methode incompatible avec la PSK ou l'index"));
        }
        let actions = vec![envoyer("server/activate", methode.activation())];
        Ok((
            Self {
                infos,
                methode,
                index,
                tour: 1,
                liaison: None,
                code_avance: None,
                ignorer_en_vol: false,
                echeance: maintenant + ATTENTE_GESTE,
                etat: Etat::Init,
            },
            actions,
        ))
    }

    /// Nouvelle activation apres annulation, sur le meme transport Noise.
    /// Les reponses non indexees de l'ancien essai restent sans effet jusqu'au
    /// premier pending/init portant le nouvel index. Un abort reste traite :
    /// il peut refuser la nouvelle activation avant tout init.
    pub fn recommencer(
        self,
        methode: MethodeAppairage,
        maintenant: Instant,
    ) -> Result<(Self, Vec<ActionAppairage>), ErreurAppairage> {
        if !matches!(self.etat, Etat::Abandonne) {
            return Err(protocole("nouvel essai sans abandon du precedent"));
        }
        let index = self
            .index
            .checked_add(1)
            .ok_or_else(|| protocole("index epuise"))?;
        let (mut suivant, actions) = Self::commencer(self.infos, methode, index, maintenant)?;
        suivant.ignorer_en_vol = true;
        Ok((suivant, actions))
    }

    pub fn echeance(&self) -> Option<Instant> {
        self.active().then_some(self.echeance)
    }

    fn active(&self) -> bool {
        !matches!(self.etat, Etat::Termine | Etat::Abandonne | Etat::Ferme)
    }

    /// Une mauvaise saisie operateur laisse le tour disponible pour correction.
    pub fn saisir_code(
        &mut self,
        saisie: &str,
        maintenant: Instant,
    ) -> Result<Vec<ActionAppairage>, ErreurAppairage> {
        if let Some(actions) = self.expirer(maintenant) {
            return Ok(actions);
        }
        let format = self
            .methode
            .format()
            .ok_or_else(|| protocole("pas de code en PSK"))?;
        if !matches!(self.etat, Etat::Saisie)
            && !(self.methode == MethodeAppairage::Statique && matches!(self.etat, Etat::Init))
        {
            return Err(protocole("saisie hors d'un tour"));
        }
        let code = lire_code(saisie, format)?;
        if matches!(self.etat, Etat::Init) {
            self.code_avance = Some(code);
            return Ok(Vec::new());
        }
        self.envoyer_partage(code)
    }

    fn envoyer_partage(
        &mut self,
        code: CodeAppairage,
    ) -> Result<Vec<ActionAppairage>, ErreurAppairage> {
        let pake = PakeServeur::demarrer(
            code,
            ContextePake::nouveau(self.infos.condensat_poignee, self.index, self.tour)?,
            self.infos.suite,
            self.liaison.clone(),
        )?;
        let action = envoyer(
            "server/pair-auth",
            json!({"pake_msg_1": b64url(pake.partage())}),
        );
        self.etat = Etat::Partage(pake);
        Ok(vec![action])
    }

    /// Un echec venant du pair ferme definitivement cette machine. Le pilote
    /// ferme le WebSocket sans erreur applicative ni enregistrement de PSK.
    pub fn recevoir(
        &mut self,
        type_message: &str,
        payload: &Value,
        maintenant: Instant,
    ) -> Result<Vec<ActionAppairage>, ErreurAppairage> {
        if !self.active() {
            return match self.etat {
                Etat::Abandonne | Etat::Termine if est_message_appairage(type_message) => {
                    Ok(vec![])
                }
                _ => Err(protocole("connexion d'appairage fermee")),
            };
        }
        if let Some(actions) = self.expirer(maintenant) {
            return Ok(actions);
        }
        let resultat = self.recevoir_actif(type_message, payload, maintenant);
        if resultat.is_err() {
            self.etat = Etat::Ferme;
            self.code_avance = None;
            self.liaison = None;
        }
        resultat
    }

    fn recevoir_actif(
        &mut self,
        type_message: &str,
        payload: &Value,
        maintenant: Instant,
    ) -> Result<Vec<ActionAppairage>, ErreurAppairage> {
        if self.ignorer_en_vol
            && type_message.starts_with("client/pair-")
            && !matches!(type_message, "client/pair-pending" | "client/pair-init")
        {
            return Ok(vec![]);
        }
        let objet = payload
            .as_object()
            .ok_or_else(|| protocole("payload non objet"))?;
        if matches!(type_message, "client/pair-pending" | "client/pair-init") {
            let index = objet
                .get("pairing_index")
                .and_then(Value::as_u64)
                .filter(|n| *n > 0 && *n <= u32::MAX as u64)
                .ok_or_else(|| protocole("pairing_index invalide"))? as u32;
            if index < self.index {
                return Ok(vec![]);
            }
            if index > self.index {
                return Err(protocole("pairing_index futur"));
            }
            self.ignorer_en_vol = false;
        }
        if type_message == "pair/abort" {
            let reason = objet
                .get("reason")
                .and_then(Value::as_str)
                .ok_or_else(|| protocole("raison d'abandon absente"))?;
            if ![
                "attempt_timeout",
                "concurrent_attempt",
                "method_not_supported",
                "pairing_code_mismatch",
                "user_cancelled",
            ]
            .contains(&reason)
            {
                return Err(protocole("raison d'abandon inconnue"));
            }
            self.abandonner();
            if reason == "concurrent_attempt" {
                self.etat = Etat::Ferme;
                return Ok(vec![ActionAppairage::Fermer]);
            }
            return Ok(vec![desactiver(), ActionAppairage::Abandonne]);
        }
        match type_message {
            "client/pair-pending"
                if matches!(self.etat, Etat::Init) && self.methode != MethodeAppairage::Psk =>
            {
                let message = match objet.get("message") {
                    None => None,
                    Some(Value::String(s)) => Some(s.chars().take(200).collect()),
                    _ => return Err(protocole("message de geste invalide")),
                };
                Ok(vec![ActionAppairage::AttendreGeste { message }])
            }
            "client/pair-init" if matches!(self.etat, Etat::Init) => {
                self.echeance = maintenant + DUREE_ESSAI;
                if self.methode.dynamique() {
                    let commit = champ_binaire::<32>(payload, "commit_B")?;
                    let liaison = LiaisonDynamique::nouvelle(*commit);
                    let nonce = b64url(liaison.nonce_a());
                    self.liaison = Some(liaison);
                    self.etat = Etat::Saisie;
                    Ok(vec![
                        envoyer("server/pair-init", json!({"nonce_A":nonce})),
                        self.demander_code(),
                    ])
                } else {
                    absent(payload, "commit_B")?;
                    if self.methode == MethodeAppairage::Psk {
                        self.etat = Etat::FinalePsk;
                        Ok(vec![])
                    } else if let Some(code) = self.code_avance.take() {
                        self.envoyer_partage(code)
                    } else {
                        self.etat = Etat::Saisie;
                        Ok(vec![self.demander_code()])
                    }
                }
            }
            "client/pair-auth" if matches!(self.etat, Etat::Partage(_)) => {
                let partage = champ_binaire::<32>(payload, "pake_msg_2")?;
                let Etat::Partage(pake) = std::mem::replace(&mut self.etat, Etat::Ferme) else {
                    unreachable!()
                };
                let confirmation = pake.recevoir_partage(partage.as_slice())?;
                let tag = b64url(&confirmation.tag_serveur());
                self.etat = Etat::Confirmation(confirmation);
                Ok(vec![envoyer(
                    "server/pair-confirm",
                    json!({"server_kc":tag}),
                )])
            }
            "client/pair-retry"
                if self.methode.dynamique() && matches!(self.etat, Etat::Confirmation(_)) =>
            {
                if self.tour >= 20 {
                    return Err(protocole("reprise invalide ou limite de tours"));
                }
                self.tour += 1;
                self.etat = Etat::Saisie; // detruit l'ancien CPace ; garde nonce/commit et echeance
                Ok(vec![
                    envoyer("server/pair-init", json!({})),
                    self.demander_code(),
                ])
            }
            "client/pair-confirm" if matches!(self.etat, Etat::Confirmation(_)) => {
                let tag = champ_binaire::<64>(payload, "client_kc")?;
                let nonce = if self.methode.dynamique() {
                    Some(champ_binaire::<48>(payload, "wrapped_nonce_B")?)
                } else {
                    absent(payload, "wrapped_nonce_B")?;
                    None
                };
                let Etat::Confirmation(confirmation) =
                    std::mem::replace(&mut self.etat, Etat::Ferme)
                else {
                    unreachable!()
                };
                match confirmation.confirmer(tag.as_slice(), nonce.as_ref().map(|n| n.as_slice())) {
                    Ok(authentifie) => {
                        self.etat = Etat::FinaleCode(authentifie);
                        Ok(vec![])
                    }
                    Err(ErreurAppairage::CodeIncorrect) => {
                        self.abandonner();
                        Ok(vec![
                            envoyer("pair/abort", json!({"reason":"pairing_code_mismatch"})),
                            desactiver(),
                            ActionAppairage::Abandonne,
                        ])
                    }
                    Err(e) => Err(e),
                }
            }
            "client/pair-finalize"
                if matches!(self.etat, Etat::FinalePsk | Etat::FinaleCode(_)) =>
            {
                let cle = match std::mem::replace(&mut self.etat, Etat::Ferme) {
                    Etat::FinalePsk => {
                        absent(payload, "wrapped_psk")?;
                        let secret = champ_binaire::<32>(payload, "long_term_psk")?;
                        PskPair::pour_pair(
                            &self.infos.client_id,
                            *secret,
                            CategoriePsk::LongueDuree,
                        )
                        .map_err(|_| protocole("PSK finale invalide"))?
                    }
                    Etat::FinaleCode(authentifie) => {
                        absent(payload, "long_term_psk")?;
                        let chiffre = champ_binaire::<48>(payload, "wrapped_psk")?;
                        authentifie.recevoir_psk(&self.infos.client_id, chiffre.as_slice())?
                    }
                    _ => unreachable!(),
                };
                self.etat = Etat::Persistance(cle.clone());
                Ok(vec![ActionAppairage::Persister(cle)])
            }
            _ => Err(protocole("message d'appairage hors sequence")),
        }
    }

    /// Accuse reception puis re-echange ; aucun succes de fil avant cet appel.
    /// Une erreur du magasin impose au pilote de fermer sans appeler ici.
    pub fn confirmer_persistance(&mut self) -> Result<Vec<ActionAppairage>, ErreurAppairage> {
        let Etat::Persistance(cle) = std::mem::replace(&mut self.etat, Etat::Ferme) else {
            return Err(protocole("confirmation sans persistance attendue"));
        };
        self.etat = Etat::Termine;
        self.liaison = None;
        self.code_avance = None;
        Ok(vec![
            envoyer("server/pair-finalize", json!({})),
            ActionAppairage::Promouvoir(cle),
        ])
    }

    fn demander_code(&self) -> ActionAppairage {
        ActionAppairage::DemanderCode {
            format: self.methode.format().expect("methode a code"),
            tour: self.tour,
        }
    }

    fn abandonner(&mut self) {
        self.etat = Etat::Abandonne;
        self.code_avance = None;
        self.liaison = None;
    }

    pub fn annuler(&mut self) -> Vec<ActionAppairage> {
        if !self.active() {
            return vec![];
        }
        self.abandonner();
        vec![
            envoyer("pair/abort", json!({"reason":"user_cancelled"})),
            desactiver(),
            ActionAppairage::Abandonne,
        ]
    }

    /// Le serveur annule par activate : attempt_timeout est une raison client.
    pub fn expirer(&mut self, maintenant: Instant) -> Option<Vec<ActionAppairage>> {
        if !self.active() || maintenant < self.echeance {
            return None;
        }
        self.abandonner();
        Some(vec![desactiver(), ActionAppairage::Abandonne])
    }
}

fn protocole(message: &'static str) -> ErreurAppairage {
    ErreurAppairage::Protocole(message)
}
fn envoyer(type_message: &'static str, payload: Value) -> ActionAppairage {
    ActionAppairage::Envoyer {
        type_message,
        payload,
    }
}
fn desactiver() -> ActionAppairage {
    envoyer(
        "server/activate",
        json!({"activities":[],"active_roles":[]}),
    )
}
fn est_message_appairage(t: &str) -> bool {
    t.starts_with("client/pair-") || t == "pair/abort"
}
fn absent(v: &Value, nom: &str) -> Result<(), ErreurAppairage> {
    if v.get(nom).is_some() {
        Err(protocole("champ interdit pour cette methode"))
    } else {
        Ok(())
    }
}
fn champ_binaire<const N: usize>(
    v: &Value,
    nom: &str,
) -> Result<Zeroizing<[u8; N]>, ErreurAppairage> {
    let texte = v
        .get(nom)
        .and_then(Value::as_str)
        .ok_or_else(|| protocole("champ binaire absent"))?;
    if texte.len() != (N * 8).div_ceil(6) {
        return Err(protocole("taille base64url"));
    }
    let octets = Zeroizing::new(depuis_b64url(texte).map_err(|_| protocole("base64url invalide"))?);
    if octets.len() != N || b64url(&octets) != texte {
        return Err(protocole("base64url non canonique"));
    }
    Ok(Zeroizing::new(
        octets.as_slice().try_into().expect("taille verifiee"),
    ))
}

#[cfg(test)]
#[path = "appairage/tests.rs"]
mod tests;
