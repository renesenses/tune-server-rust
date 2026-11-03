# Écoute à distance par laissez-passer : contrat entre le relais, le serveur et le site

Référence : #6603. Le format du laissez-passer, du jeu de clés et des défis est
fixé par `tune-laissez-passer/vecteurs/README.md` ; ce document fixe les
échanges.

## App → relais (`tune-bridge`)

| Route | Corps | Réponse |
|---|---|---|
| `GET /session/defi/{server_id}` | — | `{"defi", "expire"}` (60 s, usage unique) ; 404 `serveur_absent` |
| `POST /session/{server_id}` | `{"laissez_passer", "defi", "signature"}` | `{"jeton", "expire", "secours"}` |
| `POST /session/renouveler` | idem, jeton de session en `X-Bridge-Token` | même jeton, `expire` prolongé |

Le jeton de session (256 bits, base64url) s'utilise exactement comme l'ancien
jeton du pont : `X-Bridge-Token` sur l'API, `?token=` pour les flux et le
WebSocket. Il ne change pas au renouvellement.

Refus : JSON `{"code", "motif"?, "appareil"?}`.

| Statut | `code` | Sens |
|---|---|---|
| 401 | un code de refus du laissez-passer (`expire`, `signature`, `defi`…) | voir `vecteurs/README.md` |
| 401 | `non_autorise` | aucun jeton reconnu |
| 401 | `session_coupee` | `motif` : `ecoute_reprise_ailleurs` (avec `appareil`), `revoquee`, ou le motif du site |
| 401 | `session_expiree` | laissez-passer non renouvelé |
| 401 | `lien_obsolete` | ancien lien d'accès, chemin hérité fermé |
| 401 | `autre_appareil` | renouvellement par un autre compte, appareil ou clé |
| 403 | `refuse_par_le_site` | `motif` : celui du site |
| 403 | `revoque` | appareil ou laissez-passer révoqué |
| 503 | `cloud_unavailable` | site injoignable et règle de secours non remplie |
| 503 | `jeu_de_cles_absent` | aucun jeu de clés valable |

Une session coupée ferme ses flux audio en cours, et son WebSocket avec le
code `4001` (raison : le JSON ci-dessus).

Pendant la coexistence, l'ancien jeton du pont reste accepté avec l'en-tête
`X-Tune-Avertissement: lien-obsolete`. `TUNE_BRIDGE_JETON_HERITE=0` le ferme.

## Relais → site (`auth.tune-bridge`, `TUNE_BRIDGE_SERVICE_TOKEN`)

`POST /api/v1/remote/sessions`

```json
{"jti": "…", "sub": "…", "did": "…", "sid": "…", "chemin": "relais", "secours": false}
```

Réponse : `{"ok": true, "session_id": "…", "couper": ["session_id…"], "appareil": "iPhone de …"}`
ou `{"ok": false, "motif": "…"}` (un 4xx portant ce corps est lu comme un
refus ; tout le reste comme une panne). `secours: true` : session ouverte
pendant une panne du site, rejouée à son retour.

`POST /api/v1/remote/heartbeat`, chaque minute s'il y a quelque chose à dire :

```json
{"sessions": [{"session_id": "…", "octets_depuis_le_dernier": 0, "audio_actif": true}],
 "jeton_herite": {"<server_id>": 12}}
```

`jeton_herite` : requêtes servies par l'ancien lien depuis le battement
précédent, par serveur. Aucune IP, aucun titre.

Réponse : `{"couper": [{"session_id", "motif"}], "revoques": {"jti": [], "did": []}, "jeu_de_cles": <version>}`.

`GET /api/v1/remote/keys` (publique) : `{"jeu", "minisig"}`.

`GET /api/v1/remote/servers/{server_id}/key` : `{"public_key": "…"}`, ou 404
si le site n'en connaît pas. Quand une clé est connue, le relais refuse tout
enregistrement de ce `server_id` qui ne la prouve pas.

## Une écoute à distance par compte

