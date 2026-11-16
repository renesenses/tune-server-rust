Tune 1.0.0-rc12, duodécima versión candidata de Tune 1.0. Varias pantallas estrenan una nueva presentación diseñada por Levente, diseñador de la comunidad Tune.

## Novedades

- **Nueva interfaz**: Tune Circle rediseñado (Descubrir, Mensajes, Listas compartidas, Mi círculo); ficha de álbum con descripción, créditos, detalles y una pestaña Versiones con el número de catálogo; página de artista con un gran retrato, cifras y pestañas; Ajustes en una columna más legible; Estado del servidor y panel de escuchas, con elección del periodo.
- **Inicio**: un nuevo widget «Resumen de escuchas (nueva presentación)», que se añade en Inicio › Editar, con un panel Zonas; miniaturas más grandes con año y género.
- **Biblioteca**: fije los filtros que prefiera con el botón Filtros; música clásica con Director, Conjunto / orquesta y Obra; número de canales y filtro Estéreo; mapa y filtro por país del artista; «Sustituir la ruta» de una carpeta de música; detección de falsos Hi-Res, desactivada por defecto.
- **Ficha técnica de una pista**: espectrograma y forma de onda por canal, también para las pistas de Qobuz y TIDAL de la biblioteca.
- **Radio al final de la cola**: cuando la cola termina, Tune continúa con títulos similares, marcados «Radio»; se ajusta zona por zona.
- **Apagado programado**: la música se detiene a la hora elegida; en Tune OS, la máquina también puede apagarse.
- **Listas de reproducción**: número de escuchas y última escucha en cada fila; cruz Quitar y «Eliminar de esta lista»; retirada de un título de su propia lista de Qobuz.
- **Radio**: filtros Sin pérdida, Alta calidad y Hi-Res en una sola fila; una emisora del directorio se modifica para usted; una radio se añade a una lista de reproducción.
- **Reproducción**: DSD nativo en la salida local de Linux (ALSA); frecuencia PCM fija por zona; modo exclusivo ajustado zona por zona; zonas eliminadas restaurables; verificación AccurateRip real de los CD.
- **DLNA**: flujo continuo entre pistas para los renderers que enlazan mal dos títulos, desactivado por defecto; el volumen maestro de un grupo se ajusta desde cualquier miembro.
- **Streaming**: elección del país del catálogo de TIDAL; novedades de Qobuz idénticas a las del reproductor de Qobuz; Spotify con un asistente de conexión, y Spotify Connect suena en la zona elegida.
- **Ruta de la señal**: un clic en un procesamiento abre sus ajustes; la reproducción agrupada muestra cada salida.
- **Ajustes e interfaz**: búsqueda por ajuste, tamaño del texto, barra de progreso en forma de onda, un único campo de búsqueda en todas las pantallas, entrada por voz en el asistente.

## Mejoras

- **Velocidad**: colecciones inteligentes y saltos alfabéticos mucho más rápidos en una biblioteca grande; las listas grandes ya no ralentizan el servidor.
- **Memoria**: Tune devuelve la memoria liberada durante una escucha larga; la memoria de la transcodificación está acotada.
- **Arranque del sonido**: YouTube arranca más rápido; TIDAL ya no espera la descarga completa de la pista en una salida local u OAAT.
- **Estadísticas**: un único total de títulos y álbumes en todas las pantallas, una sola entrada por códec, búsqueda en toda la clasificación.
- **Tune Circle**: emojis en Mensajes; cuando la escucha en casa de un contacto está cerrada, Tune busca el mismo título en sus fuentes.
- **Aplicaciones móviles**: CarPlay reproduce directamente en el teléfono, con Colecciones y una pestaña Radio; Android gana el catálogo de TIDAL, el historial de radios y pódcasts, y un widget Radios favoritas.
- **Windows**: un aviso indica cuando el cortafuegos filtra Tune; insignia WASAPI o ASIO en las zonas locales.
- **Docker**: el asistente de recurso compartido SMB funciona en la imagen; el lector de CD se puede usar en el contenedor.
- **Traducciones**: plurales correctos en los 15 idiomas.
- **Dependencias** actualizadas, entre ellas SQLite 3.54 integrado.

## Correcciones

- **Interfaz web**: se acabó la pantalla negra tras una actualización de la interfaz.
- **Silencio**: una zona silenciada sigue muda cuando arranca una nueva pista; una alarma o un límite de volumen ya no quitan el silencio.
- **Reproducción**: un archivo dañado ya no congela la zona, Tune pasa a la siguiente pista; un episodio de pódcast terminado ya no se vuelve a abrir al final.
- **AirPlay 2**: tras una pausa, la reproducción se reanuda donde se había detenido, y arranca al volumen de la zona, nunca a plena potencia.
- **DLNA y grupos**: la reproducción se reanuda tras un error 701; la pista siguiente se prepara por adelantado; un dispositivo que no responde ya no bloquea las demás zonas; Sonos toma el nombre de su modelo.
- **HQPlayer, Devialet y Diretta**: posición y estado seguidos, reconexión automática; el amplificador Devialet vuelve a responder; desaparece el falso control de volumen en Diretta.
- **OAAT**: DSD nativo mostrado y bit-perfect; DSD1024 reconocido, con paso a PCM en un receptor limitado.
- **Radios**: ya no hay emisoras duplicadas; una radio rechazada en su país pasa a su siguiente flujo.
- **Spotify, Qobuz y TIDAL**: las listas y los favoritos de Spotify siguen la nueva API; un rechazo de Qobuz ya no se presenta como credenciales erróneas; la precarga de TIDAL ya no retrasa la reproducción.
- **Biblioteca**: las escuchas se cuentan por título; un guion ya no crea un falso artista; los discos de una caja se reconocen mejor; un título sin año toma el de su álbum.
- **Windows**: un DAC ausente ya no envía el sonido a los altavoces del PC; la vigilancia de carpetas cubre los recursos compartidos de red.
- **PostgreSQL 18**: los favoritos UPnP vuelven a funcionar.

## Seguridad

- Administrar Tune desde otra máquina de la red ahora requiere identificarse (Tune OS: hasta la nueva imagen).
- Los webhooks ya no pueden apuntar a la propia máquina Tune.
- La configuración de Home Assistant y de Hue requiere derechos de administración o la propia máquina Tune; un Home Assistant instalado en la misma máquina que Tune ya no es compatible.

## Problemas conocidos

- **DSD en DoP en la salida local**: puede sonar a plena escala después de un PCM atenuado, sin tener en cuenta el volumen. Mantenga bajo el volumen del amplificador al pasar de un título PCM a un título DSD en DoP (próxima versión).
- **Auditoría de integridad**: muchos FLAC pueden señalarse por error como dañados; no elimine ningún archivo basándose solo en ese veredicto.
- **Windows ASIO controlado por otro servidor**: no hay sonido tras Pausa y luego Reproducir; pase al título siguiente.
- **Tras la actualización**: si la interfaz no cambia, fuerce la recarga (Cmd+Mayús+R o Ctrl+Mayús+R). El primer arranque es más largo, el tiempo de crear dos índices.
