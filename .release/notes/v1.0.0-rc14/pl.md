Tune 1.0.0-rc14 zawiera całą zawartość wersji przedpremierowych rc13.90 i rc13.91; te notatki opisują zmiany od rc13.91. Dziękujemy testerom: wiele z tych punktów pochodzi z Waszych uwag.

## Nowości (New features)

- **TESS, presety Zapisz i Wczytaj ponownie**: gdy tylko ustawienie odbiega od wybranego presetu, TESS wyświetla „zmieniony”. **Zapisz** nadpisuje pierwotny preset, **Wczytaj ponownie** przywraca go. Presetu dostarczonego z Tune, takiego jak „Pure”, nie można nadpisać: „Zapisz jako…” tworzy kopię.
- **TESS, preset DSD stosowany od razu**: preset zmieniający cel DSD (na przykład DSD128 → DSD256), wczytany w trybie „zastosuj natychmiast”, wznawia utwór od bieżącej pozycji, zamiast czekać na następny utwór. Upsampling jest funkcją Premium.
- **Zapas (headroom)**: Automatycznie lub ręcznie od −12 do 0 dB w krokach co 0,5 dB. Panel pokazuje wartość rzeczywiście zastosowaną albo podaje, dlaczego nie jest stosowana.
- **Zapas przy upsamplingu do DSD**: ręczny zapas nie jest już stosowany dwukrotnie (−12 dB dawało −24 dB, teraz −15 dB, w tym −3 dB dla modulatora DSD, pokazane w osobnym wierszu).
- **Pomiar pomieszczenia prowadzony do 75 dB(C)**: z kalibrowanym mikrofonem ekran pokazuje zmierzony poziom, jego odchylenie od 75 dB(C) i wskazówkę dotyczącą głośności. Reguła jest ostrożna, ponieważ przeliczenia nie sprawdzono jeszcze kalibratorem: Tune nigdy nie zaleca przekraczania szacowanych 80 dB(C) i wycisza sygnał przy 90 dB SPL. Korekcja mierzona mikrofonem jest funkcją Premium; korekcja obliczana bez mikrofonu pozostaje bezpłatna.
- **Ochrona słuchu**: odznaczenie korektora, przejście na PURE lub Bypass all albo usunięcie splotu, crossfeedu lub miksowania do mono nie powoduje już nagłego skoku poziomu: wraca on do wcześniejszej wartości płynnie, w ciągu około pół sekundy. Jeśli w międzyczasie zwiększysz głośność, zapala się wskaźnik „poziom obniżony”, a „Przywróć poziom” zwalnia nadwyżkę. Dotyczy to także wyjść sieciowych (UPnP, OAAT).
- **Wierna ścieżka sygnału**:
  - na wyjściu sieciowym odpowiada rzeczywistej kolejności strumienia, aż do głośności odtwarzacza;
  - etap filtra odpowiedzi impulsowej nazywa się „Splot”;
  - filtr zachowuje nazwę zaimportowanego pliku .wav lub .zip;
  - po zmianie celu DSD pokazuje rzeczywiście odtwarzaną częstotliwość DSD i zapowiada następną;
  - okno staje się zielone: kolor pomarańczowy jest zarezerwowany dla prawdziwych ostrzeżeń.
- **Splot**: panel pokazuje krzywą wczytanego filtra, kanał po kanale, a na żądanie także fazę.
- **Bypass all**: ikona TESS na pasku odtwarzania jest przygaszona, gdy TESS jest pominięty, a strona TESS wyświetla „TESS pominięty”.
- **Ujednolicona strona TESS**: akcent w zieleni TESS, bieżąca strefa obok plakietki, panel Headroom wyszarzony na wyjściu sieciowym oraz pomiar „Bez mikrofonu (szacunek)” lub „Z mikrofonem (zalecane)”.
- **Wyjście sieciowe**: karta Nadpróbkowanie informuje, że upsampling tam nie działa.
- **Ujednolicony interfejs**: wbudowane czcionki, także bez połączenia z internetem; ekrany Biblioteka, Live Radio, Serwery multimediów i Koncerty mają nowy wygląd.

## Poprawki (Fixes)

- **Radio na Sonosie** (i na każdym odtwarzaczu sieciowym z limitem częstotliwości): stacja radiowa 192 kHz jest podawana z częstotliwością akceptowaną przez odtwarzacz. Wcześniej odtwarzacz od razu się zatrzymywał, raz za razem.
- **768 kHz**: lista „Maks. częstotliwość” strefy oferuje 768 kHz i 1536 kHz.
- **Biblioteka**: siatka albumów nie zawiesza się już podczas przewijania dużej kolekcji.
- **Odtwarzanie w karcie przeglądarki**: przewijanie rzeczywiście przesuwa odtwarzanie, także z innego urządzenia.
- **Pomoc**: baner priorytetowej pomocy technicznej nie wyświetla się już na koncie Premium.
- **Licencja**: data wygaśnięcia jest odczytywana poprawnie, a Ustawienia › Licencja podają, dlaczego poziom jest bezpłatny.
- **Splot**: filtr wzmacniający niektóre częstotliwości nie przesterowuje już sygnału.
- **Długie podcasty**: odcinek, którego serwer regularnie zrywa połączenie, nie jest już przerywany.
- **Pakiet Debian w kontenerze LXC** (Proxmox): usługa uruchamia się nawet bez prywatnych przestrzeni montowania.
- **Pierwsze uruchomienie**: kreator nie nazywa już „Włączone” usług bez konta i zapisuje wybrany folder z muzyką. Puste „Moje radia” otwierają katalog stacji.

## Bezpieczeństwo i konta

- **Odnawiana sesja**: z opcją „Nie wylogowuj mnie” (domyślnie odznaczoną) sesja przeglądarki odnawia się, dopóki jest używana, do 90 dni. W przeciwnym razie 24 h, jak wcześniej. Menu awatara oferuje „Wyloguj się z tego serwera”.
- **Odzyskiwanie dostępu administratora**: uruchom `tune-server --reset-admin` (lub `--reinitialiser-admin`) **na komputerze serwera**. Otrzymany jednorazowy kod, ważny 15 minut, wpisz w „Nie pamiętasz hasła?” na tym komputerze; bez przeglądarki dodaj `--password-stdin`. Otwarte sesje są zamykane. Przewodnik instalacji (sekcja 9) podaje polecenie dla każdego systemu.
- **Profil `admin`**: profil administratora tworzony domyślnie nazywa się teraz `admin`; wybrana przez Ciebie nazwa zostaje zachowana, a `default` nadal jest akceptowany.
- **Hasło administratora od pierwszej konfiguracji**: w nowej instalacji kreator prosi o jego utworzenie.
- **Serwer bez uwierzytelniania**: administracja jest akceptowana z sieci lokalnej, a nie tylko z komputera serwera. **Ostrzeżenie**: bez uwierzytelniania urządzenia w sieci lokalnej mogą administrować serwerem; włącz uwierzytelnianie, jeśli Twoja sieć nie jest zaufana. Ustawienie „Administracja tylko z tego komputera” przywraca poprzednią regułę.

## Piloty

- Na serwerze chronionym hasłem aplikacje, które nie są jeszcze zaktualizowane, proszą o hasło co 24 h, jak wcześniej.

## Do sprawdzenia u siebie

- **„Bypass all” w presecie**: preset, który zmienia jednocześnie Bypass all i cel DSD, w trybie „zastosuj natychmiast”, wznawia utwór od bieżącej pozycji. Sprawdzono tylko na stanowisku testowym: daj nam znać, czy wznowienie jest czyste.
