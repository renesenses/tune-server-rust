Tune 1.0.0-rc13.2, versión correctiva de la rc13.1.

## Correcciones

- Inicio de sesión: la contraseña de la cuenta de administrador se comprueba del mismo modo en el equipo del servidor y en la red local; mensaje de error más claro (usuario: default).
- Si ya no puede iniciar sesión, use el usuario default y la última contraseña introducida en el equipo del servidor.
- La cuenta mozaiklabs.fr ahora se puede vincular desde cualquier navegador de la red local (iPad, teléfono) cuando aún no hay ninguna cuenta vinculada.
- Desde la red local también se puede volver a conectar la misma cuenta. Sustituirla por otra cuenta, o desvincularla, sigue reservado al equipo del servidor o a un administrador. El nombre y la dirección de correo de la cuenta vinculada son visibles desde la red local.
- Windows: la actualización desde Tune vuelve a funcionar. Desde la rc13 y la rc13.1 había que usar el instalador.
- Servidores Tune OS que se quedaron en la rc12 y ya no se actualizan: la nueva herramienta «Débloquer Tune» (Windows, Mac) lo soluciona en unos pocos clics, sin ningún comando. Descarga: https://github.com/renesenses/tune-server-rust/releases/tag/debloquer-tune-v1
