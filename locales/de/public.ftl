## Public status page, its incident and archive pages, the subscribe flow and
## subscriber emails. Values are plain text: templates and emails escape them.

## Dates and numbers

month-1 = Januar
month-2 = Februar
month-3 = März
month-4 = April
month-5 = Mai
month-6 = Juni
month-7 = Juli
month-8 = August
month-9 = September
month-10 = Oktober
month-11 = November
month-12 = Dezember
month-short-1 = Jan.
month-short-2 = Feb.
month-short-3 = März
month-short-4 = Apr.
month-short-5 = Mai
month-short-6 = Juni
month-short-7 = Juli
month-short-8 = Aug.
month-short-9 = Sept.
month-short-10 = Okt.
month-short-11 = Nov.
month-short-12 = Dez.
date-month-year = { $month } { $year }
date-day = { $day }. { $month } { $year }
date-stamp = { $day }. { $month } { $year }, { $time } UTC
decimal-separator = ,
percent = { $value } %
duration-seconds = { $n } Sek.
duration-minutes = { $n } Min.
duration-hours = { $n } Std.
duration-days = { $n } Tg.
elapsed-ago = vor { $duration }
starts-in = (in { $duration })

## Page chrome

skip-to-content = Zum Hauptinhalt springen
title-status-suffix = Verfügbarkeit live und Störungsverlauf
title-archive-suffix = Vergangene Ausfälle
rss-link-title = Störungen (RSS)
rss-aria = Störungsmeldungen per RSS abonnieren
rss-title = { $page }: Störungen
rss-description = Betriebsstatus
rss-phase-investigating = Untersuchung läuft
rss-phase-identified = Ursache erkannt
rss-phase-monitoring = Beobachtung
rss-phase-resolved = Behoben
rss-phase-postmortem = Nachbericht
source-code = Quellcode (AGPL-3.0)
badge-operational = betriebsbereit
badge-minor = leichte Störung
badge-maintenance = Wartung
badge-partial = Teilausfall
badge-major = größerer Ausfall
badge-degraded = beeinträchtigt
badge-no-data = keine Daten
powered-by = Betrieben mit
licenses = Lizenzen
footer-updated = Aktualisiert
back-to-status = ← Zurück zur Statusseite
og-status-description = Aktueller und vergangener Status von { $name }: Verfügbarkeit jeder Komponente, offene und kürzliche Störungen, geplante Wartungsfenster sowie Benachrichtigungen per E-Mail oder Webhook.
og-incident-description = { $title }: aktuelle Phase, Beginn und Ende sowie alle Updates auf der Seite { $page }.
og-incident-description-affecting = { $title }, betrifft { $component }: aktuelle Phase, Beginn und Ende sowie alle Updates auf der Seite { $page }.
og-archive-description = Alle auf der Seite { $page } veröffentlichten Störungen, nach Monat gruppiert, mit den betroffenen Komponenten, Beginn und Ende sowie den veröffentlichten Updates.

## Status labels

overall-operational = Alle Systeme betriebsbereit
overall-maintenance = Wartung läuft
overall-minor = Leichte Störung
overall-partial = Teilweiser Systemausfall
overall-major = Größerer Systemausfall
overall-aria-operational = Alle Systeme betriebsbereit
overall-aria-maintenance = Wartung läuft
overall-aria-minor = Leichte Störung
overall-aria-partial = Teilweiser Systemausfall
overall-aria-major = Größerer Systemausfall
state-operational = Betriebsbereit
state-degraded = Beeinträchtigt
state-partial-outage = Teilausfall
state-major-outage = Größerer Ausfall
state-maintenance = Wartung
state-no-data = Keine Daten
phase-investigating = Untersuchung läuft
phase-identified = Ursache erkannt
phase-monitoring = Beobachtung
phase-resolved = Behoben
phase-postmortem = Nachbericht
auto-title = { $component }: { $status }
auto-title-down = ausgefallen
auto-title-degraded = beeinträchtigt
auto-title-error = Fehler
auto-title-generic = Dienststörung

## Status page

