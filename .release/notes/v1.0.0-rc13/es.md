Tune 1.0.0-rc13, la decimotercera versión candidata de Tune 1.0. Gracias a Gros Bidon, r5yk5, Tades, cpadiou77, Pierre, Daniel LEVY, Lio, sashasha, ajm, Jean Valjean, Semakawa, Jon Rotaetxe, hifi25nl, aqs, Christopher J, FabienM, Begastor y Régis por sus informes y sus pruebas, a alec_eiffel por el Smart Upsampler, a cometdom por Qobuz y a Levente por el diseño.

## Novedades

- **Sobremuestreador**: el Smart Upsampler de Rémi (alec_eiffel) suena al mismo nivel que sin él, y un cambio de ajuste se oye enseguida.
- **Reproducción y salidas**: Sendspin y Devialet AIR reciben el sonido procesado (ecualizador, complementos, sobremuestreador). El apagado programado puede apagar el ordenador; controles multimedia del escritorio en Linux.
- **Crossfeed**: gratuito, con sus ajustes y sus preajustes.
- **Convolución**: un archivo de respuesta al impulso por frecuencia de muestreo.
- **DSD**: el DSD de un servidor UPnP se reproduce de forma nativa en reproductores de red exigentes y dos pistas DSD se encadenan sin silencio.
- **Biblioteca**: sus etiquetas personales se convierten en etiquetas de Tune; selección de varios álbumes para una acción conjunta; filtro por instrumento.
- **Fuente alterada** sustituye a «Hi-Res dudoso», para todos los formatos, con el corte medido.
- **Forma de onda** más fina, también para las pistas CUE, Qobuz y TIDAL (de forma opcional).
- **Cola**: los álbumes se pliegan y se mueven con un solo gesto.
- **Inicio**: 5 playlists «Porque te gusta…» y los widgets Artistas más escuchados, Escuchas frecuentes, Escuchar más tarde y Seguir escuchando.
- **Servicios**: página «Para ti» de TIDAL; 200 novedades de Qobuz por género.
- **Tune Circle**: los miembros de un círculo se ven entre sí si lo aceptan, y la vista Mundo muestra todos los círculos.
- **Interfaz web**: turco, autenticación activable desde la interfaz e historial por título o por escucha.
- **Tune Client (aplicación de escritorio)**: encuentra sola los servidores de la red y controla el servidor local.
- **Windows**: inicio de Tune con Windows, propuesto por el instalador.
- **App iOS**: CarPlay en 5G más completo, vuelve la barra lateral en iPad.

## Mejoras

- **Transiciones**: ReplayGain «Auto» mantiene la ganancia de álbum entre dos pistas del mismo álbum; AAC, Opus y pistas CUE se encadenan sin silencio.
- **Reproducción y salidas**: misma curva de volumen en AirPlay 1 y 2; una radio arranca más rápido en un reproductor de red.
- **Windows**: en modo exclusivo WASAPI, repliegue a la otra familia de frecuencias en lugar de detener la pista.
- **Devialet AIR**: volumen en pasos de 1 dB, hasta 192 kHz.
- **DSD**: la detección del DSD nativo es más prudente y se vuelve a evaluar.
- **Grandes bibliotecas**: Carpetas se abre sin esperar, y las tareas en segundo plano ya no bloquean el resto.
- **Servicios**: cliente de inicio de sesión de Google propio de Tune para YouTube; Spotify y TIDAL más robustos durante la reproducción.
- **Interfaz web**: ecualizador paramétrico más preciso y confirmación antes de eliminar.
- **App iOS**: la pantalla de bloqueo maneja la zona incluso detenida; sin DAC, el control deslizante ajusta el volumen de iOS.
- **Actualización y diagnóstico**: la actualización también sustituye los programas incluidos con Tune; los informes de errores ocultan los nombres.

## Correcciones

