//! La prochaine piste Tidal de HQPlayer se prépare pendant la courante.
//! Le résultat est partagé avec le lancement explicite : une préparation
//! lente n'est ni annulée par le budget DLNA, ni téléchargée une seconde fois.
use super::*;
use tokio::sync::watch;

#[derive(Clone)]
pub(super) struct Preparation {
    cle: String,
    position: i64,
    ligne: i64,
    demande: PlayRequest,
    empreinte: String,
    resultat: watch::Receiver<Option<Result<ResolvedStream, String>>>,
    invalidee: Arc<std::sync::atomic::AtomicBool>,
    publication: watch::Sender<Option<Result<ResolvedStream, String>>>,
    _bail: Arc<std::sync::Mutex<Option<crate::http::streamer::BailFichierPrepare>>>,
    #[cfg(test)]
    fin: watch::Receiver<bool>,
}

/// Même si la tâche se termine anormalement, aucun appelant ne reste
/// suspendu sur un canal dont le propriétaire de cache garde l'émetteur.
struct PublicationInterrompue(watch::Sender<Option<Result<ResolvedStream, String>>>);
impl Drop for PublicationInterrompue {
    fn drop(&mut self) {
        self.0.send_if_modified(|resultat| {
            if resultat.is_some() {
                return false;
            }
            *resultat = Some(Err("préparation de la piste suivante interrompue".into()));
            true
        });
    }
}

#[cfg(test)]
struct FinTache(watch::Sender<bool>);
#[cfg(test)]
impl Drop for FinTache {
    fn drop(&mut self) {
        self.0.send_replace(true);
    }
}

impl PlaybackOrchestrator {
    #[cfg(test)]
    pub(crate) fn fin_preparation_fichier(&self, zone_id: i64) -> watch::Receiver<bool> {
        self.preparations_fichiers
            .lock()
            .expect("préparations fichier")
            .get(&zone_id)
            .expect("préparation lancée")
            .fin
            .clone()
    }

    fn empreinte_fichier_7247(&self, demande: &PlayRequest) -> String {
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(demande.zone_id)
            .ok()
            .flatten();
        format!(
            "{}|{}|{:?}",
            self.empreinte_du_traitement(demande.zone_id, demande.track_id),
            self.streaming_quality_preference(demande.zone_id),
            zone.as_ref().map(|z| z.max_sample_rate)
        )
    }

    pub(crate) fn fichier_suivant_en_preparation(&self, zone_id: i64) -> bool {
        self.preparations_fichiers
            .lock()
            .expect("préparations fichier")
            .contains_key(&zone_id)
    }

    pub(crate) fn preparation_fichier_indisponible(&self, zone_id: i64) -> bool {
        self.preparations_fichiers
            .lock()
            .expect("préparations fichier")
            .get(&zone_id)
            .is_some_and(|p| !matches!(p.resultat.borrow().as_ref(), Some(Ok(_))))
    }

    pub(super) fn session_fichier_retenue(&self, zone_id: i64) -> Option<String> {
        let preparations = self
            .preparations_fichiers
            .lock()
            .expect("préparations fichier");
        let preparation = preparations.get(&zone_id)?;
        let resultat = preparation.resultat.borrow();
        resultat.as_ref()?.as_ref().ok()?.stream_id.clone()
    }

    pub(crate) async fn nettoyer_preparations_absentes(
        &self,
        etats: &[crate::playback::ZoneState],
    ) {
        let absentes: Vec<i64> = self
            .preparations_fichiers
            .lock()
            .expect("préparations fichier")
            .keys()
            .filter(|id| !etats.iter().any(|e| e.zone_id == **id))
            .copied()
            .collect();
        for zone_id in absentes {
            self.annuler_preparation_fichier(zone_id).await;
        }
    }

    pub(crate) async fn annuler_preparation_fichier(&self, zone_id: i64) {
        let ancienne = self
            .preparations_fichiers
            .lock()
            .expect("préparations fichier")
            .remove(&zone_id);
        if let Some(ancienne) = ancienne {
            let resultat = ancienne.resultat.borrow().clone();
            ancienne
                .invalidee
                .store(true, std::sync::atomic::Ordering::Release);
            ancienne
                .publication
                .send_replace(Some(Err("préparation de la piste suivante annulée".into())));
            if let Some(Ok(resolu)) = resultat
                && let Some(sid) = resolu.stream_id
            {
                self.nettoyer_fichier_prepare_inactif(zone_id, &sid).await;
            }
        }
    }

