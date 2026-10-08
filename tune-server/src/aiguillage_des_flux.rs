//! 🔴 #4645 — les flux audio ne dépendent plus de l'exécuteur principal.
//!
//! ## Ce qui prive le darTZeel
//!
//! Le darTZeel LHC-208 d'Yves (tickets 151, 157, 161, 169, 170) tire le WAV au
//! rythme exact de la lecture : quelques secondes d'avance au plus
//! (`avance_max_ms` de 2,4 à 3,2 s dans ses journaux). Une panne de livraison
//! plus longue que ce tampon s'entend : micro-coupure, puis, au-delà d'une
//! vingtaine de secondes sans données, le renderer referme la connexion
//! (`fin="consommateur_parti"`).
//!
//! Deux choses le privent. Le lien réseau du testeur (Mac en Wi-Fi) : la
//! livraison reste sous le nominal (fils 1871, 1892), et ce n'est pas l'affaire
//! de ce module. Et un gel de l'exécuteur tokio de Tune, que ce module traite.
//! Le corps du flux vivait sur le MÊME exécuteur que tout le reste du serveur :
//! la connexion HTTP du renderer, la lecture du fichier temporaire, la réponse
//! à une reprise `Range`. Un gestionnaire qui bloque un fil (lecture SQLite
//! synchrone d'une collection intelligente, #5438) gelait aussi la livraison.
//! Chez Yves, le chien de garde a vu l'exécuteur figé 12 s, et le darTZeel a
//! lâché le flux puis l'a repris par `Range` (fil 2046, #5545). Un gel de
//! 41,6 s est relevé dans le fil 2051 (#5526), et d'autres ailleurs (#4924,
//! #5677).
//!
//! ## Ce que fait ce module
//!
//! Il sépare le TRANSPORT HTTP du TRAVAIL :
//!
//! - un petit moteur tokio à lui, [`FILS_DU_TRANSPORT`] fils, accepte les
//!   connexions, lit les requêtes et écrit les réponses — corps des flux
//!   compris ;
//! - une requête de flux audio ([`est_un_flux_audio`]) est servie sur place,
//!   par ce moteur : session, `Range`, lecture du fichier (son pool bloquant
//!   est le sien) ;
//! - toute autre requête est confiée à l'exécuteur principal, exactement comme
//!   avant ; la connexion attend sa réponse.
//!
//! Un gel de l'exécuteur principal suspend donc l'API, pas l'audio : le
//! renderer reçoit toujours ses octets, et une reprise `Range` demandée
//! pendant le gel reçoit son `206` tout de suite.
//!
//! ⛔ Ce module ne supprime pas les gels eux-mêmes : leurs causes (#5438,
//! #5677) se traitent une à une. Il retire l'audio de leur portée.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::OnceLock;
use std::task::{Context, Poll};

use axum::extract::{ConnectInfo, Request};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::serve::IncomingStream;
use tokio::net::TcpListener;
use tokio::runtime::Handle;
use tower_service::Service;

/// Préfixe des URL que Tune donne aux renderers pour le corps d'une piste
/// (`/stream/<id>.wav`, voir `tune_stream_http::router`).
pub const PREFIXE_DES_FLUX: &str = "/stream/";

/// Fils du moteur de transport. Deux : le travail qui s'y fait est d'E/S
/// (copie fichier → socket), et un second fil couvre un corps compressé ou un
/// gros fichier statique servi en même temps.
pub const FILS_DU_TRANSPORT: usize = 2;

/// Nom des fils du moteur de transport, tel qu'il apparaît dans un relevé de
/// gel ou une pile.
pub const NOM_DES_FILS: &str = "tune-transport";

/// La requête est-elle celle d'un renderer qui tire le corps d'une piste ?
/// Celles-là sont servies sur le moteur de transport, sans passer par
/// l'exécuteur principal.
pub fn est_un_flux_audio(chemin: &str) -> bool {
    chemin.starts_with(PREFIXE_DES_FLUX)
}

static TRANSPORT: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Le moteur de transport du processus, construit au premier appel et jamais
/// détruit (un moteur tokio ne se détruit pas depuis un contexte asynchrone).
fn moteur_du_transport() -> &'static tokio::runtime::Runtime {
    TRANSPORT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(FILS_DU_TRANSPORT)
            .thread_name(NOM_DES_FILS)
            .enable_all()
            .build()
            .expect("construction du moteur de transport HTTP")
    })
}

