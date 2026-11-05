## Public status page, its incident and archive pages, the subscribe flow and
## subscriber emails. Values are plain text: templates and emails escape them.

## Dates and numbers

month-1 = Enero
month-2 = Febrero
month-3 = Marzo
month-4 = Abril
month-5 = Mayo
month-6 = Junio
month-7 = Julio
month-8 = Agosto
month-9 = Septiembre
month-10 = Octubre
month-11 = Noviembre
month-12 = Diciembre
month-short-1 = ene
month-short-2 = feb
month-short-3 = mar
month-short-4 = abr
month-short-5 = may
month-short-6 = jun
month-short-7 = jul
month-short-8 = ago
month-short-9 = sept
month-short-10 = oct
month-short-11 = nov
month-short-12 = dic
date-month-year = { $month } de { $year }
date-day = { $day } { $month } { $year }
date-stamp = { $day } { $month } { $year }, { $time } { $zone }
zone-abbr-cet = CET
zone-abbr-cest = CEST
zone-abbr-eet = EET
zone-abbr-eest = EEST
zone-abbr-wet = WET
zone-abbr-west = WEST
decimal-separator = ,
percent = { $value } %
duration-seconds = { $n } s
duration-minutes = { $n } min
duration-hours = { $n } h
duration-days = { $n } d
elapsed-ago = hace { $duration }
starts-in = (en { $duration })

## Page chrome

skip-to-content = Saltar al contenido principal
title-status-suffix = Disponibilidad en vivo e historial de incidentes
title-archive-suffix = Incidentes y mantenimientos anteriores
rss-link-title = Incidentes (RSS)
rss-aria = Suscribirse a las actualizaciones de incidentes por RSS
rss-title = { $page }: incidentes
rss-description = Estado del servicio
rss-phase-investigating = investigación
rss-phase-identified = causa identificada
rss-phase-monitoring = seguimiento
rss-phase-resolved = resuelto
rss-phase-postmortem = postmortem
source-code = Código fuente (AGPL-3.0)
badge-operational = operativo
badge-minor = interrupción menor
badge-maintenance = mantenimiento
badge-partial = caída parcial
badge-major = caída grave
badge-degraded = degradado
badge-no-data = sin datos
powered-by = Con tecnología de
licenses = Licencias
footer-updated = Actualizado
back-to-status = ← Volver al estado
og-status-description = Estado actual y anterior de { $name }: disponibilidad de cada componente, incidentes abiertos y recientes, mantenimientos programados y actualizaciones por correo electrónico o webhook.
og-incident-description = { $title }: fase actual, cuándo empezó y terminó, y todas las actualizaciones publicadas en la página { $page }.
og-incident-description-affecting = { $title }, que afecta a { $component }: fase actual, cuándo empezó y terminó, y todas las actualizaciones publicadas en la página { $page }.
og-archive-description = Todos los incidentes y mantenimientos publicados en la página { $page }, agrupados por mes, con los componentes afectados, cuándo empezó y terminó cada uno y las actualizaciones publicadas.

## Status labels

overall-operational = Todos los sistemas funcionan con normalidad
overall-maintenance = Mantenimiento en curso
overall-minor = Interrupción menor del servicio
overall-partial = Caída parcial del sistema
overall-major = Caída grave del sistema
overall-aria-operational = Todos los sistemas funcionan con normalidad
overall-aria-maintenance = Mantenimiento en curso
overall-aria-minor = Interrupción menor del servicio
overall-aria-partial = Caída parcial del sistema
overall-aria-major = Caída grave del sistema
state-operational = Operativo
state-degraded = Degradado
state-partial-outage = Caída parcial
state-major-outage = Caída grave
state-maintenance = Mantenimiento
state-no-data = Sin datos
phase-investigating = Investigación
phase-identified = Causa identificada
phase-monitoring = Seguimiento
phase-resolved = Resuelto
phase-postmortem = Postmortem
auto-title = { $component }: { $status }
auto-title-down = no disponible
auto-title-degraded = rendimiento degradado
auto-title-error = error
auto-title-generic = Interrupción del servicio

## Status page

last-checked = Última comprobación
active-incidents-heading =
    { $count ->
        [one] Incidente activo
       *[other] Incidentes activos
    }
