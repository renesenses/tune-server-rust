//! Rendering outbound-HTTP failures so the log says what actually went wrong.

use std::error::Error;
use std::sync::Once;

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

/// `EHOSTUNREACH` — the kernel dropped the SYN instead of putting it on the wire.
const EHOSTUNREACH: &str = "os error 65";
/// `EPERM` — the connection was refused by policy before any packet was built.
const EPERM: &str = "os error 1";

static LOCAL_NETWORK_HINT: Once = Once::new();

/// Warn once per process when a failure to reach a device looks like macOS
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
    if !rendered.contains(EHOSTUNREACH) && !rendered.contains(EPERM) {
        return;
    }
    LOCAL_NETWORK_HINT.call_once(|| {
        warn!(
            "the OS refused this connection before it reached the network. If the device is \
             powered on and answers `curl` from a terminal, macOS is denying tune-server \
             local-network access — restart the server (`brew services restart tune-server`), \
             which is also needed after any upgrade that replaces the binary while it runs. \
             Confirm with: log show --last 5m --info --debug --predicate 'eventMessage CONTAINS \"reason: NECP\"'"
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;
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
