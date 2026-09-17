//! Connexion chiffree S2-b : commandes operateur et messages du pair serialises.
use super::ContexteSendspin;
use super::sessions::{
    Commande, ErreurCommande, Inscription, Soumission, decrire_methodes, methodes,
};
use axum::extract::ws::{Message, WebSocket};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tune_core::sendspin::appairage::{ActionAppairage, AppairageServeur, MethodeAppairage};
use tune_core::sendspin::messages::{ClientHello, Enveloppe, EnveloppeBrute, ServerHello};
use tune_core::sendspin::poignee::InfosPair;
use tune_core::sendspin::psk::PskPair;
use tune_core::sendspin::{ErreurSendspin, PoigneeServeur, TransportNoise, registre};

struct Pilote {
    socket: WebSocket,
    transport: TransportNoise,
    infos: InfosPair,
    hello: ClientHello,
    contexte: ContexteSendspin,
    inscription: Inscription,
    appairage: Option<AppairageServeur>,
    methode: Option<MethodeAppairage>,
    compteur: u32,
}
enum Incident {
    Commande(ErreurCommande),
    Connexion(ErreurSendspin),
}
impl From<ErreurSendspin> for Incident {
    fn from(e: ErreurSendspin) -> Self {
        Self::Connexion(e)
    }
}
impl From<ErreurCommande> for Incident {
    fn from(e: ErreurCommande) -> Self {
        Self::Commande(e)
    }
}
fn sequence(message: &'static str) -> ErreurSendspin {
    ErreurSendspin::EtatInattendu(message)
}

pub(super) async fn conduire(
    socket: WebSocket,
    transport: TransportNoise,
    infos: InfosPair,
    hello: ClientHello,
    contexte: ContexteSendspin,
) -> Result<(), ErreurSendspin> {
    let inscription = contexte
        .sessions()
        .ouvrir(&infos, decrire_methodes(&methodes(&hello)))
        .map_err(|_| sequence("registre de connexions indisponible ou client deja connecte"))?;
    contexte.verifier_longue_duree(&infos).await?;
    let mut p = Pilote {
        socket,
        transport,
        infos,
        hello,
        contexte,
        inscription,
        appairage: None,
        methode: None,
        compteur: 0,
    };
    p.boucle().await
}
impl Pilote {
    async fn boucle(&mut self) -> Result<(), ErreurSendspin> {
        enum Evenement {
            Message(Option<Result<Message, axum::Error>>),
            Commande(Option<Soumission>),
            Delai,
            Revoque,
        }
        loop {
            if *self.inscription.revocation.borrow() {
                let _ = self.socket.send(Message::Close(None)).await;
                return Ok(());
            }
            let echeance = self.appairage.as_ref().and_then(AppairageServeur::echeance);
            let ev = tokio::select! {
                _=self.inscription.revocation.changed()=>Evenement::Revoque,
                c=self.inscription.commandes.recv()=>Evenement::Commande(c),
                _=async {
                    if let Some(t)=echeance {tokio::time::sleep_until(t.into()).await;}
                    else {std::future::pending::<()>().await;}
                }=>Evenement::Delai,
                m=self.socket.recv()=>Evenement::Message(m),
            };
            match ev {
                Evenement::Revoque => {
                    let _ = self.socket.send(Message::Close(None)).await;
                    return Ok(());
                }
                Evenement::Commande(Some(s)) => {
                    if s.reponse.is_closed() {
                        continue;
                    }
                    match self.commander(s.commande).await {
                        Ok(()) => {
                            let etat = self
                                .contexte
                                .sessions()
                                .etat(&self.infos.client_id)
                                .unwrap_or(json!({}));
                            let _ = s.reponse.send(Ok(etat));
                        }
                        Err(Incident::Commande(e)) => {
                            let _ = s.reponse.send(Err(e));
                        }
                        Err(Incident::Connexion(e)) => {
                            let _ = s.reponse.send(Err(ErreurCommande::Indisponible));
                            return Err(e);
                        }
                    }
                }
                Evenement::Commande(None) => return Ok(()),
                Evenement::Delai => {
                    if let Some(a) = self
                        .appairage
                        .as_mut()
                        .and_then(|a| a.expirer(Instant::now()))
                    {
                        self.appliquer(a).await?;
                    }
                }
                Evenement::Message(None | Some(Ok(Message::Close(_)))) => return Ok(()),
                Evenement::Message(Some(Err(_))) => return Err(sequence("WebSocket interrompu")),
                Evenement::Message(Some(Ok(Message::Ping(_) | Message::Pong(_)))) => {}
                Evenement::Message(Some(Ok(Message::Text(_)))) => {
                    return Err(sequence("message en clair apres Noise"));
                }
                Evenement::Message(Some(Ok(Message::Binary(b)))) => {
                    let recu = horloge();
                    let texte = self.transport.dechiffrer_json(&b)?;
                    let message: EnveloppeBrute = serde_json::from_str(&texte)
                        .map_err(|_| sequence("JSON chiffre illisible"))?;
                    if !message.payload.is_object() {
                        return Err(sequence("payload chiffre non objet"));
                    }
                    match message.type_message.as_str() {
                        "client/goodbye" => return Ok(()),
                        "client/time" => {
                            let t = message
                                .payload
                                .get("client_transmitted")
                                .and_then(Value::as_i64)
                                .ok_or_else(|| sequence("client/time sans horodatage entier"))?;
                            self.envoyer("server/time",json!({"client_transmitted":t,"server_received":recu,"server_transmitted":horloge()})).await?;
                        }
                        typ if typ.starts_with("client/pair-") || typ == "pair/abort" => {
                            let a = self
                                .appairage
                                .as_mut()
                                .ok_or_else(|| sequence("appairage non active"))?
                                .recevoir(typ, &message.payload, Instant::now())
                                .map_err(|_| sequence("sequence d'appairage invalide"))?;
                            self.appliquer(a).await?;
                        }
                        "noise/handshake" | "client/hello" | "client/init" => {
                            return Err(sequence("handshake non sollicite"));
                        }
                        _ => {} // aucun role actif, aucune lecture
                    }
                }
            }
        }
    }