Une session compte comme écoute dès son premier `/stream/relay`, et cesse de
compter après 2 min sans audio. Quand une session se met à jouer, le relais
coupe aussitôt les autres sessions du même compte qui jouaient
(`ecoute_reprise_ailleurs`). Le site fait de même entre relais et direct, par
`couper`.

## Secours (site injoignable)

- laissez-passer valide : accepté, ouverture rejouée au retour du site ;
- laissez-passer expiré depuis moins de 24 h, appareil ouvert avec l'accord
  du site depuis moins de 7 jours : accepté, `secours: true` ;
- tout le reste : 503 `cloud_unavailable`.

## Serveur → relais

`relay.register` porte `server_pk` (Ed25519, base64url). Le relais répond
`relay.challenge {"defi"}` ; le serveur répond `relay.challenge_response
{"signature"}` (message `tune-remote/serveur/v1`). `relay.registered` porte
`jeton_herite` (l'ancien lien est-il encore accepté) et `laissez_passer`.

## Serveur → site

`POST /api/v1/remote/servers/{server_id}/key`, jeton OAuth du compte relié :
`{"public_key", "alg": "Ed25519", "preuve"}` (message
`tune-remote/inscription/v1`). La graine privée reste dans le coffre du
serveur (`credentials_vault`, service `ecoute_distante_cle_serveur`).

## Serveur, Réglages

- `GET /cloud/bridge/status` : `acces_par_compte` (texte à afficher),
  `lien_obsolete: true`, `lien_herite_coupe`.
- `GET /cloud/bridge/access-link` : pendant la coexistence, le lien avec
  `obsolete: true` et `message` ; après la coupure annoncée par le relais,
  410 `lien_obsolete`.
- `POST /cloud/bridge/enable` : ne rend plus `bridge_token` après la coupure.

## Chemin direct, pair-à-pair (#6785)

Fonction `ecoute-directe` (hors défaut) de `tune-core` et `tune-server`,
éteinte à l'exécution tant que `TUNE_ECOUTE_DIRECTE=1` ou le réglage
`ecoute_directe_active=true` ne l'allume pas (la variable, posée, l'emporte).
Le relais reste le chemin par défaut et le secours.

### Réseau

- endpoint iroh 1.3.0, preset `Minimal` : ni pkarr, ni DNS d'n0 ;
- identité d'endpoint = clé Ed25519 du serveur (coffre,
  `ecoute_distante_cle_serveur`) = `srv_pk` du laissez-passer ;
- ALPN unique `tune-ecoute/1` ;
- relais iroh : **auto-hébergé seulement**, `TUNE_ECOUTE_DIRECTE_RELAIS` ou
  réglage `ecoute_directe_relais_iroh` ; absent : direct seulement. Jamais
  les relais publics d'n0 ;
- port UDP : `TUNE_ECOUTE_DIRECTE_PORT` / `ecoute_directe_port` (0 : libre) ;
  redirection de port de la box : `TUNE_ECOUTE_DIRECTE_PORTMAPPER=0` la coupe.

`GET /api/v1/cloud/direct/status` (builds avec la fonction) :
`{"active": true, "annonce": {"endpoint_id", "cle_publique", "alpn",
"adresses": ["ip:port"…], "relais_iroh"}}` ou `{"active": false, "allumee"}`.
L'app la lit par sa session du relais avant de tenter le direct.

### App → serveur (flux de contrôle)

Premier flux bidirectionnel de la connexion ; trames = longueur u32
gros-boutienne + JSON (16 Kio au plus), discriminées par `type`.

| Sens | Trame |
|---|---|
| app → serveur | `{"type":"bonjour","v":1}` |
| serveur → app | `{"type":"defi","v":1,"server_id","defi"}` |
| app → serveur | `{"type":"ouvrir","laissez_passer","signature"}` |
| serveur → app | `{"type":"ouvert","expire","secours","defi_suivant"}` ou `{"type":"refus","code","motif"?}` puis fermeture QUIC `4003` |
| app → serveur | `{"type":"renouveler","laissez_passer","signature"}` (signe le `defi_suivant` reçu) |
| serveur → app | `{"type":"renouvele","expire","defi_suivant"}` |
| serveur → app | `{"type":"coupee","motif","appareil"?}` puis fermeture QUIC `4001`, raison `{"code":"session_coupee","motif"}` |

