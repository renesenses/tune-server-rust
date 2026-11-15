# ReplayGain Qobuz et TIDAL

L'option « ReplayGain pour Qobuz et TIDAL » des réglages audio est globale et désactivée par défaut. Elle utilise le mode piste ou album du ReplayGain général, son pré-gain et sa protection d'écrêtage. Si le mode général est désactivé, aucun gain n'est appliqué. PURE et le bit-perfect strict contournent cette correction.

La bascule web n'apparaît que lorsque le serveur annonce le réglage. Pour les clients API, GET /api/v1/system/config expose replaygain_services_enabled (booléen) et PATCH /api/v1/system/config accepte cette même clé. La désactivation conserve les autres réglages ReplayGain.

Les mesures viennent des réponses du service : audio_info chez Qobuz, normalisation de lecture chez TIDAL. Une mesure absente laisse le facteur à 1 ; le gain d'album se replie sur le gain de piste si nécessaire. Ces données restent en mémoire, sans écrire dans les balises. Les pistes de bibliothèque conservent la priorité de leurs mesures locales.

Après un redémarrage, Qobuz peut nécessiter une lecture des métadonnées du titre. Elle est effectuée uniquement lorsque l'option et le ReplayGain sont actifs, hors PURE et bit-perfect strict, avec un délai maximal de trois secondes. Un échec de cette lecture n'empêche pas la lecture musicale.

La validation synthétique couvre le choix volontaire, le pré-gain, l'anti-écrêtage, les échantillons PCM et le chemin du signal. Une réponse Qobuz réelle a confirmé la présence de mesures numériques de gain et de pic ; un témoin reprend ces valeurs pour vérifier le pré-gain et la protection d’écrêtage. La réponse TIDAL réelle et le parcours complet sur le serveur de recette restent à vérifier, sans sortie audio audible.
