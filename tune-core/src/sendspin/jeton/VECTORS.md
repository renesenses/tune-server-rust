# Entrees des jetons Sendspin

Les deux constantes V0/V1 du test sont les vecteurs publies dans
Sendspin/spec@8a8b1cbd6764ea116dcaa07e41544a97bc13080c, pairing.md
(Community-Spec-1.0).

reference.json est genere par le script propre a Tune
tune-core/tests/sendspin/generer_vecteurs_jeton.py avec les decodeurs
aiosendspin@b6f8564d07b212d77bfb026b80baa23435d9e591.
Le payload de test est deterministe, sans aucun secret de production.
Les 24 cas comportent 16 succes et 8 refus : tailles inferieures au minimum,
taille exacte et extensions de 1 a 256 octets.

Regeneration sur Shrek, dans le venv epingle de la preuve S2-b :

    python3 tune-core/tests/sendspin/generer_vecteurs_jeton.py /tmp/reference-jeton.json
    cmp tune-core/src/sendspin/jeton/reference.json /tmp/reference-jeton.json

SHA-256 du fichier : e8b3366e61675c374f5ac3d62f7ef299d927fcc9cdba397093b045cb30289996.

Le banc verifie le decodage et les octets remis a CPace ; il ne constitue
pas un parcours d'appairage WebSocket. Les bits de remplissage invalides
sont refuses par data-encoding suivant son decodeur RFC 4648 strict.