- **Reproducción y salidas**: la reanudación funciona tras una pausa que había liberado el DAC; «Repetir todo» ya no confunde una parada momentánea con el final de una pista; en Windows, se detecta un DAC encendido después de Tune.
- **Zonas**: un dispositivo ya vinculado nombra su zona; con OAAT, una reanudación vuelve a abrir la sesión en lugar de quedarse muda.
- **DLNA, UPnP y OpenHome**: se acabó el Stop parásito en Musicbook Source, silencio estable, Linn Klimax DS/2 reconocido.
- **AirPlay**: la salida AirPlay 1 ya no arranca al máximo tras un reinicio.
- **Ruta de la señal**: ya no anuncia un procesamiento que no tiene lugar.
- **Biblioteca**: las ediciones DSD y CD de un mismo álbum ya no se fusionan; la escritura del DR ya no congela el servidor.
- **Servicios**: favoritos de Qobuz y TIDAL con su duración; las secciones de Spotify ya no aparecen duplicadas.
- **Radios**: las emisoras FIP recuperan su logotipo.
- **Complementos y crossfeed**: un complemento instalado actúa sin reinicio; el crossfeed ya no está bloqueado con OAAT ni en el navegador.
- **Interfaz web**: la cuadrícula se desplaza hasta el final; el aviso «el servidor no responde» desaparece en cuanto responde.

## Seguridad

- Varios refuerzos de seguridad.
- Los registros y los informes de errores ocultan ahora las credenciales que contienen algunas direcciones de flujo.
- Reemplazar la ruta (Ajustes › carpetas de música) verifica ahora las carpetas y ya no es accesible desde otro dispositivo sin autenticación.

## Cambios de comportamiento

- **Tune Circle**: con la autenticación, reservado a los administradores; en solo lectura desde la escucha a distancia.
- **YouTube**: las cuentas conectadas antes de esta versión deben reconectarse una vez.
- **Radio al final de la cola**: de nuevo desactivada por defecto; vuelva a marcarla en la zona si la había elegido.
- **Ecualizador**: al desactivar el ecualizador, su preamplificación se mantiene hasta «Restablecer el nivel».
- **Multiroom**: grupo gratuito hasta 3 zonas; por encima, Premium. Las parejas estéreo siguen siendo gratuitas.
- **CUE con INDEX 02**: inicios de pista corregidos en el próximo escaneo, historial conservado.
- **Tune OS**: la contraseña temporal se muestra en la pantalla de la máquina o en la tarjeta de arranque, ya no en la página de inicio de sesión; el acceso remoto permanece cerrado mientras no se cambie.
- **Perfiles e historial**: con la autenticación, solo el administrador gestiona los demás perfiles.
- **Reemplazar la ruta**: sin autenticación, desde la máquina en la que funciona Tune.

## Para comprobar en casa

Estos cambios han superado las pruebas automáticas sin prueba en hardware real. Cuéntenos lo que observa:

- **Sendspin y Devialet AIR** con un procesamiento activo.
- **Windows**: minirreproductor, modo exclusivo WASAPI, inicio con Windows.
- **AirPlay 1**: nivel al arrancar y entre pistas.
- **DSD hacia un reproductor de red** (Atoll ST200, Eversolo).
- **Linn Klimax DS/2** como OpenHome, **Tune Client en macOS** y **CarPlay en 5G**.

## Problemas conocidos

- **DSD en DoP en la salida local**: después de un PCM atenuado, puede sonar a plena escala. Mantenga bajo el volumen del amplificador.
- **UPnP tras un cambio de dirección**: una pista puede no reproducirse; vuelva a indexar ese servidor.
- **Auditoría de integridad**: algunos archivos pueden señalarse por error; no elimine nada basándose solo en ese veredicto.
- **Windows ASIO controlado por otro servidor**: sin sonido tras Pausa y luego Reproducir; pase al título siguiente.
- **Tras la actualización**: si la interfaz no cambia, fuerce la recarga (Cmd+Mayús+R o Ctrl+Mayús+R).