    async fn commander(&mut self, c: Commande) -> Result<(), Incident> {
        if *self.inscription.revocation.borrow() {
            return Err(ErreurCommande::Conflit("session revoquee").into());
        }
        match c {
            Commande::Demarrer { methode, psk, code } => {
                if self
                    .appairage
                    .as_ref()
                    .is_some_and(|a| a.echeance().is_some())
                {
                    return Err(ErreurCommande::Conflit("appairage deja en cours").into());
                }
                if !methodes(&self.hello).contains(&methode) {
                    return Err(
                        ErreurCommande::Conflit("methode non proposee par ce client").into(),
                    );
                }
                let cle = if methode == MethodeAppairage::Psk {
                    psk.ok_or(ErreurCommande::Invalide("jeton requis"))?
                } else {
                    PskPair::sentinelle()
                };
                if self.infos.categorie_psk != cle.categorie()
                    || self.infos.psk_id != cle.identifiant()
                {
                    self.inscription.publier("negociation", json!({}));
                    self.reechanger(cle).await?;
                    if !methodes(&self.hello).contains(&methode) {
                        return Err(
                            ErreurCommande::Conflit("methode retiree apres renegociation").into(),
                        );
                    }
                }
                let now = Instant::now();
                let (appairage, actions) = if let Some(ancien) = self.appairage.take() {
                    ancien.recommencer(methode, now)
                } else {
                    AppairageServeur::commencer(
                        self.infos.clone(),
                        methode,
                        self.compteur
                            .checked_add(1)
                            .ok_or(ErreurCommande::Conflit("compteur epuise"))?,
                        now,
                    )
                }
                .map_err(|_| ErreurCommande::Conflit("nouvel essai indisponible"))?;
                self.compteur = self
                    .compteur
                    .checked_add(1)
                    .ok_or(ErreurCommande::Conflit("compteur epuise"))?;
                self.appairage = Some(appairage);
                self.methode = Some(methode);
                self.inscription.publier(
                    "attente_client",
                    json!({"method":decrire_methodes(&[methode])[0]}),
                );
                self.appliquer(actions).await?;
                if let Some(code) = code {
                    let actions = self
                        .appairage
                        .as_mut()
                        .ok_or_else(|| sequence("essai absent"))?
                        .saisir_code(&code, now)
                        .map_err(|_| ErreurCommande::Invalide("code invalide"))?;
                    self.appliquer(actions).await?;
                }
            }
            Commande::Saisir(code) => {
                let actions = self
                    .appairage
                    .as_mut()
                    .ok_or(ErreurCommande::Conflit("aucun essai en cours"))?
                    .saisir_code(&code, Instant::now())
                    .map_err(|_| ErreurCommande::Invalide("code invalide ou non attendu"))?;
                self.appliquer(actions).await?;
            }
            Commande::Annuler => {
                if let Some(a) = self.appairage.as_mut() {
                    let actions = a.annuler();
                    self.appliquer(actions).await?;
                }
            }
        }
        Ok(())
    }