incident-started = Inicio
view-incident = Ver incidente →
maintenance-heading = Mantenimiento programado
maintenance-in-progress = En curso
maintenance-upcoming = Próximo
maintenance-completed = Completado
maintenance-latest-update = Última actualización
maintenance-starts = Empieza
maintenance-ends = termina
maintenance-affects = afecta a
legend-label = Leyenda de colores: tiempo de inactividad por día
legend-no-downtime = Sin inactividad
legend-brief = Menos de 20 min
legend-outage = 20–60 min
legend-extended = 1 hora o más
legend-partial-note = El color cuenta una caída parcial al 30 % de su duración y no cuenta el rendimiento degradado.
group-other = Otros
component-uptime-history = historial de disponibilidad
component-uptime-history-sr = de { $name }, se abre en una pestaña nueva
day-strip-label = Historial diario de estado de { $name }, últimos 90 días. Las teclas de flecha cambian de día.
day-ago =
    { $days ->
        [0] hoy
        [one] hace { $days } día
       *[other] hace { $days } días
    }
strip-start = hace 90 días
strip-uptime = disponibilidad
strip-today = Hoy
uptime-none = —
history-summary-no-data = { $days } días, sin datos
history-summary-clean =
    { $days ->
        [one] { $days } día
       *[other] { $days } días
    }, sin incidentes
history-summary-degraded =
    { $days ->
        [one] { $days } día
       *[other] { $days } días
    }, { $degraded } con degradación
history-summary-outages =
    { $days ->
        [one] { $days } día
       *[other] { $days } días
    }, { $outages ->
        [one] { $outages } caída
       *[other] { $outages } caídas
    }, { $degraded } con degradación
no-components = Todavía no se ha configurado ningún componente público.
history-heading = Historial (30 días)
history-tabs-label = Mostrar
history-tab =
    { $tab ->
        [incidents] Incidentes
        [maintenance] Mantenimiento
       *[all] Todo
    }
history-none =
    { $tab ->
        [incidents] Sin incidentes resueltos en los últimos 30 días.
        [maintenance] Sin mantenimientos completados en los últimos 30 días.
       *[all] Sin incidentes resueltos ni mantenimientos completados en los últimos 30 días.
    }
history-none-recent =
    { $tab ->
        [incidents] Sin incidentes resueltos en los últimos 7 días.
        [maintenance] Sin mantenimientos completados en los últimos 7 días.
       *[all] Sin incidentes resueltos ni mantenimientos completados en los últimos 7 días.
    }
history-earlier =
    { $tab ->
        [incidents]
            { $count ->
                [one] 1 incidente anterior
               *[other] { $count } incidentes anteriores
            }
        [maintenance]
            { $count ->
                [one] 1 mantenimiento anterior
               *[other] { $count } mantenimientos anteriores
            }
       *[all]
            { $count ->
                [one] 1 entrada anterior
               *[other] { $count } entradas anteriores
            }
    }
history-full = Historial completo →
history-older = Entradas anteriores →
past-incident-count =
    { $count ->
        [one] 1 incidente
       *[other] { $count } incidentes
    }
past-components-more = { $names } y { $count } más
popover-no-downtime = No se registró inactividad este día.
popover-related = Relacionado
popover-maintenance = Mantenimiento: { $title }

## Incidents

incident-ongoing = En curso
incident-ended = Finalizado
incident-duration = Duración
incident-updates = Actualizaciones
incident-no-updates = Todavía no se han publicado actualizaciones sobre este incidente.
postmortem-heading = Postmortem
postmortem-summary = Resumen
postmortem-root-cause = Causa raíz
postmortem-impact = Impacto
postmortem-action-items = Acciones
postmortem-published = Publicado
archive-heading =
    { $tab ->
        [incidents] Historial de incidentes
        [maintenance] Historial de mantenimiento
       *[all] Historial
    }
archive-empty =
    { $tab ->
        [incidents] No hay incidentes registrados.
        [maintenance] No hay mantenimientos registrados.
       *[all] Todavía no hay nada registrado.
    }

## Subscribe dialog

