Tune 1.0.0-rc13.90: **pre-release de la rc14**, solo para testers. No es una versión publicada: la actualización automática no la ofrece. Gracias a quienes la prueban.

## Novedades

- **TESS, la cadena de procesamiento de 64 bits**: todo el procesamiento se hace en coma flotante de 64 bits de principio a fin, con una sola cuantificación, al final del todo.
- **El mismo camino para todas las salidas**: salida local, UPnP/DLNA, AirPlay, Devialet AIR y OAAT pasan por TESS; una salida que no puede recibir un procesamiento lo dice claramente.
- **Ecualizador paramétrico** con bandas libres (frecuencia, ganancia, Q), junto al ecualizador gráfico y al ecualizador de auriculares (perfiles AutoEq).
- **Convolución**: filtros en .wav, en archivo .zip o en configuraciones Convolver (.cfg), un filtro por frecuencia de muestreo, cambio sin cortes y latencia anunciada.
- **Corrección de sala por medición**: un filtro FIR de fase mixta calculado sobre varias posiciones de escucha.
- **Upsampling a DSD** (DSD64, DSD128, DSD256) con un modulador sigma-delta.
- **Preajustes de cadena**: guarde una cadena completa y recupérela, al instante o en la próxima reproducción.
- **Ruta de la señal**: desde la barra de reproducción, etapa por etapa, lo que realmente se aplica y por qué una etapa no se aplica.
- **Plugins nativos** en la cadena: activación en caliente, formato de 64 bits, orden y número de canales respetados.
- **Interfaz en 20 idiomas**: llegan el neerlandés, el chino tradicional, el ruso y el ucraniano.
- **Autenticación**: un botón «Desactivar la autenticación» en Ajustes › Sistema › Autenticación.

## Mejoras

- **ISO SACD**: el DSD nativo se sirve a los reproductores de red como DSF virtual; un DSD transportado en DoP se anuncia como DSD.
- **Escucha remota**: en 5G, la primera pista sale en FLAC.
- **Dispositivos ausentes**: se olvidan automáticamente tras 30 días (ajustable), sin borrar nunca una zona.
- **Cover+**: mejores portadas, también para las radios web (metadatos ICY).
- **Reproducción en grupo**: desfase ajustable por zona; «Anterior» en la zona principal reinicia también las zonas que la siguen.
- **Biblioteca y streaming**: las pasadas en segundo plano ya no bloquean la interfaz, se conserva el orden alfabético de los álbumes, artistas similares desde la ficha de un servicio.
- **Mensajes de rechazo** más cortos, que remiten al ajuste correcto.

## Correcciones

- **Las correcciones de las rc13.1 y rc13.2** están incluidas: inicio de sesión de la cuenta de administrador, vinculación de la cuenta mozaiklabs.fr desde la red local, portadas con la autenticación activa, actualización en Windows.
- **UPnP**: Tune ya no se indexa a sí mismo; una pista indexada se reproduce desde la dirección actual de su servidor.
- **Reproducción**: precarga de 30 s sin búferes descartados; un salto rechazado por la fuente ya no desplaza el flujo.
- **Chromecast** se reconecta; la republicación AirPlay ya no corta la reproducción.
- **Podcasts**: los episodios no disponibles se señalan correctamente.
- **Informes del foro**: historial, Qobuz, Spotify, botón «Atrás», búsqueda e iPad.

## Cambios de comportamiento

- **Ecualizador paramétrico**: gratuito hasta la rc13, ahora forma parte de Tune Premium, como la convolución, la corrección de sala, el declick en tiempo real y los plugins de terceros. El ecualizador gráfico, el ecualizador de auriculares y el crossfeed siguen siendo gratuitos.

## Para comprobar en casa

- **Una cadena TESS completa** (ecualizador, crossfeed, convolución o corrección de sala): ¿la ruta de la señal corresponde a lo que oye?
- **La importación de un juego de convolución** y el cambio de filtro durante la reproducción.
- **El upsampling a DSD** en un DAC compatible: estabilidad, nivel, ausencia de clics.
- **Los preajustes de cadena**, en los dos modos de aplicación.
- **Una salida de red** (UPnP, AirPlay u OAAT) con un procesamiento activo.
- **La actualización desde la rc13.x** y el botón «Desactivar la autenticación».

## Problemas conocidos

- **SACD comprimidos en DST**: estas imágenes ISO aún no se leen de forma nativa.
- **Vúmetros** en una pista MP3, AAC u Ogg retransmitida: tras un salto, se apagan hasta el final de la pista. El sonido no se ve afectado.
- **Escucha remota en CarPlay**: la corrección del relé se aplaza a la rc15.
- **Servidor sin pantalla con la autenticación desactivada**: para cambiar la cuenta mozaiklabs.fr vinculada, active la autenticación o use un túnel SSH hacia localhost:8888.
