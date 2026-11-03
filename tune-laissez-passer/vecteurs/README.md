# Vecteurs de test du laissez-passer d'écoute à distance

Ces fichiers sont le contrat entre le site (mozaiklabs.fr, qui signe) et le
relais et le serveur Tune (qui vérifient). Le site rejoue **exactement** ces
fichiers ; `tune-laissez-passer/tests/vecteurs.rs` les rejoue côté Rust.

Ils sont écrits par
`cargo run -p tune-laissez-passer --features fabrique --example generer_vecteurs`.
Les clés de `cles_de_test.json` sont **publiques** : elles ne servent qu'aux
tests.

## Fichiers

| Fichier | Contenu |
|---|---|
| `index.json` | La liste des cas, le fichier des clés, le défi du serveur |
| `cles_de_test.json` | Racine minisign, clé du site (`kid`), appareils Ed25519 et P-256, serveur |
| `jeu_de_cles.json` | Une publication de `GET /api/v1/remote/keys`, signée par la racine de test |
| `jeu_de_cles_autre_racine.json` | Le même jeu, signé par une autre racine (doit être refusé) |
| `<cas>.json` | Un laissez-passer, son contexte, un défi signé, le résultat attendu |
| `defi_serveur.json` | Le défi d'enregistrement du serveur sur le relais |

Un cas :

```json
{
  "nom": "expire",
  "publication": "jeu_de_cles.json",
  "laissez_passer": "<JWS compact>",
  "contexte": {"server_id": "…", "maintenant": 1793000000, "grace_expiration": 0},
  "defi": {"defi": "…", "signature": "<base64url>"},
  "attendu": {"resultat": "refuse", "code": "expire"}
}
```

Pour le rejouer : charger la publication avec `racine_minisign` de
`cles_de_test.json`, vérifier le laissez-passer avec le contexte, puis la
signature du défi par `cnf`. Le premier refus rencontré donne `code`. Un cas
accepté donne `sub` et `jti`.

## Format du laissez-passer

JWS compact, base64url **sans remplissage**, 4 096 octets au plus.

Entête, exactement ces trois champs (tout autre champ est refusé : `format`) :

```json
{"alg": "EdDSA", "typ": "tune-pass+jwt", "kid": "<kid du jeu de clés>"}
```

Charge :

| Champ | Contenu |
|---|---|
| `iss` | `https://mozaiklabs.fr` |
| `aud` | `tune-remote` (chaîne, pas de tableau) |
| `sub` | identifiant opaque du compte, jamais l'adresse électronique |
| `sid` | `server_id` |
| `did` | `device_id` tiré par le site |
| `cnf` | `{"alg": "Ed25519" \| "P-256", "pk": "<base64url>"}` |
| `p2p` | booléen |
| `srv_pk` | facultatif : clé publique Ed25519 du serveur, base64url (32 octets) |
| `iat`, `nbf`, `exp` | secondes Unix ; `0 ≤ exp − iat ≤ 3600`, `nbf ≤ exp` |
| `jti` | identifiant unique |
| `v` | `1` |

Clé d'appareil :

- `Ed25519` : 32 octets ; signature de 64 octets ;
- `P-256` : point SEC1 non compressé de 65 octets (`04 ‖ x ‖ y`, ce que rend
  `crypto.subtle.exportKey("raw")`) ; signature ECDSA SHA-256 brute `r ‖ s`
  de 64 octets (ce que rend `crypto.subtle.sign`).

## Ordre de vérification et codes de refus

1. taille, trois segments, base64url strict, entête JSON → `format`
2. `alg` ≠ `EdDSA` → `alg` (dont `none`, `HS256`, `ES256`)
3. `typ` ≠ `tune-pass+jwt` → `typ`
4. `kid` absent du jeu → `kid_inconnu`
5. signature Ed25519 de `entête.charge` → `signature`
6. charge JSON → `format`
7. `v` → `version` ; `iss` → `emetteur` ; `aud` → `audience` ; `sid` → `serveur`
8. `exp − iat` hors de `[0, 3600]` ou `nbf > exp` → `duree`
9. `nbf` ou `iat` > maintenant + 120 s → `pas_encore_valide`
10. maintenant > `exp` + 120 s + grâce → `expire` (grâce : 0, ou 86 400 s en
    secours quand le site est injoignable)
11. `iat` < `valable_du` − 120 s, ou maintenant > `valable_au` + 120 s →
    `cle_hors_validite`
12. `cnf` illisible → `cle_appareil`
13. signature du défi → `defi`

Jeu de clés : racine absente → `racine_absente` ; signature minisign fausse
ou en mode ancien (non prédigéré) → `jeu_de_cles_signature` ; document
illisible, `alg` autre qu'`EdDSA`, clé de mauvaise taille ou `kid` en double
→ `jeu_de_cles_format`.

## Jeu de clés

`GET /api/v1/remote/keys` rend `{"jeu": "<texte JSON>", "minisig": "<fichier .minisig>"}`.
`jeu` est une **chaîne** : la signature minisign (prédigérée BLAKE2b-512,
celle de `minisign -S`) porte sur ses octets exacts. Son contenu :

```json
{"v": 1, "version": 1, "iss": "https://mozaiklabs.fr",
 "cles": [{"kid": "…", "alg": "EdDSA", "pk": "<base64url>", "valable_du": 0, "valable_au": 0}]}
```

`version` croît à chaque publication ; un relais refuse un jeu plus ancien
que celui qu'il tient.

## Défis

L'appareil signe, en UTF-8 :

```
tune-remote/session/v1\n<server_id>\n<défi>
```

Le serveur, pour s'enregistrer sur le relais, signe avec sa clé Ed25519 :

```
tune-remote/serveur/v1\n<server_id>\n<défi>
```

Pour inscrire sa clé sur le compte (`POST /api/v1/remote/servers/{server_id}/key`,
corps `{"public_key", "alg": "Ed25519", "preuve"}`), le serveur signe :

```
tune-remote/inscription/v1\n<server_id>\n<clé publique base64url>
```

Les trois domaines sont distincts : une signature faite pour l'un ne vaut pas
pour l'autre.
