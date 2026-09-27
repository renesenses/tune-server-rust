//! #4366 — après un 403 amont sur YouTube : rafraîchir yt-dlp, puis relancer
//! la résolution et la lecture UNE fois.
//!
//! Mesuré le 27/09/2026 : l'URL `googlevideo` rendue par yt-dlp 2026.07.04 est
//! refusée en 403, y compris à yt-dlp lui-même ; celle d'un yt-dlp récent est
//! servie. Le rejeu des en-têtes (#4426) n'y pouvait rien.
//!
//! Le rafraîchissement lui-même (délai de 12 h, binaire utilisateur exclu) vit
//! dans [`crate::ytdlp::rafraichir`] ; ici, seulement le « une seule relance ».

use super::*;
use crate::ytdlp::{Rafraichissement, RaisonRafraichissement};

/// Le refus que le rafraîchissement peut guérir : le statut 403 d'un
/// téléchargement amont (`telecharger_amont` écrit `upstream HTTP 403 …`).
pub(super) fn est_un_refus_403(erreur: &str) -> bool {
    erreur.contains("upstream HTTP 403")
}

/// Essaie, et sur un 403 YouTube guéri par `rafraichir`, essaie UNE seconde
/// fois. Le second résultat est rendu tel quel : un 403 qui persiste remonte
/// avec son message d'origine, sans troisième essai.
pub(super) async fn avec_une_relance_apres_403<T, E, EF, R, RF>(
    service: &str,
    mut essayer: E,
    rafraichir: R,
) -> Result<T, String>
where
    E: FnMut() -> EF,
    EF: std::future::Future<Output = Result<T, String>>,
    R: FnOnce() -> RF,
    RF: std::future::Future<Output = bool>,
{
    match essayer().await {
        Err(e) if service == "youtube" && est_un_refus_403(&e) => {
            if rafraichir().await {
                info!(service, "youtube_relance_apres_rafraichissement_ytdlp");
                essayer().await
            } else {
                Err(e)
            }
        }
        autre => autre,
    }
}

impl PlaybackOrchestrator {
    /// `resolve_streaming_url`, avec la relance de #4366.
    pub(super) async fn resolve_streaming_url_avec_relance(
        &self,
        service_name: &str,
        req: &PlayRequest,
    ) -> Result<ResolvedStream, String> {
        avec_une_relance_apres_403(
            service_name,
            || self.resolve_streaming_url(service_name, req),
            || self.rafraichir_ytdlp_apres_403(req),
        )
        .await
    }

    /// Rafraîchit yt-dlp ; s'il a été remplacé, oublie l'URL refusée et note
    /// la nouvelle version. `true` seulement si une relance a un sens.
    async fn rafraichir_ytdlp_apres_403(&self, req: &PlayRequest) -> bool {
        let Rafraichissement::Fait { nouvelle, .. } =
            crate::ytdlp::rafraichir(RaisonRafraichissement::Refus403).await
        else {
            return false;
        };
        SettingsRepo::with_backend(self.db.clone())
            .set(crate::ytdlp::CLE_VERSION, &nouvelle)
            .ok();
        if let Some(track_id) = req.source_id.as_deref() {
            let registry = self.services.lock().await;
            if let Some(svc) = registry.get("youtube") {
                let svc = svc.read().await;
                if let Some(yt) = svc
                    .as_any()
                    .downcast_ref::<crate::streaming::youtube::YouTubeService>()
                {
                    yt.oublier_url(track_id).await;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const REFUS: &str = "AAC download failed: upstream HTTP 403 Forbidden";

    /// Compte les essais et les rafraîchissements d'un scénario.
    async fn scenario(
        service: &str,
        reponses: &[Result<u8, &str>],
        rafraichi: bool,
    ) -> (Result<u8, String>, usize, usize) {
        let essais = AtomicUsize::new(0);
        let rafraichissements = AtomicUsize::new(0);
        let r = avec_une_relance_apres_403(
            service,
            || {
                let i = essais.fetch_add(1, Ordering::SeqCst);
                let r = reponses[i.min(reponses.len() - 1)].map_err(str::to_string);
                async move { r }
            },
            || {
                rafraichissements.fetch_add(1, Ordering::SeqCst);
                async move { rafraichi }
            },
        )
        .await;
        (r, essais.into_inner(), rafraichissements.into_inner())
    }

    /// Le cas du testeur : 403, yt-dlp rafraîchi, la relance joue.
    #[tokio::test]
    async fn un_403_rafraichi_est_relance_et_joue() {
        let (r, essais, raf) = scenario("youtube", &[Err(REFUS), Ok(7)], true).await;
        assert_eq!((r, essais, raf), (Ok(7), 2, 1));
    }

    /// Une seule relance : si le 403 persiste, l'erreur actuelle remonte après
    /// DEUX essais et UN rafraîchissement, jamais plus.
    ///
    /// Contre-épreuve : remplacer la relance par un appel récursif ou une
    /// boucle — le compteur d'essais dépasse 2.
    #[tokio::test]
    async fn une_seule_relance_si_le_403_persiste() {
        let (r, essais, raf) = scenario("youtube", &[Err(REFUS)], true).await;
        assert_eq!((r, essais, raf), (Err(REFUS.to_string()), 2, 1));
    }

    /// Rafraîchissement refusé (délai de 12 h, binaire utilisateur) ⇒ pas de
    /// relance : elle reprendrait le même binaire, donc le même 403.
    #[tokio::test]
    async fn sans_rafraichissement_pas_de_relance() {
        let (r, essais, raf) = scenario("youtube", &[Err(REFUS), Ok(7)], false).await;
        assert_eq!((r, essais, raf), (Err(REFUS.to_string()), 1, 1));
    }

    /// Ni un autre service, ni une autre erreur ne déclenchent quoi que ce soit.
    #[tokio::test]
    async fn autre_service_ou_autre_erreur_sans_effet() {
        let (r, essais, raf) = scenario("tidal", &[Err(REFUS), Ok(7)], true).await;
        assert_eq!((r, essais, raf), (Err(REFUS.to_string()), 1, 0));
        let autre = "yt-dlp failed for x: Video unavailable";
        let (r, essais, raf) = scenario("youtube", &[Err(autre), Ok(7)], true).await;
        assert_eq!((r, essais, raf), (Err(autre.to_string()), 1, 0));
        let (r, essais, raf) = scenario("youtube", &[Ok(3)], true).await;
        assert_eq!((r, essais, raf), (Ok(3), 1, 0));
    }
}
