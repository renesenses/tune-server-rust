Tune 1.0.0-rc9, la novena versión candidata de Tune 1.0.

## Novedades

- **Tune Circle**: mensajería privada entre contactos, con tarjetas de música que se escuchan con un clic, borrado de tus propios mensajes y denuncia. Los mensajes no están cifrados de extremo a extremo y Mozaiklabs puede leerlos tras una denuncia.
- **Tune Circle**: «Escribir» desde la lista de contactos abre directamente la conversación; Intro envía el mensaje; diseño adaptado al teléfono.
- **Tune Circle**: encuentra los círculos a los que te han invitado y comparte una playlist; una insignia y una lista reúnen las notificaciones, configurables por tipo.
- **Tune Circle**: la vista Mundo muestra la comunidad de forma anónima, por país y por zonas de unos 110 km. Puedes retirar tu presencia del mapa público.
- **Radio**: la búsqueda general encuentra tus emisoras y las del directorio, con varias palabras en cualquier orden. Una ficha muestra la emisora, sus géneros, su web y sus flujos, y puedes elegir tu flujo preferido.
- **Mapas y podcasts**: explora las emisoras por género, país y red, con una vista de lista; recorre podcasts de varios países.
- **Reanudar la reproducción**: la reanudación recupera el álbum, la playlist o la cola de origen, incluso en aleatorio. En los podcasts retrocede unos segundos, ajustable de 0 a 300 s.
- **Colecciones**: orden aleatorio en colecciones y etiquetas, con «Volver a sortear»; los álbumes favoritos de un servicio se ordenan por año y calidad.
- **Reproduciendo**: un botón «Compartir» copia el enlace de la pista actual.
- **Oxygen**: aplica o quita una etiqueta a todo un resultado filtrado, tras una vista previa y una confirmación.
- **Apariencia**: nuevo tema «Mozaiklabs»; la página de inicio de sesión de Mozaiklabs sigue el idioma y el tema de la interfaz.
- **Idiomas**: el noruego bokmål, el polaco y el griego llegan a Tune.
- **Android Auto**: Favoritos, Playlists, Radio, Cola y Reproduciendo, con los controles de la zona elegida (aún no probado en un coche).

## Mejoras

- **En el coche**: con Android Auto y con CarPlay, Tune Remote reproduce en «Este teléfono»; la zona de casa no se toca.
- **Radio**: se muestran las carátulas publicadas por las emisoras, con el logotipo como respaldo; el directorio se actualiza sin reiniciar Tune; la elección del flujo respeta las restricciones por país.
- **TIDAL**: la búsqueda de una versión Hi-Res continúa cuando la primera respuesta solo ofrece calidad CD, dentro de los límites de tu suscripción.
- **HQPlayer**: los cambios de pista son más fiables y se avisa de un reinicio que no arranca.
- **Devialet**: avanzar en una pista Qobuz Hi-Res continúa sin cortar el sonido; en Devialet AIR, el volumen observado en el aparato se muestra aparte.
- **Volumen**: el volumen y el silencio respetan las capacidades de cada salida; el asistente de IA solo sube el volumen en pasos pequeños.
- **Salidas**: un aparato que ofrece AirPlay y UPnP/DLNA aparece en ambas formas; ASIO y CoreAudio siguen mejor la cuadrícula de reasignación de canales.
- **Biblioteca**: lista compacta de aparatos; el filtro de Carpetas también se aplica a lo añadido recientemente; el historial agrupa las escuchas de una misma emisora.
- **Escucha remota**: el acceso por cuenta se configura sin código QR y los aparatos vinculados a la cuenta pueden retirarse.

## Correcciones

- **Reproducción**: un informe de posición antiguo ya no termina la pista tras un salto; los WAV muy cortos ya no interrumpen el encadenamiento; la reproducción continúa tras un rechazo temporal de cambio de frecuencia.
- **DLNA y zonas**: los reproductores DLNA reciben el FLAC multicanal anunciado; cambiar el aparato de una zona en reproducción detiene el anterior; una transferencia rechazada deja intacta la reproducción original.
- **TIDAL**: se corrigen errores de arranque causados por ciertos enlaces de audio.
- **Varios**: extracción de CD tras Stop, alarmas en orden, playlists nuevas visibles al momento, el aviso de actualización desaparece tras instalar, claves de ListenBrainz y Last.fm con PostgreSQL, copias de seguridad sin cachés HTTP.
- **Seguridad**: corrección del relé de escucha remota; los detalles se publicarán en un aviso aparte.

Spotify se entrega sin prueba de reproducción real: dinos en el foro si la reproducción funciona con tu cuenta.
