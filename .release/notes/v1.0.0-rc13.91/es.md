Tune 1.0.0-rc13.91: **segunda pre-release de la rc14**, solo para testers. No es una versión publicada: la actualización automática no la ofrece. Incluye todo el contenido de la rc13.90. Gracias a quienes la prueban.

## Novedades

- **Ecualizadores combinables en serie**: el ecualizador gráfico, el paramétrico, el de auriculares y la corrección de sala se encadenan en la misma cadena TESS; aplicar una corrección de sala ya no sobrescribe los demás ecualizadores. Las ganancias del ecualizador paramétrico se amplían.
- **Bypass all completo**: un solo gesto saca de circuito toda la cadena TESS y la restablece tal cual.
- **Plugin Auriculares** basado en la base de datos OPRA: se reconoce el perfil de sus auriculares, con preamplificación y nivel.
- **Plugin Speaker Setup** (ajuste de los altavoces): distancia, ganancia y fase, canal por canal.
- **Upsampling a DSD en la salida local**: un modulador sigma-delta dedicado, y se publican las capacidades DSD de la salida.
- **Destino del upsampling a elegir**: en TESS, una pantalla permite elegir hacia qué hacer el upsampling, en PCM o en DSD.
- **Prueba de nivel del micrófono** antes de la medición de la sala.
- **Medición a la frecuencia nativa del micrófono**, de 44,1 a 192 kHz.
- **Ajustes TESS al vuelo**: desactivar una etapa surte efecto de inmediato, y la preamplificación sigue acotada.

## Mejoras

- **Corrección de sala**: la medición de verificación escucha la corrección de la zona; se explican las desviaciones no corregidas, y el límite superior es ajustable hasta 1 kHz.
- **Corrección de sala**: los datos introducidos, la última medición y la corrección aplicada se recuperan al volver a la pantalla; unas indicaciones de colocación del micrófono y un recuadro «Antes de la medición» guían la medición.
- **Medición en modo PURE**: el volumen se fuerza al 100 % mientras dura el barrido y luego se restablece como estaba.
- **Ruta de la señal**: una etapa apagada (upsampling, convolución) dice por qué, en TESS y en la página del plugin; en Windows en modo compartido, el upsampling apunta a la frecuencia del mezclador.
- **Créditos**: el instrumento en lugar de «Músico», el autor distinto del compositor, un nombre por rol.
- **Escaneo**: el código de barras presente en las etiquetas completa el álbum si no tiene.
- **Estado del servidor**: se muestra un aviso si no se puede crear la caché de portadas.

## Correcciones

- **Calibración**: el barrido ya no se detiene antes de la duración anunciada ni se queda bloqueado en la cola; una zona sin salida se rechaza claramente.
- **Calibración**: un pico en el techo ya no bloquea las bandas siguientes.
- **RT60**: «no medible (ruido)» en lugar de un valor falso; los mensajes de error siguen legibles al menos 10 s.
- **Aviso de frecuencia**: ya solo aparece fuera de las frecuencias aceptadas.
- **Radio**: el cambio de emisora ya no queda anulado por la emisora anterior.
- **Portadas**: en una instalación nueva, la caché se crea dentro de la carpeta de datos.
- **TESS**: un solo aviso de «pista siguiente», y se corrige la ayuda del botón «Aplicar».

## Cambios de comportamiento

- **Cambio de la cadena TESS durante la reproducción**: una etapa desactivada se corta inmediatamente; una etapa añadida o reactivada se oye a partir de la siguiente reproducción.

## Para comprobar en casa

Estos puntos aún no se han probado en hardware real:

- **El upsampling a DSD en un DAC DSD real**: estabilidad, nivel, ausencia de clics.
- **La medición de la sala a 44,1 kHz y a 96 kHz** en una interfaz de audio real, con la prueba de nivel del micrófono.
- **En Windows, con WASAPI compartido**: reproducción y upsampling.
- **Speaker Setup en una salida de red** (UPnP, AirPlay u OAAT): distancia, ganancia y fase.

Y también:

- **Varios ecualizadores en serie** con Bypass all: ¿la ruta de la señal corresponde a lo que oye?
- **El plugin Auriculares** con sus propios auriculares.
- **La actualización desde la rc13.90.**

## Problemas conocidos

- **SACD comprimidos en DST**: estas imágenes ISO aún no se leen de forma nativa.
- **Vúmetros** en una pista MP3, AAC u Ogg retransmitida: tras un salto, se apagan hasta el final de la pista. El sonido no se ve afectado.
- **Escucha remota en CarPlay**: la corrección del relé se aplaza a la rc15.
- **Servidor sin pantalla con la autenticación desactivada**: para cambiar la cuenta mozaiklabs.fr vinculada, active la autenticación o use un túnel SSH hacia localhost:8888.