/// Sert `app` sur `listener` : le transport sur son moteur à lui, le travail
/// sur l'exécuteur appelant. Rend la main quand `arret` est résolu et que les
/// connexions se sont fermées, comme `axum::serve(..).with_graceful_shutdown`.
pub async fn servir<F>(listener: TcpListener, app: axum::Router, arret: F) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    servir_sur(
        moteur_du_transport().handle().clone(),
        Handle::current(),
        listener,
        app,
        arret,
    )
    .await
}

/// [`servir`] avec ses deux moteurs nommés — c'est la forme que les tests
/// éprouvent.
pub async fn servir_sur<F>(
    transport: Handle,
    principal: Handle,
    listener: TcpListener,
    app: axum::Router,
    arret: F,
) -> std::io::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    // L'écoute a été créée sur l'exécuteur principal : elle est inscrite
    // auprès de SON pilote d'E/S, et un gel de celui-ci la rendrait sourde.
    // On la détache, puis on la réinscrit sur le moteur de transport.
    let ecoute = listener.into_std()?;

    // Le signal d'arrêt reste écouté là où il l'était : sur l'exécuteur
    // principal (signaux, repli du WAL). Le transport n'en reçoit que l'issue.
    let (signal, arrete) = tokio::sync::oneshot::channel::<()>();
    let veille_de_l_arret = principal.spawn(async move {
        arret.await;
        let _ = signal.send(());
    });

    let aiguilleur = Aiguilleur { app, principal };
    let service = transport.spawn(async move {
        let ecoute = TcpListener::from_std(ecoute)?;
        axum::serve(ecoute, aiguilleur)
            .with_graceful_shutdown(async move {
                let _ = arrete.await;
            })
            .await
    });
    let issue = match service.await {
        Ok(issue) => issue,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(std::io::Error::other(e)),
    };
    veille_de_l_arret.abort();
    issue
}

/// Fabrique, par connexion, le service qui aiguille ses requêtes.
#[derive(Clone)]
struct Aiguilleur {
    app: axum::Router,
    principal: Handle,
}

impl Service<IncomingStream<'_, TcpListener>> for Aiguilleur {
    type Response = Aiguillage;
    type Error = Infallible;
    type Future = std::future::Ready<Result<Aiguillage, Infallible>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, connexion: IncomingStream<'_, TcpListener>) -> Self::Future {
        std::future::ready(Ok(Aiguillage {
            app: self.app.clone(),
            principal: self.principal.clone(),
            client: *connexion.remote_addr(),
        }))
    }
}

/// Le service d'une connexion : chaque requête part sur le moteur qui lui
/// revient.
#[derive(Clone)]
struct Aiguillage {
    app: axum::Router,
    principal: Handle,
    client: SocketAddr,
}

impl Service<Request> for Aiguillage {
    type Response = Response;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Response, Infallible>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, mut requete: Request) -> Self::Future {
        // Ce que `into_make_service_with_connect_info::<SocketAddr>()` posait :
        // les gestionnaires lisent l'adresse du client (zones navigateur).
        requete.extensions_mut().insert(ConnectInfo(self.client));
        let app = self.app.clone();
        if est_un_flux_audio(requete.uri().path()) {
            return Box::pin(traiter(app, requete));
        }
        let tache = AnnuleeSiAbandonnee(self.principal.spawn(traiter(app, requete)));
        Box::pin(async move {
            let mut tache = tache;
            match (&mut tache.0).await {
                Ok(reponse) => reponse,
                // Même conduite qu'avant : une panique du gestionnaire emporte
                // la connexion, elle n'est pas maquillée en réponse.
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                // L'exécuteur principal s'arrête : la requête n'aura pas de
                // gestionnaire.
                Err(_) => Ok(StatusCode::SERVICE_UNAVAILABLE.into_response()),
            }
        })
    }
}

async fn traiter(mut app: axum::Router, requete: Request) -> Result<Response, Infallible> {
    std::future::poll_fn(|cx| Service::<Request>::poll_ready(&mut app, cx)).await?;
    app.call(requete).await
}

/// Une requête dont le client est parti n'a plus de gestionnaire : hyper
/// abandonne la réponse attendue, et la tâche confiée à l'exécuteur principal
/// est annulée avec elle — comme lorsque le gestionnaire tournait dans la
/// tâche de la connexion.
struct AnnuleeSiAbandonnee(tokio::task::JoinHandle<Result<Response, Infallible>>);

impl Drop for AnnuleeSiAbandonnee {
    fn drop(&mut self) {
        self.0.abort();
    }
}
