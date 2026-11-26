Tune 1.0.0-rc13.90: **wersja przedpremierowa (pre-release) rc14**, tylko dla testerów. To nie jest opublikowana wersja: automatyczna aktualizacja jej nie proponuje. Dziękujemy wszystkim, którzy ją testują.

## Nowości (New features)

- **TESS, 64-bitowy tor przetwarzania**: całe przetwarzanie odbywa się w 64-bitowej arytmetyce zmiennoprzecinkowej od początku do końca, z jedną kwantyzacją na samym końcu.
- **Ta sama droga dla wszystkich wyjść**: wyjście lokalne, UPnP/DLNA, AirPlay, Devialet AIR i OAAT przechodzą przez TESS; wyjście, które nie może przyjąć przetwarzania, jasno to mówi.
- **Korektor parametryczny** z dowolnymi pasmami (częstotliwość, wzmocnienie, Q), obok korektora graficznego i korektora słuchawkowego (profile AutoEq).
- **Splot (konwolucja)**: filtry .wav, archiwa .zip lub konfiguracje Convolver (.cfg), jeden filtr na częstotliwość próbkowania, zmiana bez przerwy i podana latencja.
- **Korekcja pomieszczenia na podstawie pomiaru**: filtr FIR o fazie mieszanej obliczony z kilku pozycji odsłuchowych.
- **Upsampling do DSD** (DSD64, DSD128, DSD256) modulatorem sigma-delta.
- **Ustawienia wstępne toru**: zapisz cały tor i przywołaj go od razu lub przy następnym odtwarzaniu.
- **Ścieżka sygnału**: z paska odtwarzania, stopień po stopniu, co jest naprawdę stosowane i dlaczego dany stopień nie jest.
- **Natywne wtyczki** w torze: włączanie w locie, format 64-bitowy, zachowana kolejność i liczba kanałów.
- **Interfejs w 20 językach**: dochodzą niderlandzki, chiński tradycyjny, rosyjski i ukraiński.
- **Uwierzytelnianie**: przycisk „Wyłącz uwierzytelnianie” w Ustawienia › System › Uwierzytelnianie.

## Ulepszenia (Improvements)

- **ISO SACD**: natywne DSD trafia do odtwarzaczy sieciowych jako wirtualny DSF; DSD przesyłane w DoP zgłasza się jako DSD.
- **Słuchanie zdalne**: w sieci 5G pierwszy utwór wychodzi jako FLAC.
- **Nieobecne urządzenia**: zapominane automatycznie po 30 dniach (regulowane), bez usuwania jakiejkolwiek strefy.
- **Cover+**: lepsze okładki, także dla radia internetowego (metadane ICY).
- **Odtwarzanie grupowe**: regulowane przesunięcie dla każdej strefy; „Poprzedni” w strefie prowadzącej uruchamia ponownie także strefy podążające.
- **Biblioteka i streaming**: zadania w tle nie blokują już interfejsu, alfabetyczne sortowanie albumów zostaje zachowane, podobni wykonawcy ze strony wykonawcy w serwisie.
- **Komunikaty odmowy** są krótsze i wskazują właściwe ustawienie.

## Poprawki (Fixes)

- **Poprawki z rc13.1 i rc13.2** są uwzględnione: logowanie na konto administratora, łączenie konta mozaiklabs.fr z sieci lokalnej, okładki przy włączonym uwierzytelnianiu, aktualizacja w systemie Windows.
- **UPnP**: Tune nie indeksuje już samego siebie; zaindeksowany utwór gra z bieżącego adresu swojego serwera.
- **Odtwarzanie**: 30 s wstępnego ładowania bez odrzucania buforów; przewinięcie odrzucone przez źródło nie przesuwa już strumienia.
- **Chromecast** łączy się ponownie; ponowna publikacja AirPlay nie przerywa już odtwarzania.
- **Podcasty**: niedostępne odcinki są poprawnie sygnalizowane.
- **Zgłoszenia z forum**: historia, Qobuz, Spotify, przycisk „Wstecz”, wyszukiwanie i iPad.

## Zmiany w działaniu

- **Korektor parametryczny**: bezpłatny do rc13, teraz należy do Tune Premium, podobnie jak splot, korekcja pomieszczenia, declick w czasie rzeczywistym i wtyczki innych firm. Korektor graficzny, korektor słuchawkowy i crossfeed pozostają bezpłatne.

## Do sprawdzenia u siebie

- **Pełny tor TESS** (korektor, crossfeed, splot lub korekcja pomieszczenia): czy ścieżka sygnału odpowiada temu, co słyszysz?
- **Import zestawu splotu** i zmiana filtra podczas odtwarzania.
- **Upsampling do DSD** na zgodnym przetworniku: stabilność, poziom, brak trzasków.
- **Ustawienia wstępne toru** w obu trybach stosowania.
- **Wyjście sieciowe** (UPnP, AirPlay lub OAAT) z aktywnym przetwarzaniem.
- **Aktualizacja z rc13.x** i przycisk „Wyłącz uwierzytelnianie”.

## Znane problemy

- **SACD skompresowane w DST**: te obrazy ISO nie są jeszcze odtwarzane natywnie.
- **Wskaźniki VU** na przekazywanym utworze MP3, AAC lub Ogg: po przewinięciu wyłączają się do końca utworu. Dźwięk nie jest zmieniony.
- **Słuchanie zdalne w CarPlay**: poprawka przekaźnika przesunięta do rc15.
- **Serwer bez ekranu z wyłączonym uwierzytelnianiem**: aby zmienić połączone konto mozaiklabs.fr, włącz uwierzytelnianie lub użyj tunelu SSH do localhost:8888.
