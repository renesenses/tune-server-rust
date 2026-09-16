//! Un magasin par instance de routeur, charge a la premiere utilisation.
//! Les I/O passent par spawn_blocking, jamais dans un verrou tenu sur le runtime.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tune_core::sendspin::magasin::{ErreurMagasin, MagasinAppairage};
use tune_core::sendspin::poignee::InfosPair;
use tune_core::sendspin::psk::{CategoriePsk, PskPair};
use tune_core::sendspin::{ErreurSendspin, Identite};

struct Interieur {
    dossier: PathBuf,
    magasin: OnceLock<Result<Mutex<MagasinAppairage>, String>>,
}

#[derive(Clone)]
pub struct ContexteSendspin(Arc<Interieur>);

impl ContexteSendspin {
    pub fn nouveau(dossier: PathBuf) -> Self {
        Self(Arc::new(Interieur {
            dossier,
            magasin: OnceLock::new(),
        }))
    }

    /// Suit le chemin de donnees deja resolu par TuneConfig, y compris quand
    /// la bibliotheque utilise PostgreSQL. Deux bases locales ont deux magasins.
    pub fn pour_base(base: &str) -> Self {
        let mut chemin = Path::new(base).as_os_str().to_os_string();
        chemin.push(".sendspin");
        Self::nouveau(PathBuf::from(chemin))
    }

    async fn avec_magasin<R: Send + 'static>(
        &self,
        action: impl FnOnce(&mut MagasinAppairage) -> Result<R, ErreurMagasin> + Send + 'static,
    ) -> Result<R, ErreurSendspin> {
        let contexte = self.clone();
        tokio::task::spawn_blocking(move || {
            let resultat = contexte.0.magasin.get_or_init(|| {
                MagasinAppairage::ouvrir(&contexte.0.dossier)
                    .map(Mutex::new)
                    .map_err(|e| {
                        tracing::warn!(error = %e, "sendspin_magasin_indisponible");
                        e.to_string()
                    })
            });
            let mutex = resultat.as_ref().map_err(|_| stockage_indisponible())?;
            let mut magasin = mutex.lock().map_err(|_| stockage_indisponible())?;
            action(&mut magasin).map_err(|e| {
                tracing::warn!(error = %e, "sendspin_magasin_operation_echouee");
                stockage_indisponible()
            })
        })
        .await
        .map_err(|_| stockage_indisponible())?
    }

    pub async fn identite(&self) -> Result<Identite, ErreurSendspin> {
        self.avec_magasin(|m| {
            m.lister()?; // refuse aussi un magasin ayant subi une erreur d'ecriture
            Ok(m.identite().clone())
        })
        .await
    }

    pub(super) async fn selectionner(
        &self,
        client_id: &str,
    ) -> Result<(Identite, PskPair), ErreurSendspin> {
        let client_id = client_id.to_owned();
        self.avec_magasin(move |m| {
            let cle = m
                .cle_du_pair(&client_id)?
                .unwrap_or_else(PskPair::sentinelle);
            Ok((m.identite().clone(), cle))
        })
        .await
    }

    pub(super) async fn est_appaire(&self, client_id: &str) -> Result<bool, ErreurSendspin> {
        let client_id = client_id.to_owned();
        self.avec_magasin(move |m| Ok(m.cle_du_pair(&client_id)?.is_some()))
            .await
    }

    pub(super) async fn verifier_longue_duree(
        &self,
        infos: &InfosPair,
    ) -> Result<(), ErreurSendspin> {
        if infos.categorie_psk != CategoriePsk::LongueDuree {
            return Ok(());
        }
        let client_id = infos.client_id.clone();
        let psk_id = infos.psk_id.clone();
        let encore_valide = self
            .avec_magasin(move |m| {
                Ok(m.cle_du_pair(&client_id)?
                    .is_some_and(|p| p.identifiant() == psk_id))
            })
            .await?;
        if !encore_valide {
            return Err(ErreurSendspin::EtatInattendu(
                "appairage revoque pendant la poignee",
            ));
        }
        Ok(())
    }

    pub async fn decrire(&self) -> serde_json::Value {
        match self
            .avec_magasin(|m| Ok((m.identite().id(), m.lister()?)))
            .await
        {
            Ok((id, pairs)) => serde_json::json!({
                "available":true, "server_id":id, "paired_clients":pairs,
            }),
            Err(_) => serde_json::json!({
                "available":false, "server_id":null, "error":"storage_unavailable",
            }),
        }
    }
}

fn stockage_indisponible() -> ErreurSendspin {
    ErreurSendspin::EtatInattendu("magasin d'appairage indisponible")
}
