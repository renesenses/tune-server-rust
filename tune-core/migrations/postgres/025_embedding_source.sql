-- Provenance d'un embedding CLAP (#1732 phase 1) :
--   NULL                 = analysé sur le fichier lui-même (comportement historique)
--   'inherited:<id>'     = copié depuis la piste <id> (même titre/artiste/durée),
--                          pour les formats exclus de l'analyse (DSD).
ALTER TABLE track_audio_embedding ADD COLUMN IF NOT EXISTS source TEXT;
