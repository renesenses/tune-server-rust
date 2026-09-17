# #3326 — fragmentation du transport, premiere brique S2-c

JP Robbe / OpenAI Codex / jp-robbe-20260916-3326-pairing.

Suite de la meme session, verrou global conserve. Base :
60a1a7da787ff87fa66417831156798594ba3879, lot
batch/jp-sendspin-pairing-20260916. Worktree et target Shrek :
jp-robbe-20260916-3326-fragments. Developpement et validations sur Shrek,
six jobs, priorite reduite, aucun processus d'un autre intervenant arrete.

## Contrat

[Sendspin/spec@cd9330ef](https://github.com/Sendspin/spec/blob/cd9330ef0b4037570c3e2b2d39894ff259bc1487/messaging.md#fragmentation)
est la revision epinglee. Le format actuel emploie le type 1 et un octet
de drapeaux first/last ; les anciennes notes sur 2/3 sont historiques.

Les messages qui tiennent dans une trame gardent leur format. Les autres
sont decoupes avant chiffrement. A la reception, chaque fragment est
authentifie, puis assemble ; aucun corps partiel ne passe au JSON ou au
traitement de role. Le corps est limite localement a 1 Mio. L'allocation
croit par paliers, sans depasser ce plafond de capacite demandee.
Une sequence invalide abandonne le tampon et invalide le recepteur.

Le raccordement couvre hello initial, regime normal, re-echange sur les
anciennes cles et hello sur les nouvelles cles. Le serveur peut traiter une
revocation pendant un message incomplet. Les delais existants de hello et
de re-echange couvrent le message entier. Le regime normal ne gagne pas de
delai d'inactivite ; son tampon reste borne.

## Tests coeur

    cargo test --locked -j6 -p tune-core --no-default-features --features oaat \
      --test sendspin_poignee_s2a

Apres restauration des contre-epreuves : **30 reussites, 4 ignores**.
Les ignores sont les bancs tiers explicites et l'auxiliaire multiprocessus
execute par son parent. Six nouveaux temoins, tous dans les deux suites
Noise, emploient snow directement cote client et des en-tetes de fixture
independants du decoupeur de production :

- limites 65518/65519 octets, plusieurs fragments, emission jusqu'a 1 Mio ;
- ordre et bits de l'en-tete observes apres dechiffrement par snow ;
- UTF-8 coupe entre fragments, livraison unique, message suivant independant ;
- sequence malformee, entrelacement et refus de reprendre apres une erreur ;
- corruption authentifiee avant assemblage ;
- exactement 1 Mio accepte ; premier octet supplementaire refuse.

## Tests HTTP/WebSocket

    cargo test --locked -j6 -p tune-server --no-default-features --features oaat \
      --test sendspin_point_d_acces_s2a

Apres restauration : **22 reussites, 2 ignores**. Trois nouveaux temoins :

- parcours PSK complet dans les deux suites, avec hello de plus de 70 ko,
  messages fragmentes, ping intercale, gestes HTTP, re-echanges PR et LT,
  persistance, reconnexion puis revocation ;
- six sequences malformees par suite : fermeture sans reponse applicative ;
- message laisse incomplet : la revocation ferme sans attendre la fin.

Le test du parcours PSK sans fragmentation reste execute avec le meme
scenario. Le client de fixture encode ses propres en-tetes, sans appeler
le decoupeur teste.

## Contre-epreuves

Les trois fichiers de tests sont inchanges par SHA-256 entre correctif,
sabotage et restauration par cp. Chaque sabotage compile.

1. Dans le decoupeur de production, emettre le type 2 au lieu de 1.
   Meme commande coeur, filtre i3326_fragments : **5 verts / 1 rouge**,
   i3326_fragments_emis_portent_le_type_et_les_drapeaux_du_fil :
   « le type de fragmentation courant est 1 ».
2. Retirer le plafond du reassemblage dans le code de production.
   Meme commande et filtre : **5 verts / 1 rouge**,
   i3326_fragments_bornent_le_corps_et_l_emetteur_avant_chiffrement :
   « le reassemblage doit refuser le premier octet au-dela de 1 Mio ».
3. Rebrancher le hello serveur sur dechiffrer_json, primitive d'une seule
   trame, au lieu du reassembleur. Meme commande serveur, filtre
   i3326_fragments : **2 verts / 1 rouge**,
   i3326_fragments_websocket_hello_regime_et_renouvellements_noise :
   « trame WebSocket: Io(... ConnectionReset ... Connection reset by peer) ».
   Le serveur coupe au lieu d'emettre l'activation attendue apres le hello.

Les journaux de compilation initiaux (unwrap superflu dans le banc, puis
import manquant) sont conserves ; ils ne sont pas des contre-epreuves.

## Validation finale

Apres restauration serveur, les SHA-256 des tests concordent et la suite
complete repasse a 22 verts / 2 ignores. Le banc CPace existant est execute
explicitement, avec le Python epingle de S2-b :

    SENDSPIN_REFERENCE_PYTHON=/srv/builds/jp-research/jp-robbe-20260916-3326-pairing/venv/bin/python3 \
      cargo test --locked -j6 -p tune-server --no-default-features --features oaat \
      --test sendspin_point_d_acces_s2a i3326_cpace_websocket -- --ignored --nocapture

Resultat : **1 test vert, dix scenarios CPace**, en 29,42 s hors compilation.
Ce banc utilise les fixtures et helpers tiers de S2-b ; il ne devient pas
une preuve de lecteur tiers complet ni de fragmentation du SDK.

    cargo clippy --locked -j6 -p tune-core -p tune-server \
      --no-default-features --features oaat \
      --test sendspin_poignee_s2a --test sendspin_point_d_acces_s2a \
      -- -D clippy::correctness
    cargo fmt --all -- --check
    git diff --check

Ces controles reussissent. Clippy conserve des avertissements ; ce resultat
ne signifie pas une compilation sans avertissement. Journaux :
cpace-regression.log et clippy.log. La CI GitHub reste une validation
distincte, avec ci:full pour cette modification du transport entre crates.

## Limites

Cette brique prepare le transport audio : elle n'active aucun role,
n'enregistre aucune sortie et ne cree aucune zone. OutputTarget, codecs,
horodatage des trames audio, decision de pause et ecoute synchronisee sur
deux enceintes restent hors de cette PR. Le plafond local devra etre
confronte aux futurs formats audio negocies.

Les bancs utilisent des fixtures, aucun compte de service musical et aucun
appareil reel. Ils ne prouvent pas l'interoperabilite d'un lecteur tiers
complet ni l'acceptation materielle. Le mode de transition en clair est
inchange. Aucune migration, aucun bump, merge, tag ou deploiement.

## Archives

/srv/builds/jp-evidence/jp-robbe-20260916-3326-fragments/ :
core-restored.log, server-restored.log, counter-type.log, counter-cap.log,
counter-hello.log, tests-before.sha256, tests-restored-core.log,
tests-restored-server.log, scripts de reproduction et specification epinglee.