subscribe-title = Suscribirse a las actualizaciones
subscribe-close = Cerrar
subscribe-lead = Reciba un aviso cada vez que esta página publique una actualización de un incidente o de un mantenimiento.
subscribe-method = Método de entrega
subscribe-email = Correo electrónico
subscribe-webhook = Webhook
subscribe-email-label = Dirección de correo electrónico
subscribe-webhook-label = URL del webhook
subscribe-webhook-hint = Primero enviamos un POST de verificación y después JSON con cada actualización.
subscribe-submit = Suscribirse
subscribe-privacy = Solo se usa para enviar las actualizaciones de esta página. Puede cancelar la suscripción en cualquier momento.
subscribe-feed-prompt = ¿Prefiere un feed?

## Subscribe flow

notice-copy = copiar
notice-copied = copiado
notice-copy-aria = Copiar al portapapeles
notice-back = ← Volver a la página de estado
notice-page-not-found = Página no encontrada
notice-page-not-found-body = Esta página de estado no está disponible.
notice-check-address = Revise la dirección
notice-invalid-email = Esa dirección de correo electrónico no parece válida.
notice-blocked-email = Esa dirección no se puede usar para suscribirse.
email-risk-disposable = Parece una dirección de correo temporal. Use una que pueda seguir leyendo cuando le enviemos una alerta.
email-risk-no-mx = Ese dominio no acepta correo, así que no tendríamos forma de contactar con usted. Revise cómo está escrito.
notice-almost-there = Casi listo
notice-check-inbox = Revise su bandeja de entrada: encontrará un enlace de confirmación para terminar la suscripción.
notice-link-expired = Enlace caducado
notice-link-expired-body = Este enlace de confirmación no es válido, ha caducado o ya se ha usado.
notice-subscribed = Suscripción completada
notice-subscribed-body = Recibirá un aviso cuando cambie el estado de esta página.
notice-check-url = Revise la URL
notice-invalid-url = Escriba una URL de webhook https:// válida.
notice-unreachable = No pudimos contactar con su endpoint
notice-unreachable-body = No pudimos entregar un POST de verificación a esa URL. Asegúrese de que acepte POST por HTTPS y devuelva 2xx, y vuelva a intentarlo.
notice-webhook-subscribed = Suscripción completada
notice-webhook-subscribed-body = Su endpoint está verificado. Guarde este secreto de firma. Con él firmamos nuestras solicitudes para que pueda verificarlas:
unsubscribe-title = Cancelar la suscripción
unsubscribe-prompt = ¿Dejar de recibir actualizaciones de estado en esta dirección? Puede volver a suscribirse cuando quiera.
unsubscribed-title = Suscripción cancelada
unsubscribed-body = No recibirá más actualizaciones de esta página de estado. Ya puede cerrar esta página.
unsubscribe-invalid-title = Enlace no válido
unsubscribe-invalid-body = Este enlace no es válido o ha caducado.

## Subscriber emails

email-fallback-page-name = página de estado
email-confirm-subject = Confirme su suscripción a { $page }
email-confirm-heading = Confirme su suscripción
email-confirm-preheader = Un clic y las actualizaciones de estado empezarán a llegar aquí.
email-confirm-intro = Pidió recibir actualizaciones de estado de { $page } en { $site }.
email-confirm-lead = Pidió recibir actualizaciones de estado de { $page }. Confirme esta dirección y empezarán a llegar aquí.
email-confirm-cta = Confirme esta dirección para empezar a recibir avisos:
email-confirm-button = Confirmar suscripción
email-confirm-expiry = Este enlace caduca en { $hours } horas y solo se puede usar una vez.
email-confirm-not-you = Si no lo pidió usted, puede eliminar esta dirección con un clic:
email-confirm-footnote = ¿No lo pidió usted? { $remove } y no se le enviará nada.
email-confirm-remove = Elimine esta dirección
email-status-line = Estado: { $phase }
email-outage-affected = { $count } componentes afectados
email-outage-names-more = { $names } y { $count } más
email-outage-since = Desde el { $at }
email-outage-started-lead = Las comprobaciones empezaron a fallar el { $at }. Afectados hasta ahora:
email-outage-resolved-lead = Todo lo afectado vuelve a funcionar. Duró { $duration }, del { $from } al { $to }.
email-outage-down = No disponible
email-outage-degraded = Degradado
email-outage-was-down = no disponible durante { $duration }
email-maintenance-scheduled = Mantenimiento programado
email-maintenance-completed = Mantenimiento completado
email-maintenance-update = Actualización de mantenimiento
email-maintenance-when = Cuándo: { $window }
email-maintenance-window = Período
email-maintenance-ran = Realizado
email-view-page = Ver la página de estado:
email-view-page-button = Ver página de estado
email-unsubscribe = Cancelar la suscripción
email-unsubscribe-text = Cancelar la suscripción:
email-footnote = Recibe este mensaje porque se suscribió a { $page }. { $unsubscribe }.
email-credit = Página de estado de uptimepage, gratis para su propio proyecto

