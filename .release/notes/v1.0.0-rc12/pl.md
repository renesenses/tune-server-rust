Tune 1.0.0-rc12, dwunasta wersja kandydująca Tune 1.0. Kilka ekranów otrzymuje nowy wygląd zaprojektowany przez Leventego, projektanta ze społeczności Tune.

## Nowości (New features)

- **Nowy interfejs**: przeprojektowany Tune Circle (Odkrywaj, Wiadomości, Udostępnione playlisty, Mój krąg); strona albumu z opisem, twórcami, szczegółami i kartą Wersje z numerem katalogowym; strona wykonawcy z dużym portretem, liczbami i kartami; Ustawienia w czytelniejszej kolumnie; Stan serwera i panel słuchania z wyborem okresu.
- **Start**: nowy widżet „Przegląd słuchania (nowy układ)”, do dodania w Start › Edytuj, z panelem Strefy; większe kafelki z rokiem i gatunkiem.
- **Biblioteka**: przypnij wybrane filtry przyciskiem Filtry; muzyka klasyczna z polami Dyrygent, Zespół / orkiestra i Utwór; liczba kanałów i filtr Stereo; mapa i filtr według kraju wykonawcy; „Zastąp ścieżkę” dla folderu z muzyką; wykrywanie fałszywego Hi-Res, domyślnie wyłączone.
- **Karta techniczna utworu**: spektrogram i przebieg fali dla każdego kanału, także dla utworów Qobuz i TIDAL w bibliotece.
- **Radio na końcu kolejki**: gdy kolejka się kończy, Tune gra dalej podobne utwory, oznaczone „Radio”; ustawiane osobno dla każdej strefy.
- **Wyłącznik czasowy**: muzyka zatrzymuje się o wybranej godzinie; w Tune OS maszyna może się też wyłączyć.
- **Playlisty**: liczba odtworzeń i ostatnie odtworzenie w każdym wierszu; krzyżyk Usuń i „Usuń z tej playlisty”; usuwanie utworu z własnej playlisty Qobuz.
- **Radio**: filtry Bezstratne, Wysoka jakość i Hi-Res w jednym wierszu; stację z katalogu można edytować tylko dla siebie; radio można dodać do playlisty.
- **Odtwarzanie**: natywne DSD na lokalnym wyjściu Linux (ALSA); stała częstotliwość PCM dla każdej strefy; tryb wyłączny ustawiany osobno dla każdej strefy; usunięte strefy można przywrócić; prawdziwa weryfikacja AccurateRip płyt CD.
- **DLNA**: ciągły strumień między utworami dla rendererów, które źle łączą dwa utwory, domyślnie wyłączony; głośność główną grupy można ustawić z dowolnego jej członka.
- **Streaming**: wybór kraju katalogu TIDAL; nowości Qobuz takie same jak w odtwarzaczu Qobuz; Spotify z asystentem połączenia, a Spotify Connect gra w wybranej strefie.
- **Ścieżka sygnału**: kliknięcie etapu przetwarzania otwiera jego ustawienia; odtwarzanie grupowe pokazuje każde wyjście.
- **Ustawienia i interfejs**: wyszukiwanie według ustawienia, rozmiar tekstu, pasek postępu w kształcie przebiegu fali, jedno pole wyszukiwania na wszystkich ekranach, wprowadzanie głosowe w asystencie.

## Ulepszenia (Improvements)

- **Szybkość**: inteligentne kolekcje i skoki alfabetyczne są znacznie szybsze w dużej bibliotece; duże listy nie spowalniają już serwera.
- **Pamięć**: Tune oddaje pamięć zwolnioną podczas długiego słuchania; pamięć transkodowania jest ograniczona.
- **Szybszy start dźwięku**: YouTube startuje szybciej; TIDAL nie czeka już na pobranie całego utworu na wyjściu lokalnym lub OAAT.
- **Statystyki**: jedna suma utworów i albumów na wszystkich ekranach, jeden wpis na kodek, wyszukiwanie w całym rankingu.
- **Tune Circle**: emoji w Wiadomościach; gdy słuchanie u kontaktu jest zamknięte, Tune szuka tego samego utworu w Twoich źródłach.
- **Aplikacje mobilne**: CarPlay gra bezpośrednio na telefonie, z Kolekcjami i kartą Radio; Android zyskuje katalog TIDAL, historię radia i podcastów oraz widżet Ulubione radia.
- **Windows**: baner ostrzega, gdy zapora filtruje Tune; znacznik WASAPI lub ASIO na strefach lokalnych.
- **Docker**: asystent udziału SMB działa w obrazie; napęd CD można używać w kontenerze.
- **Tłumaczenia**: poprawne liczby mnogie we wszystkich 15 językach.
- **Zależności** zaktualizowane, w tym wbudowany SQLite 3.54.

