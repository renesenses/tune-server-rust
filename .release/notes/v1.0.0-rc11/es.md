Tune 1.0.0-rc11, undécima versión candidata de Tune 1.0.

## Novedades

- **Corrección de sala**: un plugin, desactivado por defecto, mide la sala con micrófono (barrido, respuesta al impulso, RT60) o simula sus modos de graves sin micrófono, y propone un objetivo y un ecualizador automático que solo atenúa, con un perfil por zona; sin Premium, la convolución por respuesta al impulso se apaga con un fundido.
- **Control parental**: reglas por perfil en Ajustes › Perfiles, ocultación de contenido explícito, franjas horarias y presupuesto de escucha.
- **Sobremuestreo**: un plugin Premium de sobremuestreo de alta calidad se conecta a la salida local, con su ajuste por zona.
- **Consulta avanzada**: consulte la biblioteca con una expresión, la misma que define las colecciones inteligentes.
- **Una ficha por álbum**: las versiones de un álbum se agrupan y se eligen en la ficha; aparecen nuevos filtros de calidad DSD y multicanal y el número de canales.
- **Notas y estadísticas**: notas personales en Markdown y estadísticas de la biblioteca por reparto.
- **Catálogo y espectrograma**: exporte la biblioteca como catálogo HTML imprimible, con los filtros actuales; muestre el espectrograma de una pista desde su menú; un botón detiene el análisis del DR entre dos pistas.
- **Mapas del mundo**: mapa de países y filtro por país en la Biblioteca; el globo traza fronteras, costas y grandes ríos, y nombra el país al pasar el cursor.
- **Pódcasts**: añada un pódcast con la dirección de su feed RSS.
- **Mixes de TIDAL**: la fila «Tus mixes» de Streaming › TIDAL muestra y reproduce los mixes personales de la cuenta.
- **Importación de ecualizador**: importe un archivo de ecualizador (AutoEq, Equalizer APO, OPRA) con una vista previa de las bandas antes de aplicarlo.
- **Biografías**: enlaces a artistas y álbumes de Tune y a la fuente; biografías y notas de álbum completadas a petición desde Qobuz, sin sobrescribir nada y de forma reversible.
- **Listas de reproducción**: sustituya en su sitio una pista de servicio no disponible y repare la lista.
- **Tune Circle**: escriba a todo un círculo en Mensajes; los círculos generales y el consentimiento del hilo pasan por el relé.
- **Colección física**: carátula de cada disco, búsqueda en los servicios de streaming, exploración por carpetas, géneros y estilos.

## Mejoras

- **Radios**: filtro por calidad mínima de flujo en «Más filtros», «Mis radios» separado de «Últimas escuchadas», flujo predeterminado siempre reproducible en el país de escucha.
- **Tune tested**: Tune propone añadir un dispositivo tras una hora de escucha sin errores, desde un botón dedicado; un dispositivo ya en el catálogo añade un hogar.
- **Biblioteca**: ajuste «Biblioteca ampliada a los favoritos de los servicios», activado por defecto; búsqueda por inicio de palabra más rápida; la búsqueda de la cuadrícula exige todas las palabras; periodo Desde / Hasta en la cronología de años; desplazamiento de pistas más fluido.
- **Listas de pistas**: el orden de las columnas se cambia y cada pantalla lo recuerda; la pantalla de una lista gana carátulas, tamaño de miniaturas y cabecera compacta; insignias de origen en el Gestor de listas.
- **Inicio y reanudación**: elija los paneles del resumen de escuchas; «Reanudar escucha» muestra un subtítulo por tipo, reanuda un pódcast con título, carátula y posición, y sin duplicados.
- **Historial**: una escucha solo se valida a los 30 s o a la mitad de la pista; «Reproducir» tras Stop conserva el álbum o la lista en curso; los episodios de pódcast y las radios llevan el nombre de la emisora.
- **Estadísticas de escucha**: las escuchas importadas (Roon, Plex, m3u) cuentan entre las más escuchadas; las listas inteligentes y el Historial cuentan como la Biblioteca; una escucha de Qobuz ya no cuenta para el álbum local del mismo nombre.
- **Crosstalk y ecualizador**: curva separación / coloración y respuesta de los altavoces en la pantalla de ajuste, separación objetivo desde 1 dB; gráfico paramétrico fijo por defecto (Editar / Hecho) y un color por canal.
- **Extensiones**: un panel «Estado de los plugins» para el administrador; una ficha del catálogo y su plugin local son ahora una sola extensión.
- **Notificaciones**: los errores persisten, se pausan al pasar el cursor y se agrupan; los últimos mensajes de error fijos están traducidos; el texto bruto de la excepción se pliega en «Detalles».
- **Análisis en segundo plano**: se muestra el avance, la prioridad es baja y el valor por defecto prudente tras el escaneo; el enriquecimiento continúa donde se quedó y reintenta tras un 503; «Lanzar pasada» de los logos de sellos actúa incluso en pausa.
- **Soporte y acceso remoto**: el informe de error adjunta registros; el enlace, el código QR y el botón copiar vuelven a Ajustes › Acceso remoto.
- **Reproducción**: se nombran los motivos de transcodificación y el FLAC se sirve tal cual cuando nada se aplica; Siguiente y Anterior saltan las pistas rechazadas por el servicio.