    async fn appliquer(&mut self, actions: Vec<ActionAppairage>) -> Result<(), ErreurSendspin> {
        let mut actions: VecDeque<_> = actions.into();
        while let Some(action) = actions.pop_front() {
            if *self.inscription.revocation.borrow() {
                return Err(sequence("session revoquee"));
            }
            match action {
                ActionAppairage::Envoyer {
                    type_message,
                    payload,
                } => {
                    if type_message == "server/pair-auth" {
                        self.inscription.publier("verification", json!({}));
                    }
                    self.envoyer(type_message, payload).await?;
                }
                ActionAppairage::AttendreGeste { message } => {
                    self.inscription.publier(
                        "attente_geste",
                        json!({"device_message":message,"untrusted":true}),
                    );
                }
                ActionAppairage::DemanderCode { format, tour } => {
                    use tune_core::sendspin::pake::FormatCode;
                    let format = match format {
                        FormatCode::Statique => "static_digits",
                        FormatCode::Dynamique => "digits",
                        FormatCode::Qr => "qr_code",
                    };
                    self.inscription
                        .publier("code_attendu", json!({"format":format,"round":tour}));
                }
                ActionAppairage::Persister(cle) => {
                    self.inscription.publier("persistance", json!({}));
                    self.contexte
                        .conserver(
                            &self.infos.client_id,
                            cle,
                            self.methode.ok_or_else(|| sequence("methode absente"))?,
                            self.inscription.revocation.clone(),
                        )
                        .await?;
                    if *self.inscription.revocation.borrow() {
                        return Err(sequence("revocation pendant la persistance"));
                    }
                    let suivants = self
                        .appairage
                        .as_mut()
                        .ok_or_else(|| sequence("essai absent"))?
                        .confirmer_persistance()
                        .map_err(|_| sequence("persistance hors sequence"))?;
                    for a in suivants.into_iter().rev() {
                        actions.push_front(a);
                    }
                }
                ActionAppairage::Promouvoir(cle) => {
                    self.reechanger(cle).await?;
                    self.inscription.publier("appaire", json!({}));
                }
                ActionAppairage::Abandonne => self.inscription.publier("abandonne", json!({})),
                ActionAppairage::Fermer => {
                    let _ = self.socket.send(Message::Close(None)).await;
                    return Err(sequence("appairage concurrent"));
                }
            }
        }
        Ok(())
    }

    async fn envoyer(&mut self, typ: &str, payload: Value) -> Result<(), ErreurSendspin> {
        let texte = serde_json::to_string(&Enveloppe::nouvelle(typ, payload))
            .map_err(|_| sequence("serialisation"))?;
        let b = self.transport.chiffrer_json(&texte)?;
        super::envoyer_binaire(&mut self.socket, b).await
    }