## Poprawki (Fixes)

- **Interfejs webowy**: koniec z czarnym ekranem po aktualizacji interfejsu.
- **Wyciszenie**: wyciszona strefa pozostaje cicha, gdy startuje nowy utwór; budzik ani limit głośności nie wyłączają już wyciszenia.
- **Odtwarzanie**: uszkodzony plik nie zawiesza już strefy, Tune przechodzi dalej; zakończony odcinek podcastu nie otwiera się już ponownie na końcu.
- **AirPlay 2**: po pauzie odtwarzanie wznawia się tam, gdzie się zatrzymało, i startuje z głośnością strefy, nigdy z pełną mocą.
- **DLNA i grupy**: odtwarzanie wznawia się po błędzie 701; następny utwór jest przygotowywany z wyprzedzeniem; urządzenie, które nie odpowiada, nie blokuje już innych stref; Sonos przyjmuje nazwę swojego modelu.
- **HQPlayer, Devialet i Diretta**: śledzenie pozycji i stanu, automatyczne ponowne łączenie; wzmacniacz Devialet znowu odpowiada; koniec z pozornym suwakiem głośności w Diretta.
- **OAAT**: natywne DSD wyświetlane i bit-perfect; DSD1024 rozpoznawane, z przejściem na PCM w ograniczonym odbiorniku.
- **Radia**: koniec ze zdublowanymi stacjami; radio odrzucone w Twoim kraju przechodzi do kolejnego strumienia.
- **Spotify, Qobuz i TIDAL**: playlisty i ulubione Spotify korzystają z nowego API; odmowa Qobuz nie jest już przedstawiana jako błędne dane logowania; wstępne ładowanie TIDAL nie opóźnia już odtwarzania.
- **Biblioteka**: odsłuchy są liczone dla każdego utworu; łącznik nie tworzy już fałszywego wykonawcy; płyty z boxu są lepiej rozpoznawane; utwór bez roku przyjmuje rok swojego albumu.
- **Windows**: brak DAC nie kieruje już dźwięku do głośników komputera; obserwowanie folderów obejmuje udziały sieciowe.
- **PostgreSQL 18**: ulubione UPnP znowu działają.

## Bezpieczeństwo

- Administrowanie Tune z innej maszyny w sieci wymaga teraz zalogowania się (Tune OS: do czasu nowego obrazu).
- Webhooki nie mogą już być kierowane do samej maszyny Tune.
- Konfiguracja Home Assistant i Hue wymaga uprawnień administratora albo samej maszyny Tune; Home Assistant zainstalowany na tej samej maszynie co Tune nie jest już obsługiwany.

## Znane problemy

- **DSD przez DoP na wyjściu lokalnym**: po wytłumionym PCM może grać z pełną skalą, bez uwzględnienia głośności. Trzymaj niską głośność wzmacniacza, gdy przechodzisz z utworu PCM do utworu DSD przez DoP (następna wersja).
- **Audyt integralności**: wiele plików FLAC może zostać błędnie zgłoszonych jako uszkodzone; nie usuwaj żadnego pliku wyłącznie na podstawie tego werdyktu.
- **Windows ASIO sterowane przez inny serwer**: brak dźwięku po Pauza, a następnie Odtwórz; przejdź do następnego utworu.
- **Po aktualizacji**: jeśli interfejs się nie zmienia, wymuś ponowne załadowanie (Cmd+Shift+R lub Ctrl+Shift+R). Pierwsze uruchomienie trwa dłużej, ponieważ tworzone są dwa indeksy.
