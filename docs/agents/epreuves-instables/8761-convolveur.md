# Convolveur #8761 : témoin distinct du mode autorisé

Ce témoin a été extrait de la PR de fuseaux #8912 à la demande de la revue.
Il vérifie le changement 48→96→48 kHz et le layout 2→6 canaux lorsque le
rééchantillonnage est autorisé par défaut. Le test strict déjà dans le lot
conserve toutes ses assertions de refus, d’effacement, de layout et de droit
Premium. Aucun code de production ne change.

Contre-épreuve : forcer avec_reechantillonnage(false) dans
rebuild_local_convolver, sans modifier le test. Le passage à96kHz doit échouer.
Restaurer par copie puis rejouer les deux témoins de changement de format.