    async fn reechanger(&mut self, cle: PskPair) -> Result<(), ErreurSendspin> {
        let identite = self.contexte.identite().await?;
        let mut poignee = PoigneeServeur::renouveler(&identite, &self.infos, &cle)?;
        let texte = poignee.message_un()?;
        let chiffre = self.transport.chiffrer_json(&texte)?;
        super::envoyer_binaire(&mut self.socket, chiffre).await?;
        let (transport, infos) = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let b = tokio::select! {
                    _ = self.inscription.revocation.changed() => return Err(sequence("session revoquee")),
                    b = super::lire_binaire(&mut self.socket, "re-echange Noise") => b?,
                };
                let (typ, clair) = self.transport.dechiffrer(&b)?;
                if typ != tune_core::sendspin::transport::TYPE_CORPS_JSON {
                    continue;
                }
                let texte =
                    String::from_utf8(clair).map_err(|_| sequence("ancien message non UTF-8"))?;
                let m: EnveloppeBrute = serde_json::from_str(&texte)
                    .map_err(|_| sequence("ancien message JSON invalide"))?;
                if m.type_message == "noise/handshake" {
                    return poignee.message_deux(&texte);
                }
                if !m.payload.is_object() {
                    return Err(sequence("ancien payload invalide"));
                }
            }
        })
        .await
        .map_err(|_| sequence("delai du re-echange"))??;
        if *self.inscription.revocation.borrow() {
            return Err(sequence("revocation pendant le re-echange"));
        }
        self.transport = transport;
        self.infos = infos;
        self.contexte.verifier_longue_duree(&self.infos).await?;
        self.appairage = None;
        self.compteur = 0;
        let hello = ServerHello {
            name: format!("Tune ({})", tune_core::discovery::system_hostname()),
            languages: None,
        };
        self.envoyer(
            "server/hello",
            serde_json::to_value(hello).map_err(|_| sequence("hello serveur"))?,
        )
        .await?;
        let b = tokio::select! {
            _ = self.inscription.revocation.changed() => return Err(sequence("session revoquee")),
            b = super::lire_binaire(&mut self.socket, "client/hello apres re-echange") => b?,
        };
        let texte = self.transport.dechiffrer_json(&b)?;
        let m: EnveloppeBrute =
            serde_json::from_str(&texte).map_err(|_| sequence("hello chiffre invalide"))?;
        if m.type_message != "client/hello" {
            return Err(sequence("client/hello attendu apres re-echange"));
        }
        self.hello = serde_json::from_value(m.payload.clone())
            .map_err(|_| sequence("hello du pair invalide"))?;
        self.inscription
            .confiance(&self.infos, decrire_methodes(&methodes(&self.hello)));
        registre::enregistrer(registre::PairVu {
            client_id: self.infos.client_id.clone(),
            suite: Some(self.infos.suite.to_string()),
            chiffre: true,
            categorie_psk: Some(self.infos.categorie_psk),
            cle_non_reconnue: self.infos.identifiant_perdu,
            nom: self.hello.name.clone(),
            roles: self.hello.supported_roles.clone(),
            player_support: self.hello.support_du_lecteur().cloned(),
            hello_brut: m.payload,
            vu_a: registre::maintenant(),
        });
        if *self.inscription.revocation.borrow() {
            return Err(sequence("revocation pendant le hello"));
        }
        self.envoyer(
            "server/activate",
            json!({"activities":[],"active_roles":[]}),
        )
        .await?;
        self.inscription.publier("disponible", json!({}));
        Ok(())
    }
}
fn horloge() -> u64 {
    static ORIGINE: OnceLock<Instant> = OnceLock::new();
    ORIGINE
        .get_or_init(Instant::now)
        .elapsed()
        .as_micros()
        .min(u64::MAX as u128) as u64
}
