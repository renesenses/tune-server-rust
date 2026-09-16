//! Une boite de commandes bornee par connexion ; aucune PSK dans les vues.
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot, watch};
use tune_core::sendspin::appairage::MethodeAppairage;
use tune_core::sendspin::poignee::InfosPair;
use tune_core::sendspin::psk::{CategoriePsk, PskPair};

pub(super) enum Commande {
    Demarrer {
        methode: MethodeAppairage,
        psk: Option<PskPair>,
        code: Option<String>,
    },
    Saisir(String),
    Annuler,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum ErreurCommande {
    Absent,
    Sature,
    Invalide(&'static str),
    Conflit(&'static str),
    Indisponible,
}

pub(super) struct Soumission {
    pub commande: Commande,
    pub reponse: oneshot::Sender<Result<Value, ErreurCommande>>,
}

struct Entree {
    numero: u64,
    commandes: mpsc::Sender<Soumission>,
    revocation: watch::Sender<bool>,
    etat: Value,
}

#[derive(Default)]
struct Interieur {
    prochain: u64,
    connexions: BTreeMap<String, Entree>,
}
#[derive(Clone, Default)]
pub(super) struct Sessions(Arc<Mutex<Interieur>>);

pub(super) struct Inscription {
    sessions: Sessions,
    id: String,
    numero: u64,
    pub commandes: mpsc::Receiver<Soumission>,
    pub revocation: watch::Receiver<bool>,
}
impl Drop for Inscription {
    fn drop(&mut self) {
        if let Ok(mut interieur) = self.sessions.0.lock()
            && interieur
                .connexions
                .get(&self.id)
                .is_some_and(|e| e.numero == self.numero)
        {
            interieur.connexions.remove(&self.id);
        }
    }
}
impl Inscription {
    pub fn publier(&self, phase: &str, details: Value) {
        if let Ok(mut interieur) = self.sessions.0.lock()
            && let Some(e) = interieur.connexions.get_mut(&self.id)
            && e.numero == self.numero
        {
            e.etat["phase"] = json!(phase);
            e.etat["details"] = details;
        }
    }
    pub fn confiance(&self, infos: &InfosPair, methodes: Value) {
        if let Ok(mut interieur) = self.sessions.0.lock()
            && let Some(e) = interieur.connexions.get_mut(&self.id)
            && e.numero == self.numero
        {
            e.etat["authenticated"] =
                json!(infos.categorie_psk == CategoriePsk::LongueDuree && !infos.identifiant_perdu);
            e.etat["credential_mismatch"] = json!(infos.identifiant_perdu);
            e.etat["methods"] = methodes;
        }
    }
}
impl Sessions {
    pub fn ouvrir(
        &self,
        infos: &InfosPair,
        methodes: Value,
    ) -> Result<Inscription, ErreurCommande> {
        let mut interieur = self.0.lock().map_err(|_| ErreurCommande::Indisponible)?;
        if interieur.connexions.contains_key(&infos.client_id) {
            return Err(ErreurCommande::Conflit("client deja connecte"));
        }
        if interieur.connexions.len() >= 64 {
            return Err(ErreurCommande::Sature);
        }
        let numero = interieur
            .prochain
            .checked_add(1)
            .ok_or(ErreurCommande::Indisponible)?;
        interieur.prochain = numero;
        let (commandes, recepteur) = mpsc::channel(8);
        let (revocation, revoque) = watch::channel(false);
        interieur.connexions.insert(infos.client_id.clone(),Entree{
            numero,commandes,revocation,
            etat:json!({"client_id":infos.client_id,"phase":"disponible","details":{},
                "authenticated":infos.categorie_psk==CategoriePsk::LongueDuree && !infos.identifiant_perdu,
                "credential_mismatch":infos.identifiant_perdu,"methods":methodes}),
        });
        Ok(Inscription {
            sessions: self.clone(),
            id: infos.client_id.clone(),
            numero,
            commandes: recepteur,
            revocation: revoque,
        })
    }
    pub fn etat(&self, id: &str) -> Option<Value> {
        self.0
            .lock()
            .ok()?
            .connexions
            .get(id)
            .map(|e| e.etat.clone())
    }
    pub fn lister(&self) -> Vec<Value> {
        self.0
            .lock()
            .map(|i| i.connexions.values().map(|e| e.etat.clone()).collect())
            .unwrap_or_default()
    }
    pub async fn commander(&self, id: &str, commande: Commande) -> Result<Value, ErreurCommande> {
        let (reponse, recepteur) = oneshot::channel();
        {
            let interieur = self.0.lock().map_err(|_| ErreurCommande::Indisponible)?;
            let entree = interieur.connexions.get(id).ok_or(ErreurCommande::Absent)?;
            if *entree.revocation.borrow() {
                return Err(ErreurCommande::Conflit("session revoquee"));
            }
            entree
                .commandes
                .try_send(Soumission { commande, reponse })
                .map_err(|e| match e {
                    mpsc::error::TrySendError::Full(_) => ErreurCommande::Sature,
                    mpsc::error::TrySendError::Closed(_) => ErreurCommande::Absent,
                })?;
        }
        tokio::time::timeout(std::time::Duration::from_secs(15), recepteur)
            .await
            .map_err(|_| ErreurCommande::Indisponible)?
            .map_err(|_| ErreurCommande::Indisponible)?
    }
    pub fn revoquer(&self, id: &str) {
        if let Ok(interieur) = self.0.lock()
            && let Some(e) = interieur.connexions.get(id)
        {
            e.revocation.send_replace(true);
        }
    }
}

pub(super) fn methodes(
    hello: &tune_core::sendspin::messages::ClientHello,
) -> Vec<MethodeAppairage> {
    let Some(v) = hello
        .supported_pair_methods
        .as_ref()
        .and_then(Value::as_object)
    else {
        return vec![];
    };
    let mut result = vec![];
    if v.get("pairing_psk").is_some_and(Value::is_object) {
        result.push(MethodeAppairage::Psk);
    }
    if !v.contains_key("dynamic_pairing_code")
        && v.get("static_pairing_code").is_some_and(Value::is_object)
    {
        result.push(MethodeAppairage::Statique);
    }
    if let Some(d) = v.get("dynamic_pairing_code").and_then(Value::as_object) {
        let contient = |champ: &str, mot: &str| {
            d.get(champ)
                .and_then(Value::as_array)
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some(mot)))
        };
        let display = contient("out_channels", "display");
        if display || contient("out_channels", "speaker") {
            if contient("formats", "digits") {
                result.push(MethodeAppairage::Dynamique);
            }
            if display && contient("formats", "qr_code") {
                result.push(MethodeAppairage::Qr);
            }
        }
    }
    result
}
pub(super) fn decrire_methodes(methodes: &[MethodeAppairage]) -> Value {
    json!(
        methodes
            .iter()
            .map(|m| match m {
                MethodeAppairage::Psk => json!({"method":"pairing_psk"}),
                MethodeAppairage::Statique => json!({"method":"static_pairing_code"}),
                MethodeAppairage::Dynamique =>
                    json!({"method":"dynamic_pairing_code","format":"digits"}),
                MethodeAppairage::Qr => json!({"method":"dynamic_pairing_code","format":"qr_code"}),
            })
            .collect::<Vec<_>>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tune_core::sendspin::{Identite, Suite};
    fn infos() -> InfosPair {
        InfosPair {
            client_id: Identite::depuis_prive([4; 32]).id(),
            server_id: Identite::depuis_prive([5; 32]).id(),
            suite: Suite::ChaChaPoly,
            psk_id: "test".into(),
            categorie_psk: CategoriePsk::Sentinelle,
            identifiant_perdu: false,
            condensat_poignee: [42; 32],
        }
    }
    #[test]
    fn i3326_sessions_descripteurs_privilegient_le_dynamique_et_ignorent_le_futur() {
        let hello = tune_core::sendspin::messages::ClientHello {
            supported_pair_methods: Some(json!({
                "pairing_psk":{},"static_pairing_code":{},
                "dynamic_pairing_code":{"formats":["future","digits","qr_code"],"out_channels":["speaker","unknown"]},
                "future_method":42
            })),
            ..Default::default()
        };
        assert_eq!(
            methodes(&hello),
            vec![MethodeAppairage::Psk, MethodeAppairage::Dynamique]
        );
        let mut h = hello.clone();
        h.supported_pair_methods.as_mut().unwrap()["dynamic_pairing_code"]["formats"] =
            json!(["future"]);
        assert_eq!(
            methodes(&h),
            vec![MethodeAppairage::Psk],
            "ne pas retomber en statique quand le dynamique est ignore"
        );
    }
    #[tokio::test]
    async fn i3326_sessions_bornent_la_file_et_la_revocation_ne_sature_pas() {
        let s = Sessions::default();
        let infos = infos();
        let mut inscription = s.ouvrir(&infos, json!([])).unwrap();
        for _ in 0..8 {
            let (tx, _rx) = oneshot::channel();
            s.0.lock().unwrap().connexions[&infos.client_id]
                .commandes
                .try_send(Soumission {
                    commande: Commande::Annuler,
                    reponse: tx,
                })
                .unwrap_or_else(|_| panic!("file initiale"));
        }
        assert!(matches!(
            s.commander(&infos.client_id, Commande::Annuler).await,
            Err(ErreurCommande::Sature)
        ));
        s.revoquer(&infos.client_id);
        inscription.revocation.changed().await.unwrap();
        assert!(
            *inscription.revocation.borrow(),
            "la revocation ne doit pas attendre la file pleine"
        );
        assert!(matches!(
            s.commander(&infos.client_id, Commande::Annuler).await,
            Err(ErreurCommande::Conflit(_))
        ));
        drop(inscription);
        assert!(s.etat(&infos.client_id).is_none());
    }
    #[tokio::test]
    async fn i3326_sessions_reponses_et_connexions_sont_isolees() {
        let s = Sessions::default();
        let i = infos();
        let mut a = s.ouvrir(&i, json!([])).unwrap();
        assert!(s.ouvrir(&i, json!([])).is_err());
        let mut autre = i.clone();
        autre.client_id = Identite::depuis_prive([6; 32]).id();
        let b = s.ouvrir(&autre, json!([])).unwrap();
        let demande = s.commander(&i.client_id, Commande::Annuler);
        let reponse = async {
            let v = a.commandes.recv().await.unwrap();
            v.reponse.send(Ok(json!({"accepted":true}))).unwrap();
        };
        let (r, ()) = tokio::join!(demande, reponse);
        assert_eq!(r.unwrap()["accepted"], true);
        drop(a);
        assert!(s.etat(&i.client_id).is_none());
        assert!(s.etat(&autre.client_id).is_some());
        assert!(!*b.revocation.borrow());
    }
}
