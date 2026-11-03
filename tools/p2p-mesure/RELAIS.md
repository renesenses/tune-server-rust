# Relais iroh auto-hébergé — recette (étape 3)

Recette seulement : **rien n'est déployé**. Le DNS et le déploiement sur le VPS
reviennent à Bertrand, après son go.

Objectif : un `iroh-relay` 1.3.0 (même version que le banc) sur le VPS, servi
sous `relay.mozaiklabs.fr`, à côté de Caddy qui sert déjà
`bridge.mozaiklabs.fr` sur le port 443. Rien ne sort de mozaiklabs ; le relais
ne voit jamais le contenu (QUIC + TLS 1.3 de bout en bout), mais il voit les
adresses IP et les identifiants des deux pairs.

## 1. Compiler (sur Shrek)

```sh
cargo install iroh-relay@1.3.0 --features server --locked --root ~/iroh-relay-1.3.0
# binaire : ~/iroh-relay-1.3.0/bin/iroh-relay   (rust ≥ 1.91)
```

La feature `server` est obligatoire : sans elle, le binaire n'existe pas.
Cette commande a été passée telle quelle le 20/10/2026 en session cloud
(Rust 1.99.1) : `iroh-relay 1.3.0` compile. Cible du VPS : `x86_64-unknown-linux-gnu` ; compiler sur une
distribution dont la glibc n'est pas plus récente que celle du VPS, ou ajouter
`--target x86_64-unknown-linux-musl`.

## 2. Ports

| Port | Protocole | Ouvert sur Internet | Rôle |
|---|---|---|---|
| 443 | TCP | oui (déjà, Caddy) | Caddy termine TLS pour `relay.mozaiklabs.fr` et passe le trafic du relais (WebSocket sur `/relay`, sonde `/ping`, `/generate_204`) |
| 7842 | **UDP** | **oui, à ouvrir** | découverte d'adresse QUIC (QAD) : le relais dit à chaque pair son adresse publique vue de l'extérieur. Sans elle, le perçage du NAT réussit moins souvent |
| 3443 | TCP | non (127.0.0.1) | HTTPS interne du relais, derrière Caddy |
| 3340 | TCP | non (127.0.0.1) | HTTP interne (portail captif) |
| 9090 | TCP | non (127.0.0.1) | métriques Prometheus |

Pare-feu (exemple `ufw`) : `ufw allow 7842/udp`. Rien d'autre à ouvrir : 443/tcp
l'est déjà. Les clients iroh utilisent 7842 par défaut pour la QAD ; ne pas
changer ce port.

## 3. Cohabitation avec Caddy sur le port 443

Deux processus ne peuvent pas écouter le même port : c'est **Caddy** qui garde
le 443, obtient le certificat ACME de `relay.mozaiklabs.fr` comme pour
`bridge.mozaiklabs.fr`, et relaie vers le relais en local. Ajout au Caddyfile :

```caddyfile
relay.mozaiklabs.fr {
	reverse_proxy https://127.0.0.1:3443 {
		transport http {
			tls_server_name relay.mozaiklabs.fr
		}
	}
}
```

`reverse_proxy` passe les WebSocket sans réglage particulier. Le bloc
`bridge.mozaiklabs.fr` existant n'est pas touché.

Pourquoi un HTTPS **interne** plutôt que du HTTP simple derrière Caddy : la QAD
sur UDP 7842 est du QUIC, donc TLS, et Caddy ne la relaie pas. iroh-relay
refuse d'activer la QAD sans section `[tls]` ; il lui faut donc le certificat.
Il réutilise celui que Caddy obtient et renouvelle (mode `Reloading` : relu
périodiquement, pas de redémarrage au renouvellement).

Variante sans QAD (plus simple, moins bonne pour le perçage) : pas de section
`[tls]`, `http_bind_addr = "127.0.0.1:3340"`, `enable_quic_addr_discovery =
false`, et `reverse_proxy 127.0.0.1:3340` dans Caddy ; pas de port UDP à ouvrir.
À ne retenir que si l'accès aux certificats de Caddy pose problème.

## 4. Certificat

Caddy range ses certificats dans son dossier de données. Avec le paquet Debian
(utilisateur `caddy`), **à vérifier sur le VPS** :

```text
/var/lib/caddy/.local/share/caddy/certificates/acme-v02.api.letsencrypt.org-directory/relay.mozaiklabs.fr/relay.mozaiklabs.fr.crt
/var/lib/caddy/.local/share/caddy/certificates/acme-v02.api.letsencrypt.org-directory/relay.mozaiklabs.fr/relay.mozaiklabs.fr.key
```

Le fichier n'existe qu'après la première requête de Caddy sur le nom : déclarer
le DNS et le bloc Caddy **avant** de démarrer le relais. L'utilisateur du relais
doit pouvoir lire ces deux fichiers (groupe `caddy`, ou copie par un service
périodique). Ne pas utiliser le mode `LetsEncrypt` d'iroh-relay : son défi
TLS-ALPN-01 exige le port 443, qui est à Caddy.

