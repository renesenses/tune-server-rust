Tune 1.0.0-rc13.2, wersja poprawkowa rc13.1.

## Poprawki (Fixes)

- Logowanie: hasło konta administratora jest sprawdzane w ten sam sposób na komputerze serwera i w sieci lokalnej; czytelniejszy komunikat o błędzie (nazwa użytkownika: default).
- Jeśli nie możesz się zalogować, użyj nazwy użytkownika default i ostatniego hasła wpisanego na komputerze serwera.
- Konto mozaiklabs.fr można teraz połączyć z dowolnej przeglądarki w sieci lokalnej (iPad, telefon), gdy żadne konto nie jest jeszcze połączone.
- Z sieci lokalnej można też ponownie połączyć to samo konto. Zastąpienie go innym kontem lub odłączenie nadal jest możliwe tylko z komputera serwera lub przez administratora. Nazwa i adres e-mail połączonego konta są widoczne z sieci lokalnej.
- Windows: aktualizacja z poziomu Tune znowu działa. Od rc13 i rc13.1 trzeba było korzystać z instalatora.
- Tune OS w wersji rc12, który już się nie aktualizuje: narzędzie „Débloquer Tune” (Windows, macOS) wznawia aktualizację bez żadnych poleceń. Pobiera się je osobno: https://github.com/renesenses/tune-server-rust/releases/tag/debloquer-tune-v1