## Private page sign-in

access-title = Esta página es privada
access-lead = Inicie sesión con su correo electrónico de trabajo para ver el estado de { $page }.
access-email-label = Dirección de correo electrónico
access-send = Enviarme un código por correo
access-or = o
access-passkey = Iniciar sesión con una llave de acceso
access-fine = Si esta dirección puede abrir la página, le enviamos un código. Si no, no se envía nada.
access-check-title = Revise su bandeja de entrada
access-sent = Si { $email } puede abrir esta página, le enviamos un código. Funciona una vez y caduca en { $minutes } minutos.
access-code-label = Código
access-sign-in = Iniciar sesión
access-one-try = Un solo intento. Si falla, el enlace del mismo correo sigue funcionando.
access-again = Enviar otro
access-other = Usar otro correo electrónico
access-no-mail = ¿No llega el correo después de unos minutos? Pida a quien compartió esta página con usted que compruebe que su dirección está en la lista.
access-pk-title = ¿Omitir el código la próxima vez?
access-pk-lead = Guarde una llave de acceso en su gestor de contraseñas o en este dispositivo. La próxima vez iniciará sesión con ella, sin necesidad de correo.
access-pk-save = Guardar una llave de acceso
access-pk-skip = Ahora no
access-pk-failed = No se pudo guardar la llave de acceso. Su sesión sigue iniciada.
access-ref-title = Ese código no funcionó
access-ref-lead = Cada código funciona una sola vez. Pida uno nuevo o use el enlace del mismo correo.
access-ref-again = Enviar un código nuevo
access-confirm-title = Abrir { $page }
access-confirm-lead = ¿Continuar como { $email }?
access-confirm-button = Continuar
access-invalid-title = Ese enlace no funcionó
access-invalid-lead = Ya se usó, ha caducado o se envió uno más reciente. Pida un código nuevo.
access-passkey-refused = Esa llave de acceso no funcionó aquí. Inicie sesión con su correo electrónico.
access-passkey-recheck = Ha pasado un tiempo. Confirme su correo electrónico una vez más para seguir usando su llave de acceso.
access-bad-email = Escriba su dirección de correo electrónico.
access-bad-code = Eso no es un código. Los códigos tienen 6 letras y dígitos; no se ha gastado nada, así que vuelva a intentarlo.
access-signed-as = sesión iniciada como
access-sign-out = cerrar sesión
access-lead-password = Escriba la contraseña de la página para ver el estado de { $page }.
access-or-password = o con la contraseña de la página
access-password-label = Contraseña de la página
access-password-open = Abrir
access-password-hint = Pida la contraseña a quien compartió esta página con usted.
access-password-wrong = Esa contraseña no es correcta.
access-password-throttled = Demasiados intentos fallidos desde su red. Vuelva a intentarlo en { $minutes ->
        [one] 1 minuto
       *[other] { $minutes } minutos
    }.
access-signed-password = sesión iniciada con la contraseña de la página
subscribe-as-lead = Reciba un correo cuando un incidente empiece, cambie o termine.
subscribe-as-note = Termina automáticamente si pierde el acceso a esta página.
subscribe-password-note = Primero le enviamos un enlace por correo para confirmar. Si cambia la contraseña de la página, vuelva a suscribirse con la nueva.
notice-subscribed-as-body = Las actualizaciones de esta página ahora se envían a { $email }.

## Private page sign-in email

email-access-subject = Su código para { $page }
email-access-heading = Iniciar sesión
email-access-preheader = Código de un solo uso, válido durante { $minutes } minutos.
email-access-lead = Este es su código para abrir { $page }. Escríbalo en la pestaña donde empezó:
email-access-link = O abra la página desde este correo:
email-access-button = Abrir { $page }
email-access-expiry = El código y el enlace caducan en { $minutes } minutos y funcionan una sola vez.
email-access-not-you = Si no pidió abrir esta página, puede ignorar este mensaje.