## 5. Configuration `/etc/iroh-relay/relay.toml`

```toml
enable_relay = true
http_bind_addr = "127.0.0.1:3340"
enable_quic_addr_discovery = true
enable_metrics = true
metrics_bind_addr = "127.0.0.1:9090"

[tls]
https_bind_addr = "127.0.0.1:3443"
quic_bind_addr = "[::]:7842"
cert_mode = "Reloading"
manual_cert_path = "/var/lib/caddy/.local/share/caddy/certificates/acme-v02.api.letsencrypt.org-directory/relay.mozaiklabs.fr/relay.mozaiklabs.fr.crt"
manual_key_path = "/var/lib/caddy/.local/share/caddy/certificates/acme-v02.api.letsencrypt.org-directory/relay.mozaiklabs.fr/relay.mozaiklabs.fr.key"

# Seuls les endpoints de la campagne passent par le relais : identifiants des
# serveurs de test et des clients (les mêmes que dans autorises.txt).
[access]
allowlist = [
  "5f0c…e1a2",
]

# Bornes de prudence pour la bande passante du VPS (octets par seconde, par client).
[limits.client.rx]
bytes_per_second = 2500000
max_burst_bytes = 5000000
```

La limite de 2,5 Mo/s (20 Mbit/s) laisse passer un flux 24/192 (7 Mbit/s) et
borne l'essai en vrac quand il reste en relais : le `debit_max_mbps` mesuré en
relais est alors celui de cette limite, pas celui du chemin. À noter dans le
rapport, ou à lever pour la durée des essais longs.

`quic_bind_addr = "[::]:7842"` demande un noyau avec IPv6 : sans IPv6, le relais
refuse de démarrer (« Failed to bind QAD listener … Address family not
supported by protocol »). Dans ce cas, écrire `"0.0.0.0:7842"`. Si le VPS a de
l'IPv6, garder `[::]` (double pile sous Linux), sinon les clients en IPv6 n'ont
pas de QAD.

### Ce qui a été vérifié, et ce qui ne l'a pas été

Vérifié le 20/10/2026 en session cloud, avec le binaire compilé ci-dessus, ce
fichier tel quel (chemins du certificat et liste blanche remplacés, QAD sur
`0.0.0.0:7842` faute d'IPv6) et un certificat auto-signé pour
`relay.mozaiklabs.fr` :

- la configuration est acceptée, mode `Reloading` compris ;
- HTTPS interne `127.0.0.1:3443` : TLS 1.3, `/ping` répond 200 ;
- HTTP interne `127.0.0.1:3340` : `/generate_204` répond 204 ;
- métriques `127.0.0.1:9090` : 200 ;
- QAD liée sur UDP 7842 ;
- un essai `p2p-mesure` complet à travers un `iroh-relay` 1.3.0 local en HTTP :
  serveur accroché au relais (ligne `Relais :`), client connecté par le relais
  à 43 ms puis passé en direct à 74 ms.

Non vérifié (pas de Caddy, pas de DNS, pas de VPS ici) : le bloc Caddy, le
chemin réel des certificats de Caddy et leurs droits de lecture, l'ouverture de
7842/udp chez Contabo, un essai par `https://relay.mozaiklabs.fr`.

## 6. Service systemd

```ini
# /etc/systemd/system/iroh-relay.service
[Unit]
Description=iroh-relay (banc p2p-mesure, rc8)
After=network-online.target caddy.service
Wants=network-online.target

[Service]
User=iroh-relay
Group=caddy
ExecStart=/usr/local/bin/iroh-relay --config-path /etc/iroh-relay/relay.toml
Restart=on-failure
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

## 7. Procédure de déploiement (Bertrand)

1. DNS : `relay.mozaiklabs.fr` → IP du VPS (A, et AAAA si le VPS a de l'IPv6).
2. Caddyfile : ajouter le bloc du § 3, `caddy validate`, puis `systemctl reload
   caddy` ; vérifier que le certificat apparaît (§ 4).
3. Copier le binaire dans `/usr/local/bin/`, créer l'utilisateur `iroh-relay`,
   écrire `relay.toml` avec la liste blanche de la campagne.
4. `ufw allow 7842/udp`, puis `systemctl enable --now iroh-relay`.
5. Contrôles :
   - `curl -sS https://relay.mozaiklabs.fr/ping` répond 200 ;
   - `journalctl -u iroh-relay` ne montre pas d'erreur de certificat ;
   - `p2p-mesure serveur … --relais https://relay.mozaiklabs.fr` affiche une
     ligne `Relais : https://relay.mozaiklabs.fr/` (sinon : « relais injoignable ») ;
   - témoin : un serveur `--site vps --box vps` sur le VPS doit donner 100 %
     de direct (cahier § 2.2).
6. Fin de campagne : `systemctl disable --now iroh-relay`, retirer le bloc
   Caddy et refermer 7842/udp.

Bande passante : seuls les essais restés en relais consomment celle du VPS ;
le suivi vnstat du § 7 doit les distinguer du trafic de tune-bridge (fenêtre
des essais, ou compteurs de `/metrics`).