last-checked = Zuletzt geprüft
active-incidents-heading =
    { $count ->
        [one] Aktuelle Störung
       *[other] Aktuelle Störungen
    }
incident-started = Begonnen
view-incident = Störung ansehen →
maintenance-heading = Geplante Wartung
maintenance-in-progress = Läuft
maintenance-upcoming = Demnächst
maintenance-starts = Beginnt
maintenance-ends = endet
maintenance-affects = betrifft
legend-label = Farblegende: Ausfallzeit pro Tag
legend-no-downtime = Keine Ausfallzeit
legend-brief = Unter 20 Min.
legend-outage = 20–60 Min.
legend-extended = 1 Stunde oder mehr
legend-partial-note = Die Farbe zählt einen Teilausfall mit 30 % seiner Dauer und eine Beeinträchtigung gar nicht.
group-other = Sonstige
component-uptime-history = Verfügbarkeitsverlauf
component-uptime-history-sr = für { $name }, öffnet in neuem Tab
day-strip-label = Täglicher Statusverlauf für { $name }, letzte 90 Tage. Mit den Pfeiltasten zwischen den Tagen wechseln.
day-ago =
    { $days ->
        [0] heute
        [one] vor { $days } Tag
       *[other] vor { $days } Tagen
    }
strip-start = vor 90 Tagen
strip-uptime = Verfügbarkeit
strip-today = Heute
uptime-none = —
history-summary-no-data = { $days } Tage, keine Daten
history-summary-clean =
    { $days ->
        [one] { $days } Tag
       *[other] { $days } Tage
    }, keine Störungen
history-summary-degraded =
    { $days ->
        [one] { $days } Tag
       *[other] { $days } Tage
    }, { $degraded } beeinträchtigt
history-summary-outages =
    { $days ->
        [one] { $days } Tag
       *[other] { $days } Tage
    }, { $outages ->
        [one] { $outages } Ausfall
       *[other] { $outages } Ausfälle
    }, { $degraded } beeinträchtigt
no-components = Es wurden noch keine öffentlichen Komponenten eingerichtet.
past-incidents-heading = Vergangene Störungen (30 Tage)
older-incidents = Ältere Störungen →
past-none-recent = Keine Störungen in den letzten 7 Tagen.
past-earlier =
    { $count ->
        [one] 1 frühere Störung
       *[other] { $count } frühere Störungen
    }
past-incident-count =
    { $count ->
        [one] 1 Störung
       *[other] { $count } Störungen
    }
past-components-more = { $names } und { $count } weitere
popover-no-downtime = An diesem Tag wurde keine Ausfallzeit erfasst.
popover-related = Zugehörig

## Incidents

incident-ongoing = Andauernd
incident-ended = Beendet
incident-duration = Dauer
incident-updates = Updates
incident-no-updates = Zu dieser Störung wurden noch keine Updates veröffentlicht.
postmortem-heading = Nachbericht
postmortem-summary = Zusammenfassung
postmortem-root-cause = Ursache
postmortem-impact = Auswirkungen
postmortem-action-items = Maßnahmen
postmortem-published = Veröffentlicht
archive-heading = Störungsverlauf
archive-empty = Keine Störungen erfasst.

## Subscribe dialog

subscribe-title = Updates abonnieren
subscribe-close = Schließen
subscribe-lead = Lassen Sie sich benachrichtigen, sobald diese Seite eine Störung oder Wartung meldet.
subscribe-method = Zustellweg
subscribe-email = E-Mail
subscribe-webhook = Webhook
subscribe-email-label = E-Mail-Adresse
subscribe-webhook-label = Webhook-URL
subscribe-webhook-hint = Wir senden zuerst einen Bestätigungs-POST, danach JSON bei jedem Update.
subscribe-submit = Abonnieren
subscribe-privacy = Wird nur für die Updates dieser Seite verwendet. Jederzeit abbestellbar.
subscribe-feed-prompt = Lieber einen Feed?

## Subscribe flow

