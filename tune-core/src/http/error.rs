//! Rendering outbound-HTTP failures so the log says what actually went wrong.

use std::error::Error;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::warn;

/// Format an error together with every `source()` behind it.
///
/// `reqwest::Error`'s own `Display` stops at
/// `error sending request for url (http://…)` and drops the chain, so the errno
/// never reaches the log. A DLNA renderer that had gone unreachable, one whose
/// port was closed, and one the kernel refused to route to all produced that
/// same sentence — 163k times in one log file, with no way to tell them apart.
pub fn chain(err: &(dyn Error + 'static)) -> String {
    let mut rendered = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        let text = cause.to_string();
        // hyper wraps its own message verbatim at some levels; don't repeat it.
        if !rendered.ends_with(&text) {
            rendered.push_str(": ");
            rendered.push_str(&text);
        }
        source = cause.source();
    }
    rendered
}

/// Display de `hyper::Error::IncompleteMessage` : le pair a fermé la connexion
/// avant d'avoir terminé sa réponse.
pub const CONNECTION_CLOSED_EARLY: &str = "connection closed before message completed";

/// Le renderer a fermé la connexion avant d'avoir fini de répondre.
///
/// C'est la panne typique d'un socket mutualisé : l'appareil a refermé une
/// connexion inactive sans l'annoncer, la requête suivante part sur ce socket
/// mort, et hyper rend `IncompleteMessage`.
///
/// Ce prédicat existe parce que `reqwest` ne classe ce cas **ni** en
/// `is_connect()` — la connexion, elle, avait bien été établie — **ni** en
/// `is_timeout()` : rien n'a expiré, le pair a raccroché. Un code qui ne
/// réessaie que sur ces deux prédicats laisse donc passer cette panne-là sans
/// la moindre seconde tentative. Voir le test
/// `un_socket_ferme_net_n_est_ni_connect_ni_timeout`, qui le vérifie face à un
/// vrai socket plutôt que sur parole.
///
/// La détection se fait sur le texte de la chaîne d'erreurs, comme
/// [`hint_if_local_network_denied`] : `hyper` n'est pas une dépendance directe
/// de ce workspace, et le type d'erreur concret dépend de la version que
/// `reqwest` embarque.
pub fn is_connection_closed_early(err: &(dyn Error + 'static)) -> bool {
    chain(err).contains(CONNECTION_CLOSED_EARLY)
}

/// Le pair a **refusé** la connexion : le port n'écoute plus.
///
/// C'est la signature d'un renderer dont la pile UPnP a redémarré sur un
/// autre port — Platinum en tire un au hasard à chaque démarrage (#3829 :
/// 1145 → 1838 sur le même appareil, même UDN). Elle se distingue du timeout
/// (appareil éteint, SYN sans réponse) et du `IncompleteMessage` (le pair a
/// accepté puis raccroché) : ici l'hôte est joignable et répond `RST`.
///
/// La détection porte sur [`std::io::ErrorKind::ConnectionRefused`], lu dans
/// la chaîne des `source()`, et JAMAIS sur le texte : le libellé est
/// localisé par le système — « Aucune connexion n'a pu être établie car
/// l'ordinateur cible l'a expressément refusée. (os error 10061) » sous un
/// Windows français, `Connection refused (os error 111)` sous Linux,
/// `os error 61` sous macOS. La bibliothèque standard replie ces trois
/// numéros sur le même `kind()`, et c'est lui qui fait foi.
pub fn is_connection_refused(err: &(dyn Error + 'static)) -> bool {
    let mut courant: Option<&(dyn Error + 'static)> = Some(err);
    while let Some(e) = courant {
        if let Some(io) = e.downcast_ref::<std::io::Error>()
            && io.kind() == std::io::ErrorKind::ConnectionRefused
        {
            return true;
        }
        courant = e.source();
    }
    false
}
/// `EHOSTUNREACH` — the kernel dropped the SYN instead of putting it on the wire.
const EHOSTUNREACH: &str = "os error 65";
/// `EPERM` — the connection was refused by policy before any packet was built.
const EPERM: &str = "os error 1";

/// Dernière émission de l'indice « permission réseau local » (#4597).
static LOCAL_NETWORK_HINT: Mutex<Option<Instant>> = Mutex::new(None);

/// #4597 — combien de connexions vers le LAN macOS a refusées depuis le
/// démarrage (`os error 65` / `os error 1`), et quand pour la dernière fois.
static REFUS_RESEAU_LOCAL: AtomicU64 = AtomicU64::new(0);
static DERNIER_REFUS_RESEAU_LOCAL: AtomicU64 = AtomicU64::new(0);

/// Le motif d'erreur que macOS rend quand il refuse le réseau local.
pub fn ressemble_a_un_refus_du_reseau_local(rendered: &str) -> bool {
    rendered.contains(EHOSTUNREACH) || rendered.contains(EPERM)
}

/// #4597 — `(nombre de refus, horodatage Unix du dernier)` depuis le démarrage.
/// Toujours `(0, None)` hors macOS. Reste un INDICE : `EHOSTUNREACH` dit aussi
/// « appareil éteint ».
pub fn refus_du_reseau_local() -> (u64, Option<u64>) {
    let n = REFUS_RESEAU_LOCAL.load(Ordering::Relaxed);
    let dernier = DERNIER_REFUS_RESEAU_LOCAL.load(Ordering::Relaxed);
    (n, (n > 0 && dernier > 0).then_some(dernier))
}

/// #4597 — l'indice se RÉARME passé ce délai.
///
/// Il était sous un `Once` : une fois par PROCESSUS. Un serveur qui tourne
/// depuis des heures l'avait brûlé bien avant que quelqu'un exporte un
/// journal, c'est-à-dire au seul moment où il est lu — le journal de Cyrille
/// (fil 1861, 0.9.158) porte 31 `No route to host (os error 65)` en une heure
/// et zéro indice. Quinze minutes bornent le bruit (quatre lignes par heure
/// au pire, pendant une panne qui en produit des dizaines) et garantissent
/// qu'un export pris pendant l'incident le contient.
const LOCAL_NETWORK_HINT_REARM: Duration = Duration::from_secs(15 * 60);

/// La décision, isolée de l'horloge et de la plateforme : faut-il émettre
/// l'indice maintenant ? Oui la première fois, puis à nouveau dès que
/// `rearm` s'est écoulé depuis la dernière émission. Met l'état à jour
/// quand elle répond oui.
fn indice_du(derniere: &mut Option<Instant>, maintenant: Instant, rearm: Duration) -> bool {
    let du = match *derniere {
        None => true,
        Some(t) => maintenant.saturating_duration_since(t) >= rearm,
    };
    if du {
        *derniere = Some(maintenant);
    }
    du
}

/// Warn — at most once per [`LOCAL_NETWORK_HINT_REARM`] — when a failure to
/// reach a device looks like macOS
/// denying local-network access rather than the device being at fault.
///
/// macOS keys that permission to the binary's code identity. An ad-hoc signed
/// build gets a new identity on every release, and replacing the binary under a
/// running server invalidates the grant in flight — after which the kernel drops
/// every connection to the LAN (`tcp drop outgoing … reason: NECP`) while
/// traffic to the internet keeps flowing, so the server looks healthy from the
/// outside. Only a restart clears it.
///
/// `EHOSTUNREACH` also means "device is off" often enough that this stays a
/// hint: it points at the one-command check rather than asserting a cause.
pub fn hint_if_local_network_denied(rendered: &str) {
    if !cfg!(target_os = "macos") {
        return;
    }
    if !ressemble_a_un_refus_du_reseau_local(rendered) {
        return;
    }
    // #4597 — le journal seul ne suffisait pas : l'utilisateur ne voit que des
    // zones hors ligne. On COMPTE, pour que l'API puisse le dire à l'écran.
    REFUS_RESEAU_LOCAL.fetch_add(1, Ordering::Relaxed);
    let maintenant = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    DERNIER_REFUS_RESEAU_LOCAL.store(maintenant, Ordering::Relaxed);
    let du = LOCAL_NETWORK_HINT
        .lock()
        .map(|mut derniere| indice_du(&mut derniere, Instant::now(), LOCAL_NETWORK_HINT_REARM))
        .unwrap_or(false);
    if du {
        warn!(
            "the OS refused this connection before it reached the network. If the device is \
             powered on and answers `curl` from a terminal, macOS is denying tune-server \
             local-network access — restart the server (`brew services restart tune-server`), \
             which is also needed after any upgrade that replaces the binary while it runs. \
             Confirm with: log show --last 5m --info --debug --predicate 'eventMessage CONTAINS \"reason: NECP\"'"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #4597 — deux rafales d'`EHOSTUNREACH` séparées par une pause, sur un
    /// même processus : la seconde produit ELLE AUSSI l'indice.
    ///
    /// Rouge attendu avec un `Once` : la seconde rafale est muette, et c'est
    /// elle que l'export de journal contient.
    #[test]
    fn une_seconde_rafale_apres_une_pause_reemet_l_indice_4597() {
        let rearm = LOCAL_NETWORK_HINT_REARM;
        let t0 = Instant::now();
        let mut derniere = None;
        // Première rafale : une émission, puis silence dans la rafale.
        assert!(indice_du(&mut derniere, t0, rearm), "première occurrence");
        for s in 1..60 {
            assert!(
                !indice_du(&mut derniere, t0 + Duration::from_secs(s), rearm),
                "dans la même rafale, l'indice ne se répète pas ({s} s)"
            );
        }
        // Seconde rafale, une heure plus tard.
        let t1 = t0 + Duration::from_secs(3_600);
        assert!(
            indice_du(&mut derniere, t1, rearm),
            "une heure plus tard, la seconde rafale doit porter l'indice — \
             sinon l'export pris pendant l'incident ne le contient pas (#4597)"
        );
        assert!(!indice_du(
            &mut derniere,
            t1 + Duration::from_secs(1),
            rearm
        ));
    }

    /// Garde de BRANCHEMENT : l'indice de production passe par `indice_du`,
    /// pas par un `Once` — sinon les deux témoins ci-dessus resteraient verts
    /// sur une règle que plus personne n'appelle.
    #[test]
    fn l_indice_de_production_passe_par_la_regle_rearmable_4597() {
        let src = include_str!("error.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap();
        let debut = prod
            .find("pub fn hint_if_local_network_denied(")
            .expect("hint_if_local_network_denied");
        let corps = &prod[debut..];
        let corps = &corps[..corps.find("\n}\n").expect("fin du corps")];
        assert!(
            corps.contains("indice_du("),
            "l'indice doit passer par indice_du"
        );
        assert!(
            !corps.contains("call_once"),
            "plus de Once par processus (#4597)"
        );
    }

    /// La borne du bruit : jamais plus d'une émission par délai de réarmement,
    /// même sous une rafale continue d'une heure (une par seconde).
    #[test]
    fn l_indice_reste_borne_sous_une_rafale_continue_4597() {
        let rearm = LOCAL_NETWORK_HINT_REARM;
        let t0 = Instant::now();
        let mut derniere = None;
        let emissions = (0..3_600u64)
            .filter(|s| indice_du(&mut derniere, t0 + Duration::from_secs(*s), rearm))
            .count();
        assert_eq!(
            emissions, 4,
            "une heure à 15 min de réarmement : quatre lignes"
        );
    }
    use std::fmt;

    #[derive(Debug)]
    struct Layer(&'static str, Option<Box<Layer>>);

    impl fmt::Display for Layer {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(self.0)
        }
    }

    impl Error for Layer {
        fn source(&self) -> Option<&(dyn Error + 'static)> {
            self.1.as_deref().map(|l| l as &(dyn Error + 'static))
        }
    }

    #[test]
    fn chain_appends_every_source() {
        let err = Layer(
            "error sending request",
            Some(Box::new(Layer(
                "tcp connect error",
                Some(Box::new(Layer("No route to host (os error 65)", None))),
            ))),
        );
        assert_eq!(
            chain(&err),
            "error sending request: tcp connect error: No route to host (os error 65)"
        );
    }

    #[test]
    fn chain_does_not_repeat_a_source_the_parent_already_quoted() {
        let err = Layer(
            "outer: inner detail",
            Some(Box::new(Layer("inner detail", None))),
        );
        assert_eq!(chain(&err), "outer: inner detail");
    }

    #[test]
    fn chain_of_a_lone_error_is_its_display() {
        assert_eq!(chain(&Layer("just this", None)), "just this");
    }

    /// Le fait porteur de #1984 : quand le pair accepte, lit la requête puis
    /// raccroche sans répondre, `reqwest` rend une erreur qui n'est **ni**
    /// `is_connect()` **ni** `is_timeout()`.
    ///
    /// C'est ce qui faisait échouer le SOAP du Marantz ND8006 sans la moindre
    /// nouvelle tentative : la garde de `soap_action` ne connaissait que ces
    /// deux prédicats. Le test tient le fait face à un vrai socket, pour que la
    /// montée de version de `reqwest`/`hyper` qui changerait cette
    /// classification tombe ici plutôt que chez un utilisateur.
    /// #3829 — un port qui n'écoute plus se reconnaît à son `kind()`, pas à
    /// son libellé.
    ///
    /// La couche du bas porte un `io::Error` de `kind` `ConnectionRefused`
    /// dont le TEXTE est celui d'un Windows français (10061). Une détection
    /// par chaîne de caractères sur `connection refused` resterait muette ;
    /// celle-ci ne lit que le `kind`.
    #[test]
    fn un_refus_de_connexion_se_reconnait_au_kind_meme_en_francais_windows() {
        #[derive(Debug)]
        struct Enveloppe(std::io::Error);
        impl fmt::Display for Enveloppe {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("tcp connect error")
            }
        }
        impl Error for Enveloppe {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&self.0)
            }
        }
        let windows_fr = std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "Aucune connexion n'a pu être établie car l'ordinateur cible l'a \
             expressément refusée. (os error 10061)",
        );
        let err = Layer(
            "error sending request for url (http://192.168.1.17:1145/AVTransport/control.xml)",
            None,
        );
        // Une couche « reqwest », une couche « connect », l'`io::Error` au fond.
        struct Haut(Layer, Enveloppe);
        impl fmt::Debug for Haut {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "Haut")
            }
        }
        impl fmt::Display for Haut {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0, f)
            }
        }
        impl Error for Haut {
            fn source(&self) -> Option<&(dyn Error + 'static)> {
                Some(&self.1)
            }
        }
        let haut = Haut(err, Enveloppe(windows_fr));
        assert!(
            !chain(&haut).to_lowercase().contains("connection refused"),
            "le banc doit porter un libellé SANS le mot anglais : {}",
            chain(&haut)
        );
        assert!(
            is_connection_refused(&haut),
            "prédicat muet sur {}",
            chain(&haut)
        );
        // Contre-épreuve : un timeout, ou un autre `kind`, n'est pas un refus.
        let timeout = Haut(
            Layer("error sending request", None),
            Enveloppe(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Connection refused (os error 111)",
            )),
        );
        assert!(
            !is_connection_refused(&timeout),
            "le texte dit « refused » mais le kind dit timeout : le kind fait foi"
        );
    }
    /// #3829, face à un VRAI socket : ce que `reqwest` rend quand le port ne
    /// répond plus est bien reconnu par [`is_connection_refused`] — la chaîne
    /// des `source()` descend jusqu'à l'`io::Error`. C'est ce qui permet à la
    /// redécouverte DLNA de ne pas se déclencher sur un timeout.
    #[tokio::test]
    async fn un_port_qui_n_ecoute_plus_est_un_refus_de_connexion() {
        // On prend un port libre, puis on le relâche : rien n'y écoute.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let err = crate::http::client::shared()
            .post(format!("http://127.0.0.1:{port}/AVTransport/control.xml"))
            .body("<s:Envelope/>")
            .send()
            .await
            .expect_err("personne n'écoute : la requête ne peut pas réussir");
        let rendu = chain(&err);
        assert!(is_connection_refused(&err), "prédicat muet sur {rendu}");
        assert!(
            !err.is_timeout(),
            "reqwest classerait ça en timeout : {rendu}"
        );
        assert!(
            !is_connection_closed_early(&err),
            "un refus n'est pas un raccroché : {rendu}"
        );
    }
    #[tokio::test]
    async fn un_socket_ferme_net_n_est_ni_connect_ni_timeout() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            // Lire la requête, puis raccrocher sans écrire un seul octet de
            // réponse — exactement ce que fait une pile HTTP embarquée qui a
            // refermé un socket inactif.
            let mut buf = [0u8; 1024];
            let _ = sock.read(&mut buf).await;
            drop(sock);
        });

        // Client partage, y compris ici : le garde-fou de `http_client_seam`
        // interdit d'en construire un a la main, meme dans un test. Un client
        // nu utilise le verificateur TLS de plateforme, que la build FFI
        // Android n'initialise pas.
        let err = crate::http::client::shared()
            .post(format!("http://127.0.0.1:{port}/upnp/control"))
            .body("<s:Envelope/>")
            .send()
            .await
            .expect_err("le pair a raccroché : la requête ne peut pas réussir");

        let rendu = chain(&err);
        assert!(
            rendu.contains(CONNECTION_CLOSED_EARLY),
            "erreur inattendue : {rendu}"
        );
        assert!(
            !err.is_connect(),
            "reqwest classerait ça en connect : {rendu}"
        );
        assert!(
            !err.is_timeout(),
            "reqwest classerait ça en timeout : {rendu}"
        );
        assert!(
            is_connection_closed_early(&err),
            "prédicat muet sur {rendu}"
        );

        server.await.unwrap();
    }
}
