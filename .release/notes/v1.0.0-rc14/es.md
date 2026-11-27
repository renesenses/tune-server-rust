Tune 1.0.0-rc14 incluye todo el contenido de las pre-releases rc13.90 y rc13.91; estas notas describen lo que ha cambiado desde la rc13.91. Gracias a los testers: muchos de estos puntos proceden de sus comentarios.

## Novedades

- **TESS, preajustes Guardar y Recargar**: en cuanto un ajuste se aparta del preajuste elegido, TESS muestra «modificado». **Guardar** sobrescribe el preajuste original, **Recargar** vuelve a él. Un preajuste incluido con Tune, como «Pure», no se sobrescribe: «Guardar como…» crea una copia.
- **TESS, preajuste DSD aplicado al instante**: un preajuste que cambia el objetivo DSD (por ejemplo DSD128 → DSD256), cargado en modo «aplicar inmediatamente», reinicia la pista en su posición, en lugar de esperar a la pista siguiente. El upsampling es una función Premium.
- **Headroom**: Automático, o a mano de −12 a 0 dB en pasos de 0,5 dB. El panel indica el valor realmente aplicado, o explica por qué no se aplica.
- **Headroom del upsampling a DSD**: el headroom manual ya no se aplica dos veces (−12 dB daba −24 dB; ahora son −15 dB, de los que −3 dB corresponden al modulador DSD, mostrados en su propia línea).
- **Medición de la sala guiada hacia 75 dB(C)**: con un micrófono calibrado, la pantalla muestra el nivel medido, su diferencia con 75 dB(C) y una indicación de volumen. La regla es prudente, porque la conversión aún no se ha verificado con un calibrador: Tune nunca aconseja superar 80 dB(C) estimados y corta la señal a 90 dB SPL. La corrección medida con micrófono es Premium; la calculada sin micrófono sigue siendo gratuita.
- **Seguridad auditiva**: desmarcar un ecualizador, pasar a PURE o a Bypass all, o quitar una convolución, un crossfeed o una mezcla a mono ya no hace subir el nivel de golpe: vuelve al nivel anterior con un fundido de aproximadamente medio segundo. Si ha subido el volumen mientras tanto, se enciende el indicador «nivel reducido» y «Restaurar nivel» libera el excedente. También vale para las salidas de red (UPnP, OAAT).
- **Ruta de la señal fiel**:
  - en una salida de red, sigue el orden real del flujo, hasta el volumen del reproductor;
  - la etapa de un filtro de respuesta al impulso se llama «Convolución»;
  - el filtro conserva el nombre del archivo .wav o .zip importado;
  - tras un cambio de objetivo DSD, muestra la frecuencia DSD que realmente se reproduce, y anuncia la siguiente;
  - la ventana pasa a verde: el naranja queda reservado a las alertas reales.
- **Convolución**: el panel muestra la curva del filtro cargado, canal por canal, y la fase a petición.
- **Bypass all**: el icono de TESS de la barra de reproducción aparece atenuado cuando TESS está omitido, y la página de TESS muestra «TESS omitido».
- **Página de TESS armonizada**: acento verde TESS, zona actual junto a la insignia, panel Headroom en gris en una salida de red, y medición «Sin micrófono (estimación)» o «Con micrófono (recomendado)».
- **Salida de red**: la tarjeta Upsampling indica que el upsampling no se aplica en ella.
- **Interfaz armonizada**: fuentes integradas, también sin conexión; Biblioteca, Live Radio, Servidores multimedia y Conciertos siguen el nuevo diseño.

## Correcciones

- **Radio hacia un Sonos** (y cualquier reproductor de red con un límite de frecuencia): una radio en 192 kHz se sirve a la frecuencia que acepta el reproductor. Antes, el reproductor se detenía enseguida, una y otra vez.
- **768 kHz**: la lista «Frecuencia máx.» de una zona ofrece 768 kHz y 1536 kHz.
- **Biblioteca**: la cuadrícula de álbumes ya no se congela al desplazarse por una colección grande.
- **Reproducción en la pestaña del navegador**: desplazarse dentro de la pista mueve realmente la reproducción, incluso desde otro dispositivo.
- **Ayuda**: el aviso del soporte prioritario ya no aparece en una cuenta Premium.
- **Licencia**: la fecha de caducidad se lee correctamente, y Ajustes › Licencia indica por qué el nivel es gratuito.
- **Convolución**: un filtro que amplifica ciertas frecuencias ya no satura.
- **Podcasts largos**: un episodio cuyo alojamiento corta la conexión con regularidad ya no se interrumpe.
- **Paquete Debian en un contenedor LXC** (Proxmox): el servicio arranca incluso sin espacios de nombres de montaje privados.
- **Primer inicio**: el asistente ya no llama «activados» a servicios sin cuenta, y guarda la carpeta de música elegida. «Mis radios» vacío abre el directorio.

## Seguridad y cuentas

- **Sesión renovada**: con «Mantener la sesión iniciada» (desmarcado por defecto), la sesión del navegador se renueva mientras se use, hasta 90 días. Si no, 24 h como antes. El menú del avatar ofrece «Cerrar sesión en este servidor».
- **Recuperar el acceso de administrador**: ejecute `tune-server --reset-admin` (o `--reinitialiser-admin`) **en la máquina del servidor**. El código de un solo uso obtenido, válido durante 15 minutos, se introduce en «¿Olvidaste la contraseña?» desde esa máquina; sin navegador, añada `--password-stdin`. Las sesiones abiertas se cierran. La guía de instalación (sección 9) indica el comando para cada sistema.
- **Perfil `admin`**: el perfil de administrador creado por defecto se llama ahora `admin`; un nombre elegido se conserva, y `default` sigue aceptándose.
- **Contraseña de administrador desde la primera configuración**: en una instalación nueva, el asistente le hace crearla.
- **Servidor sin autenticación**: la administración se acepta desde la red local, y ya no solo desde la máquina del servidor. **Advertencia**: sin autenticación, los dispositivos de la red local pueden administrar el servidor; actívela si su red no es de confianza. El ajuste «Administración solo desde esta máquina» restablece la regla anterior.

## Mandos a distancia

- En un servidor protegido, las apps que aún no están actualizadas vuelven a pedir la contraseña cada 24 h, como antes.

## Para comprobar en casa

- **«Bypass all» en un preajuste**: un preajuste que cambia a la vez Bypass all y el objetivo DSD, en modo «aplicar inmediatamente», reinicia la pista en su posición. Verificado solo en el banco de pruebas: díganos si la reanudación es limpia.