notice-copy = kopieren
notice-copied = kopiert
notice-copy-aria = In die Zwischenablage kopieren
notice-back = ← Zurück zur Statusseite
notice-page-not-found = Seite nicht gefunden
notice-page-not-found-body = Diese Statusseite ist nicht verfügbar.
notice-check-address = Adresse prüfen
notice-invalid-email = Das sieht nicht nach einer gültigen E-Mail-Adresse aus.
notice-blocked-email = Mit dieser Adresse ist kein Abonnement möglich.
email-risk-disposable = Das sieht nach einer Wegwerfadresse aus. Bitte verwenden Sie eine Adresse, die Sie auch später noch lesen können.
email-risk-no-mx = Diese Domain empfängt keine E-Mails, daher könnten wir Sie nicht erreichen. Bitte prüfen Sie die Schreibweise.
notice-almost-there = Fast geschafft
notice-check-inbox = Bitte prüfen Sie Ihren Posteingang und bestätigen Sie das Abonnement über den Link.
notice-link-expired = Link abgelaufen
notice-link-expired-body = Dieser Bestätigungslink ist ungültig, abgelaufen oder wurde bereits verwendet.
notice-subscribed = Abonniert
notice-subscribed-body = Sie werden benachrichtigt, sobald sich der Status dieser Seite ändert.
notice-check-url = URL prüfen
notice-invalid-url = Bitte geben Sie eine gültige https://-Webhook-URL ein.
notice-unreachable = Endpunkt nicht erreichbar
notice-unreachable-body = Wir konnten keinen Bestätigungs-POST an diese URL zustellen. Stellen Sie sicher, dass sie HTTPS-POST annimmt und mit 2xx antwortet, und versuchen Sie es dann erneut.
notice-webhook-subscribed = Abonniert
notice-webhook-subscribed-body = Ihr Endpunkt ist bestätigt. Speichern Sie dieses Signaturgeheimnis. Damit signieren wir unsere Anfragen, sodass Sie sie prüfen können:
unsubscribe-title = Abbestellen
unsubscribe-prompt = Keine Status-Updates mehr an diese Adresse senden? Sie können jederzeit erneut abonnieren.
unsubscribed-title = Abbestellt
unsubscribed-body = Sie erhalten keine weiteren Updates von dieser Statusseite. Sie können diese Seite schließen.
unsubscribe-invalid-title = Link ungültig
unsubscribe-invalid-body = Dieser Link ist ungültig oder abgelaufen.

## Subscriber emails

email-fallback-page-name = Statusseite
email-confirm-subject = Bestätigen Sie Ihr Abonnement für { $page }
email-confirm-heading = Abonnement bestätigen
email-confirm-preheader = Ein Klick, und die Status-Updates kommen hier an.
email-confirm-intro = Sie haben Status-Updates für { $page } auf { $site } angefordert.
email-confirm-lead = Sie haben Status-Updates für { $page } angefordert. Bestätigen Sie diese Adresse, dann kommen die Updates hier an.
email-confirm-cta = Bestätigen Sie diese Adresse, um Benachrichtigungen zu erhalten:
email-confirm-button = Abonnement bestätigen
email-confirm-expiry = Dieser Link ist { $hours } Stunden gültig und kann nur einmal verwendet werden.
email-confirm-not-you = Falls Sie das nicht angefordert haben, können Sie diese Adresse mit einem Klick entfernen:
email-confirm-footnote = Nicht angefordert? { $remove }, dann senden wir nichts an diese Adresse.
email-confirm-remove = Diese Adresse entfernen
email-status-line = Status: { $phase }
email-maintenance-scheduled = Geplante Wartung
email-maintenance-completed = Wartung abgeschlossen
email-maintenance-when = Zeitraum: { $window }
email-maintenance-window = Zeitraum
email-maintenance-ran = Durchgeführt
email-view-page = Statusseite ansehen:
email-view-page-button = Statusseite ansehen
email-unsubscribe = Abbestellen
email-unsubscribe-text = Abbestellen:
email-footnote = Sie erhalten diese E-Mail, weil Sie { $page } abonniert haben. { $unsubscribe }.