Signature : clé d'appareil (`cnf`) sur
`tune-remote/direct/v1\n<server_id>\n<défi>` (UTF-8) — domaine distinct de
celui du relais. Chaque défi ne sert qu'une fois.

Codes de refus : ceux du laissez-passer (`expire`, `serveur`, `signature`,
`defi`…), plus `p2p_non_permis`, `revoque`, `premium_requis`,
`jeu_de_cles_absent`, `refuse_par_le_site` (avec `motif`), `autre_appareil`
(renouvellement), `version`, `format`.

Règles : laissez-passer vérifié hors ligne avec un jeu signé par la racine
**embarquée** (pas de surcharge), `sid` = ce serveur, `p2p: true`, `srv_pk`
(s'il est là) = la clé de ce serveur, **aucune grâce d'expiration** en direct,
`jti` et `did` révoqués refusés pendant 1 h, droit Premium (`cloud_relay`)
relu à chaque ouverture.

Ensuite, **chaque autre flux bidirectionnel** porte une connexion HTTP/1.1 :
seuls `/api/v1/…` et `/stream/…` (après normalisation du chemin) sont
rejoués en local, avec la liste blanche d'en-têtes du relais (plus
`user-agent`), `x-tune-relais: 1`, `x-tune-appareil-distant` et
`x-tune-appareil-distant-nom`. Tout le reste : 404 sans appel local.

Une écoute : un `/stream/…` ou un `/api/v1/library/tracks/{id}/audio`. Même
politique que le relais : la session qui se met à jouer coupe les autres
sessions directes du même compte qui jouaient (`ecoute_reprise_ailleurs`).

### Serveur → site (à faire côté site)

Requêtes signées par la clé du serveur inscrite sur le compte :

| En-tête | Contenu |
|---|---|
| `X-Tune-Server-Id` | `server_id` |
| `X-Tune-Horodatage` | secondes Unix ; refus au-delà de ± 120 s |
| `X-Tune-Nonce` | 256 bits base64url ; refus d'un nonce déjà vu dans la fenêtre |
| `X-Tune-Signature` | Ed25519, base64url, de `tune-remote/requete-serveur/v1\n<MÉTHODE>\n<chemin>\n<server_id>\n<horodatage>\n<nonce>\n<base64url(SHA-256(corps exact))>` |

Le site vérifie avec la clé de `POST /api/v1/remote/servers/{server_id}/key`,
et que le `server_id` du chemin est celui de l'en-tête. Refus : 401
`{"ok": false, "motif": "signature"}`.

`POST /api/v1/remote/servers/{server_id}/sessions` — ouverture, appel
synchrone :

```json
{"jti": "…", "sub": "…", "did": "…", "sid": "…", "chemin": "direct", "secours": false}
```

Réponse : celle de l'ouverture du relais, `{"ok": true, "session_id",
"couper": [...], "appareil"}` ou `{"ok": false, "motif"}`. Le site vérifie en
plus que `sid` est le serveur signataire, que le compte a toujours le droit
P2P (liste « P2P autorisé ») et applique la limite d'une écoute entre relais
et direct par `couper`. `secours: true` : session ouverte pendant une panne,
rejouée à son retour.

`POST /api/v1/remote/servers/{server_id}/heartbeat`, chaque minute s'il y a
des sessions directes :

```json
{"sessions": [{"session_id": "…", "octets_depuis_le_dernier": 0, "audio_actif": true}]}
```

Réponse : celle du battement du relais, `{"couper": [{"session_id",
"motif"}], "revoques": {"jti": [], "did": []}, "jeu_de_cles": <version>}`. Le
retrait du droit P2P d'un compte se traduit par `couper` sur ses sessions
directes.

Site injoignable : laissez-passer valide accepté (`secours: true`),
ouverture rejouée au battement suivant ; un refus à ce moment coupe la
session. Jamais d'expiré accepté en direct.
