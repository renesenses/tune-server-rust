Tune 1.0.0-rc10, décima versión candidata de Tune 1.0.

## Novedades

- **Mi colección física**: un nuevo complemento compara tu colección de Discogs con tu biblioteca y con los servicios de streaming, en una cuadrícula de portadas guardadas en caché en el servidor. Aparece una entrada propia en la barra lateral, justo después de Tune Circle.
- **Mi tema**: elige todos los colores de la interfaz, por perfil. Un complemento también puede aportar un tema, y puedes enviar el tuyo a un contacto de Tune Circle, que lo aplica con un clic.
- **Crosstalk**: el complemento que atenúa la diafonía entre altavoces llega al catálogo de extensiones, con su pantalla de ajustes; en una misma zona desactiva el crossfeed y lo indica.
- **Carpetas**: reproduce o añade a la cola una carpeta con todas sus subcarpetas, en orden; el filtro Directorio de la Biblioteca entra en las subcarpetas y ofrece volver a toda la biblioteca.
- **Radios**: una barra «Más filtros» en Mis radios y en el Directorio permite buscar por criterios; llegan cuatro géneros nuevos al directorio.
- **Radio en reproducción**: los flujos de una emisora se ordenan por calidad; Reproduciendo muestra el logotipo de la emisora y un botón «Mis radios»; el país de escucha se ajusta en Ajustes › General.
- **Sellos**: se muestran los logotipos de los sellos discográficos, compartidos por mozaiklabs.fr; el estado del servidor sigue el avance de las portadas de álbumes y de los logotipos de sellos.
- **Volumen**: el control está graduado en dB, los límites de confort y de seguridad aparecen en color, y se rechaza un límite de confort por encima del de seguridad.
- **Oxygen y metadatos**: modifica un campo en todo un resultado filtrado; excluye carpetas del enriquecimiento automático de metadatos.
- **Tune Circle**: zoom sobre una zona del globo en la vista Mundo; las listas de Tune Circle tienen su propia etiqueta en los filtros de Listas; una lista de un servicio se comparte desde su ficha.
- **Widget de escritorio**: la ventana mini usa la vista mini de la interfaz web y gana una búsqueda de artistas, Qobuz y TIDAL, con miniaturas y navegación por teclado.
- **Salida AES67**: una salida experimental hacia redes de audio Dante y Ravenna compatibles con AES67, desactivada por defecto, está en Ajustes › Experimental.

## Mejoras

- **Este navegador**: la pista siguiente se prepara antes del final para un encadenado más fluido, y la zona «Este navegador» del servidor se reproduce sin configuración previa.
- **Reanudar la escucha**: la tarjeta nombra el álbum, la lista o el episodio reanudado, y una escucha terminada sale de la lista.
- **Historial y temas destacados**: las pistas de un álbum o una lista abiertos van de la más reciente a la más antigua; los temas destacados de un artista añaden los que más escuchas, de todas las fuentes.
- **Biblioteca**: los filtros por DR y por año original son mucho más rápidos en bibliotecas grandes; las columnas de la lista de pistas se pueden redimensionar y cada pantalla recuerda su anchura; el análisis acústico explica su ritmo y muestra su avance.
- **Ecualizador**: entrada libre de la frecuencia y de la Q, validada al terminar de escribir; borrar preajustes importados pide confirmación; la corrección de sala y la calibración del DAC aplican el perfil de ecualizador completo.
- **Listas**: la importación .m3u, .m3u8 y .dpl está en el Gestor de listas e indica el número real de pistas; Importar, Transferir y Comparar dicen por qué fallan.
- **Portadas**: la carátula incrustada se lee en todas las etiquetas, primero la portada frontal; la búsqueda en línea sigue a la relectura del archivo; la imagen rectangular de una lista de Qobuz se muestra entera.
- **Mensajes**: los últimos mensajes de error que seguían en francés se traducen al idioma de la interfaz; un altavoz AirPlay 2 que rechaza la conexión indica que falta el emparejamiento; un identificador de aplicación rechazado por Qobuz ya no se presenta como una contraseña incorrecta.
- **Licencia**: la pantalla indica que Premium procede de tu cuenta Mozaiklabs, sin pedir clave.
- **Inicio**: «Otras versiones» ya no se bloquea esperando a los servicios de streaming.
- **ASIO**: cuando no se puede comprobar la compatibilidad con DSD, Tune lo indica y explica lo que admite la salida.

## Correcciones

- **Reproducción**: cambiar entre PURE y el ecualizador ya no reinicia una zona detenida; desmarcar «Repetir por defecto» quita de verdad la repetición de la zona; las transiciones entre pistas compatibles siguen siendo continuas.
- **OpenHome**: la pausa y la parada retiran del reproductor la pista siguiente preparada, y la posición final se conserva al pasar a la siguiente.
- **Radios**: con un país de escucha desconocido, el flujo elegido ya no se sustituye por uno de 96 kbit/s; una emisora añadida desde la consola conserva el vínculo con su ficha; un fallo aislado ya no suspende los metadatos de las radios; «Últimas escuchadas» muestra la fecha correcta; la lista de Qobuz creada desde los favoritos de radio aparece al instante.
- **Biblioteca y perfiles**: los filtros de favoritos y de valoraciones siguen el perfil activo; «Lo más escuchado» ya no cuenta una escucha varias veces; el enriquecimiento ya no sustituye un género existente; las pistas UPnP conservan año y géneros; un identificador de MusicBrainz ya asignado no se vuelve a asignar.
- **Colecciones y listas**: un guardado rechazado se indica, una carpeta defectuosa ya no vacía la lista y se leen todas las páginas de listas.
- **Complementos y cuenta**: desinstalar un complemento nativo activo lo desactiva de forma duradera; la primera vinculación con una cuenta Mozaiklabs se asocia al perfil único incluso sin correo electrónico.
- **Varios**: la importación de listas durante el escaneo dice por qué no importa; un fallo al leer los ajustes se indica en lugar de volver a los valores por defecto; la suspensión del ordenador ya no se señala como un bloqueo; la interfaz antigua ya no sobrevive a una actualización del servidor; la barra de vúmetros ya no se desborda entre 1280 y 1800 px; el modo WASAPI sigue visible con el ajuste predeterminado de Windows; el audio ya no se comprime al enviarse.
- **Seguridad**: correcciones de refuerzo.