    async fn nettoyer_fichier_prepare_inactif(&self, zone_id: i64, sid: &str) {
        let courante = self.playback.get_state(zone_id).await;
        let en_lecture = courante
            .now_playing
            .as_ref()
            .and_then(|np| np.stream_id.as_deref())
            == Some(sid);
        let mut sessions = self.gapless_sessions.lock().await;
        if sessions.get(&zone_id).map(String::as_str) == Some(sid) {
            sessions.remove(&zone_id);
        }
        drop(sessions);
        if !en_lecture {
            self.streamer.remove_session(sid).await;
        }
    }

    /// Appelé avant le sondage réseau : cette fonction ne télécharge rien et
    /// ne prend aucun verrou de sortie. Chaque zone a sa propre tâche.
    pub(crate) async fn maintenir_preparation_fichier(
        self: &Arc<Self>,
        etat: &crate::playback::ZoneState,
    ) {
        let zone_id = etat.zone_id;
        if etat.resolving || etat.state == crate::playback::PlayState::Paused {
            return;
        }
        let zone = ZoneRepo::with_backend(self.db.clone())
            .get(zone_id)
            .ok()
            .flatten();
        let cible =
            zone.filter(|z| z.output_type.as_deref() == Some("hqplayer") && z.gapless_enabled);
        let suivant = (etat.state == crate::playback::PlayState::Playing
            && !etat
                .now_playing
                .as_ref()
                .is_some_and(|np| np.source == "radio"))
        .then(|| crate::poller::PositionPoller::prochaine_position_jouable(&self.db, zone_id, etat))
        .flatten();
        let ligne = suivant.and_then(|position| {
            PlayQueueRepo::with_backend(self.db.clone())
                .get_at(zone_id, position)
                .ok()
                .flatten()
                .filter(|e| e.track_id.is_none() && e.source.as_deref() == Some("tidal"))
                .map(|e| (position, e))
        });
        let (Some(zone), Some((position, ligne))) = (cible, ligne) else {
            self.annuler_preparation_fichier(zone_id).await;
            return;
        };
        let demande = PlayRequest {
            zone_id,
            output_device_id: zone.output_device_id,
            source: ligne.source,
            source_id: ligne.source_id,
            title: ligne.title,
            artist_name: ligne.artist_name,
            album_title: ligne.album_title,
            cover_url: ligne.cover_path,
            duration_ms: ligne.duration_ms,
            album_ref: ligne.album_ref,
            ..Default::default()
        };
        let empreinte = self.empreinte_fichier_7247(&demande);
        let cle = format!(
            "{}|{}|{}|{:?}|{}",
            etat.track_generation,
            etat.queue_position,
            ligne.id,
            demande.output_device_id,
            empreinte
        );
        let identique = self
            .preparations_fichiers
            .lock()
            .expect("préparations fichier")
            .get(&zone_id)
            .is_some_and(|p| p.cle == cle);
        if identique {
            return;
        }
        self.annuler_preparation_fichier(zone_id).await;
        let (publication, resultat) = watch::channel(None);
        #[cfg(test)]
        let (publication_fin, fin) = watch::channel(false);
        let publication_de_la_tache = publication.clone();
        let invalidee = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let invalidation_de_la_tache = invalidee.clone();
        let bail = Arc::new(std::sync::Mutex::new(None));
        let bail_de_la_tache = bail.clone();
        let orch = self.clone();
        let a_resoudre = demande.clone();
        tokio::spawn(async move {
            #[cfg(test)]
            let _fin = FinTache(publication_fin);
            let _publication = PublicationInterrompue(publication_de_la_tache.clone());
            if invalidation_de_la_tache.load(std::sync::atomic::Ordering::Acquire) {
                return;
            }
            let _prewarm = orch.begin_levels_prewarm(zone_id);
            let resolu = orch.resolve_stream_frais(&a_resoudre).await;
            if let Ok(ref resolu) = resolu
                && let Some(ref sid) = resolu.stream_id
            {
                let retenu = orch.streamer.retenir_fichier_prepare(sid).await;
                *bail_de_la_tache.lock().expect("bail du fichier préparé") = retenu;
            }
            info!(
                zone_id,
                reussie = resolu.is_ok(),
                "preparation_fichier_terminee_7247"
            );
            // Ne pas détruire une future en plein spawn_blocking : le
            // transcodage finirait sans propriétaire pour nettoyer ses fichiers.
            // L'invalidation retire immédiatement le résultat des appelants ;
            // la tâche termine son opération bornée puis nettoie la session.
            publication_de_la_tache.send_if_modified(|courant| {
                if invalidation_de_la_tache.load(std::sync::atomic::Ordering::Acquire) {
                    return false;
                }
                *courant = Some(resolu.clone());
                true
            });
            if invalidation_de_la_tache.load(std::sync::atomic::Ordering::Acquire)
                && let Ok(resolu) = resolu
                && let Some(sid) = resolu.stream_id
            {
                orch.nettoyer_fichier_prepare_inactif(zone_id, &sid).await;
            }
        });
        self.preparations_fichiers
            .lock()
            .expect("préparations fichier")
            .insert(
                zone_id,
                Preparation {
                    cle,
                    position,
                    ligne: ligne.id,
                    demande,
                    empreinte,
                    resultat,
                    invalidee,
                    publication,
                    _bail: bail,
                    #[cfg(test)]
                    fin,
                },
            );
        info!(zone_id, position, "preparation_fichier_lancee_7247");
    }