## Correcciones

- **AirPlay 2**: el botón «Pair» empareja con el componente AirPlay 2 o nombra su causa; la espera del PIN ya no bloquea la reproducción y cerrar su ventana la cancela; la sesión se reabre tras una pausa; la posición avanza y una pausa ya no entra en bucle; el componente existe para Raspberry Pi de 64 bits.
- **DLNA y OpenHome**: pistas DSD sin cortes, «siguiente» más rápido, se respeta una pausa hecha en el dispositivo, la salida de la propia máquina ya no aparece duplicada, volumen OpenHome sin espera, se conserva la lista cargada por Kazoo, Play más rápido en TIDAL CD.
- **Volumen**: una salida local nueva ya no arranca a plena escala; el silencio ya no deja pasar un volumen releído; cada posición del control da un nivel distinto; el volumen WASAPI y ASIO se oye al instante; Tune sigue al dispositivo bajo el techo de seguridad.
- **Windows**: el acceso directo se abre sin ventana de consola; un DoP que la salida no puede llevar sale como PCM; el descubrimiento de red pasa el cortafuegos; una temperatura ilegible ya no frena los análisis; la actualización lanza su script de cambio al salir.
- **Actualizaciones y licencia**: en Unix, el nuevo binario y la interfaz web se validan antes de sustituir los anteriores, y un fallo es explícito; retirar la licencia también libera la máquina en el sitio.
- **Biblioteca**: las fotos y vídeos de un servidor UPnP ya no entran; un cofre FLAC + CUE forma un solo álbum; las ediciones DSD y CD siguen siendo dos álbumes; una recopilación hecha a mano ya no se desmenuza; una lista eliminada ya no se recrea en el escaneo; una relectura sin etiqueta de artista ya no pasa el álbum a «Unknown Artist»; añadir una carpeta escanea solo esa carpeta.
- **Carátulas y análisis**: como último recurso, la imagen más grande de la carpeta; análisis de falso Hi-Res sobre toda la duración; MP3 sin cortes; la auditoría de integridad ya no declara truncados FLAC intactos.
- **Servicios de streaming**: la búsqueda de Spotify devuelve resultados, un rechazo ya no es mudo y los favoritos superan las 50 pistas; TIDAL › Favoritos ya no congela Tune; una cuenta de Qobuz sin favoritos devuelve rápido su lista vacía; una pista convertida arranca en Safari.
- **Red y almacenamiento**: el descubrimiento se reanuda cuando la red llega después de Tune; un montaje SMB que falla dice por qué, también en Docker; la copia de seguridad culmina en PostgreSQL; la extracción de CD rechaza un destino lleno o no escribible; Tune deja los puertos Squeezebox a un servidor Lyrion de la misma máquina.
- **Pantallas**: la pantalla Conciertos se abre sin licencia; «Desagrupar todo» reaparece; Reproduciendo ahora sigue el cambio de álbum; una sesión de streaming caducada conserva su pestaña «Reconectar»; Nueva alarma sigue visible.
- **Seguridad**: correcciones de refuerzo.