    pub(super) async fn reprendre_preparation_fichier(
        &self,
        demande: &PlayRequest,
    ) -> Option<Result<ResolvedStream, String>> {
        if demande.seek_ms.is_some_and(|ms| ms > 0) {
            return None;
        }
        let preparation = self
            .preparations_fichiers
            .lock()
            .expect("préparations fichier")
            .get(&demande.zone_id)
            .cloned()?;
        if demande.source != preparation.demande.source
            || demande.source_id != preparation.demande.source_id
            || demande.track_id != preparation.demande.track_id
            || demande.output_device_id != preparation.demande.output_device_id
            || preparation.empreinte != self.empreinte_fichier_7247(demande)
        {
            return None;
        }
        let ligne_actuelle = PlayQueueRepo::with_backend(self.db.clone())
            .get_at(demande.zone_id, preparation.position)
            .ok()
            .flatten()
            .map(|e| e.id);
        if ligne_actuelle != Some(preparation.ligne) {
            return None;
        }
        let mut resultat = preparation.resultat;
        loop {
            if let Some(resolu) = resultat.borrow().clone() {
                return match resolu {
                    Ok(resolu) => Some(Ok(resolu)),
                    Err(erreur)
                        if preparation
                            .invalidee
                            .load(std::sync::atomic::Ordering::Acquire) =>
                    {
                        Some(Err(erreur))
                    }
                    // Un échec anticipé ne condamne pas la lecture explicite
                    // quand la panne du service a disparu entre-temps.
                    Err(_) => None,
                };
            }
            if resultat.changed().await.is_err() {
                return Some(Err("préparation de la piste suivante annulée".into()));
            }
        }
    }
}

#[cfg(test)]
mod niveaux_tests_7247 {
    use super::*;

    #[tokio::test]
    async fn finir_une_preparation_ne_rattache_pas_les_niveaux_de_sa_remplacante_7247() {
        let db = crate::db::sqlite::SqliteDb::open_in_memory().unwrap();
        db.init_schema().unwrap();
        crate::db::migrations::run_migrations(&db).unwrap();
        let orch = PlaybackOrchestrator::new(
            Arc::new(db),
            Arc::new(PlaybackManager::new()),
            Arc::new(AudioStreamer::new(0)),
            Arc::new(Mutex::new(ServiceRegistry::new())),
            Arc::new(Mutex::new(OutputRegistry::new())),
            None,
        );
        let premiere = orch.begin_levels_prewarm(7247);
        let remplacante = orch.begin_levels_prewarm(7247);
        assert!(!orch.levels_attach_allowed(7247));
        drop(premiere);
        assert!(
            !orch.levels_attach_allowed(7247),
            "finir l'ancienne préparation ne doit jamais dater les niveaux de la suivante avec la piste courante"
        );
        assert!(
            orch.levels_attach_allowed(7248),
            "les autres zones restent indépendantes"
        );
        drop(remplacante);
        assert!(orch.levels_attach_allowed(7247));
    }
}
