## Public status page, its incident and archive pages, the subscribe flow and
## subscriber emails. Values are plain text: templates and emails escape them.

## Dates and numbers

month-1 = Janeiro
month-2 = Fevereiro
month-3 = Março
month-4 = Abril
month-5 = Maio
month-6 = Junho
month-7 = Julho
month-8 = Agosto
month-9 = Setembro
month-10 = Outubro
month-11 = Novembro
month-12 = Dezembro
month-short-1 = jan.
month-short-2 = fev.
month-short-3 = mar.
month-short-4 = abr.
month-short-5 = mai.
month-short-6 = jun.
month-short-7 = jul.
month-short-8 = ago.
month-short-9 = set.
month-short-10 = out.
month-short-11 = nov.
month-short-12 = dez.
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
percent = { $value }%
duration-seconds = { $n } s
duration-minutes = { $n } min
duration-hours = { $n } h
duration-days = { $n } d
elapsed-ago = há { $duration }
starts-in = (daqui a { $duration })

## Page chrome

skip-to-content = Saltar para o conteúdo principal
title-status-suffix = Disponibilidade em direto e histórico de incidentes
title-archive-suffix = Incidentes e manutenções anteriores
rss-link-title = Incidentes (RSS)
rss-aria = Subscrever as atualizações de incidentes por RSS
rss-title = { $page }: incidentes
rss-description = Estado do serviço
rss-phase-investigating = em investigação
rss-phase-identified = causa identificada
rss-phase-monitoring = em monitorização
rss-phase-resolved = resolvido
rss-phase-postmortem = análise pós-incidente
source-code = Código-fonte (AGPL-3.0)
badge-operational = operacional
badge-minor = perturbação ligeira
badge-maintenance = manutenção
badge-partial = interrupção parcial
badge-major = interrupção grave
badge-degraded = degradado
badge-no-data = sem dados
powered-by = Com tecnologia da
licenses = Licenças
footer-updated = Atualizado
back-to-status = ← Voltar ao estado
og-status-description = Estado atual e anterior de { $name }: disponibilidade de cada componente, incidentes abertos e recentes, manutenções programadas e atualizações por e-mail ou webhook.
og-incident-description = { $title }: fase atual, quando começou e terminou, e todas as atualizações publicadas na página { $page }.
og-incident-description-affecting = { $title }, que afeta { $component }: fase atual, quando começou e terminou, e todas as atualizações publicadas na página { $page }.
og-archive-description = Todos os incidentes e manutenções publicados na página { $page }, agrupados por mês, com os componentes afetados, quando cada um começou e terminou e as atualizações publicadas.

## Status labels

overall-operational = Todos os sistemas a funcionar normalmente
overall-maintenance = Manutenção em curso
overall-minor = Perturbação ligeira do serviço
overall-partial = Interrupção parcial do sistema
overall-major = Interrupção grave do sistema
overall-aria-operational = Todos os sistemas a funcionar normalmente
overall-aria-maintenance = Manutenção em curso
overall-aria-minor = Perturbação ligeira do serviço
overall-aria-partial = Interrupção parcial do sistema
overall-aria-major = Interrupção grave do sistema
state-operational = Operacional
state-degraded = Degradado
state-partial-outage = Interrupção parcial
state-major-outage = Interrupção grave
state-maintenance = Manutenção
state-no-data = Sem dados
phase-investigating = Em investigação
phase-identified = Causa identificada
phase-monitoring = Em monitorização
phase-resolved = Resolvido
phase-postmortem = Análise pós-incidente
auto-title = { $component }: { $status }
auto-title-down = indisponível
auto-title-degraded = desempenho degradado
auto-title-error = erro
auto-title-generic = Perturbação do serviço

## Status page

last-checked = Última verificação
active-incidents-heading =
    { $count ->
        [one] Incidente ativo
       *[other] Incidentes ativos
    }
incident-started = Início
view-incident = Ver incidente →
maintenance-heading = Manutenção programada
maintenance-in-progress = Em curso
maintenance-upcoming = Próxima
maintenance-completed = Concluída
maintenance-latest-update = Última atualização
maintenance-starts = Começa
maintenance-ends = termina
maintenance-affects = afeta
legend-label = Legenda de cores: tempo de inatividade por dia
legend-no-downtime = Sem inatividade
legend-brief = Menos de 20 min
legend-outage = 20–60 min
legend-extended = 1 hora ou mais
legend-partial-note = A cor conta uma interrupção parcial a 30% da sua duração e não conta o desempenho degradado.
group-other = Outros
component-uptime-history = histórico de disponibilidade
component-uptime-history-sr = de { $name }, abre num novo separador
day-strip-label = Histórico diário de estado de { $name }, últimos 90 dias. As teclas de seta mudam de dia.
day-ago =
    { $days ->
        [0] hoje
        [one] há { $days } dia
       *[other] há { $days } dias
    }
strip-start = há 90 dias
strip-uptime = disponibilidade
strip-today = Hoje
uptime-none = —
history-summary-no-data = { $days } dias, sem dados
history-summary-clean =
    { $days ->
        [one] { $days } dia
       *[other] { $days } dias
    }, sem incidentes
history-summary-degraded =
    { $days ->
        [one] { $days } dia
       *[other] { $days } dias
    }, { $degraded } com degradação
history-summary-outages =
    { $days ->
        [one] { $days } dia
       *[other] { $days } dias
    }, { $outages ->
        [one] { $outages } interrupção
       *[other] { $outages } interrupções
    }, { $degraded } com degradação
no-components = Ainda não foi configurado nenhum componente público.
history-heading = Histórico (30 dias)
history-tabs-label = Mostrar
history-tab =
    { $tab ->
        [incidents] Incidentes
        [maintenance] Manutenção
       *[all] Tudo
    }
history-none =
    { $tab ->
        [incidents] Sem incidentes resolvidos nos últimos 30 dias.
        [maintenance] Sem manutenções concluídas nos últimos 30 dias.
       *[all] Sem incidentes resolvidos nem manutenções concluídas nos últimos 30 dias.
    }
history-none-recent =
    { $tab ->
        [incidents] Sem incidentes resolvidos nos últimos 7 dias.
        [maintenance] Sem manutenções concluídas nos últimos 7 dias.
       *[all] Sem incidentes resolvidos nem manutenções concluídas nos últimos 7 dias.
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
                [one] 1 manutenção anterior
               *[other] { $count } manutenções anteriores
            }
       *[all]
            { $count ->
                [one] 1 entrada anterior
               *[other] { $count } entradas anteriores
            }
    }
history-full = Histórico completo →
history-older = Entradas anteriores →
past-incident-count =
    { $count ->
        [one] 1 incidente
       *[other] { $count } incidentes
    }
past-components-more = { $names } e mais { $count }
popover-no-downtime = Sem inatividade registada neste dia.
popover-related = Relacionado
popover-maintenance = Manutenção: { $title }

## Incidents

incident-ongoing = Em curso
incident-ended = Terminado
incident-duration = Duração
incident-updates = Atualizações
incident-no-updates = Ainda não foram publicadas atualizações sobre este incidente.
postmortem-heading = Análise pós-incidente
postmortem-summary = Resumo
postmortem-root-cause = Causa raiz
postmortem-impact = Impacto
postmortem-action-items = Ações
postmortem-published = Publicado
archive-heading =
    { $tab ->
        [incidents] Histórico de incidentes
        [maintenance] Histórico de manutenção
       *[all] Histórico
    }
archive-empty =
    { $tab ->
        [incidents] Sem incidentes registados.
        [maintenance] Sem manutenções registadas.
       *[all] Ainda nada registado.
    }

## Subscribe dialog

subscribe-title = Subscrever atualizações
subscribe-close = Fechar
subscribe-lead = Receba um aviso sempre que esta página publicar uma atualização de incidente ou de manutenção.
subscribe-method = Método de entrega
subscribe-email = E-mail
subscribe-webhook = Webhook
subscribe-email-label = Endereço de e-mail
subscribe-webhook-label = URL do webhook
subscribe-webhook-hint = Enviamos primeiro um POST de verificação e depois JSON a cada atualização.
subscribe-submit = Subscrever
subscribe-privacy = Usado apenas para enviar as atualizações desta página. Pode cancelar a subscrição a qualquer momento.
subscribe-feed-prompt = Prefere um feed?

## Subscribe flow

notice-copy = copiar
notice-copied = copiado
notice-copy-aria = Copiar para a área de transferência
notice-back = ← Voltar à página de estado
notice-page-not-found = Página não encontrada
notice-page-not-found-body = Esta página de estado não está disponível.
notice-check-address = Verifique o endereço
notice-invalid-email = Esse endereço de e-mail não parece válido.
notice-blocked-email = Esse endereço não pode ser usado para subscrever.
email-risk-disposable = Parece um endereço de e-mail temporário. Use um que continue a conseguir ler quando lhe enviarmos um alerta.
email-risk-no-mx = Esse domínio não aceita e-mail, por isso não conseguiríamos entrar em contacto consigo. Verifique se está bem escrito.
notice-almost-there = Quase a terminar
notice-check-inbox = Consulte a sua caixa de entrada: enviámos uma ligação de confirmação para concluir a subscrição.
notice-link-expired = Ligação expirada
notice-link-expired-body = Esta ligação de confirmação é inválida, expirou ou já foi usada.
notice-subscribed = Subscrição concluída
notice-subscribed-body = Receberá um aviso quando o estado desta página mudar.
notice-check-url = Verifique o URL
notice-invalid-url = Introduza um URL de webhook https:// válido.
notice-unreachable = Não foi possível contactar o seu endpoint
notice-unreachable-body = Não conseguimos entregar um POST de verificação nesse URL. Confirme que aceita POST por HTTPS e responde com 2xx e tente novamente.
notice-webhook-subscribed = Subscrição concluída
notice-webhook-subscribed-body = O seu endpoint está verificado. Guarde este segredo de assinatura. É com ele que assinamos os nossos pedidos, para que os possa verificar:
unsubscribe-title = Cancelar subscrição
unsubscribe-prompt = Deixar de receber atualizações de estado neste endereço? Pode voltar a subscrever a qualquer momento.
unsubscribed-title = Subscrição cancelada
unsubscribed-body = Não receberá mais atualizações desta página de estado. Já pode fechar esta página.
unsubscribe-invalid-title = Ligação inválida
unsubscribe-invalid-body = Esta ligação é inválida ou expirou.

## Subscriber emails

email-fallback-page-name = página de estado
email-confirm-subject = Confirme a sua subscrição de { $page }
email-confirm-heading = Confirme a sua subscrição
email-confirm-preheader = Um clique e as atualizações de estado começam a chegar aqui.
email-confirm-intro = Pediu para receber atualizações de estado de { $page } em { $site }.
email-confirm-lead = Pediu para receber atualizações de estado de { $page }. Confirme este endereço e as atualizações começam a chegar aqui.
email-confirm-cta = Confirme este endereço para começar a receber notificações:
email-confirm-button = Confirmar subscrição
email-confirm-expiry = Esta ligação expira em { $hours } horas e só pode ser usada uma vez.
email-confirm-not-you = Se não fez este pedido, pode remover este endereço com um clique:
email-confirm-footnote = Não fez este pedido? { $remove } e não lhe será enviado nada.
email-confirm-remove = Remova este endereço
email-status-line = Estado: { $phase }
email-outage-affected = { $count } componentes afetados
email-outage-names-more = { $names } e mais { $count }
email-outage-since = Desde { $at }
email-outage-started-lead = As verificações começaram a falhar a { $at }. Afetados até agora:
email-outage-resolved-lead = Tudo o que foi afetado voltou a funcionar. Durou { $duration }, de { $from } a { $to }.
email-outage-down = Indisponível
email-outage-degraded = Degradado
email-outage-was-down = indisponível durante { $duration }
email-maintenance-scheduled = Manutenção programada
email-maintenance-completed = Manutenção concluída
email-maintenance-update = Atualização de manutenção
email-maintenance-when = Quando: { $window }
email-maintenance-window = Período
email-maintenance-ran = Realizada
email-view-page = Ver a página de estado:
email-view-page-button = Ver página de estado
email-unsubscribe = Cancelar subscrição
email-unsubscribe-text = Cancelar subscrição:
email-footnote = Recebeu esta mensagem porque subscreveu { $page }. { $unsubscribe }.
email-credit = Página de estado da uptimepage, grátis para o seu próprio projeto

## Private page sign-in

access-title = Esta página é privada
access-lead = Inicie sessão com o seu e-mail profissional para ver o estado de { $page }.
access-email-label = Endereço de e-mail
access-send = Enviar-me um código por e-mail
access-or = ou
access-passkey = Iniciar sessão com uma chave de acesso
access-fine = Se este endereço puder abrir a página, vai receber um código. Caso contrário, nada é enviado.
access-check-title = Consulte a sua caixa de entrada
access-sent = Se { $email } puder abrir esta página, enviámos-lhe um código. Funciona uma vez e expira em { $minutes } minutos.
access-code-label = Código
access-sign-in = Iniciar sessão
access-one-try = Uma tentativa. Se falhar, a ligação do mesmo e-mail continua a funcionar.
access-again = Enviar outro
access-other = Usar outro e-mail
access-no-mail = Não recebeu o e-mail ao fim de alguns minutos? Peça a quem partilhou esta página consigo que confirme se o seu endereço está na lista.
access-pk-title = Dispensar o código da próxima vez?
access-pk-lead = Guarde uma chave de acesso no seu gestor de palavras-passe ou neste dispositivo. Da próxima vez, inicia sessão com ela sem precisar de e-mail.
access-pk-save = Guardar uma chave de acesso
access-pk-skip = Agora não
access-pk-failed = Não foi possível guardar a chave de acesso. A sua sessão continua iniciada.
access-ref-title = Esse código não funcionou
access-ref-lead = Cada código funciona uma vez. Peça um novo ou use a ligação do mesmo e-mail.
access-ref-again = Enviar um novo código
access-confirm-title = Abrir { $page }
access-confirm-lead = Continuar como { $email }?
access-confirm-button = Continuar
access-invalid-title = Essa ligação não funcionou
access-invalid-lead = Já foi usada, expirou ou foi enviada uma mais recente. Peça um novo código.
access-passkey-refused = Essa chave de acesso não funcionou aqui. Inicie sessão com o seu e-mail.
access-passkey-recheck = Já passou algum tempo. Confirme novamente o seu e-mail para continuar a usar a sua chave de acesso.
access-bad-email = Introduza o seu endereço de e-mail.
access-bad-code = Isso não é um código. Os códigos têm 6 letras e dígitos; nada foi gasto, por isso tente novamente.
access-signed-as = sessão iniciada como
access-sign-out = terminar sessão
access-lead-password = Introduza a palavra-passe da página para ver o estado de { $page }.
access-or-password = ou com a palavra-passe da página
access-password-label = Palavra-passe da página
access-password-open = Abrir
access-password-hint = Peça a palavra-passe a quem partilhou esta página consigo.
access-password-wrong = Essa palavra-passe não está correta.
access-password-throttled = Demasiadas tentativas falhadas a partir da sua rede. Tente novamente dentro de { $minutes ->
        [one] 1 minuto
       *[other] { $minutes } minutos
    }.
access-signed-password = sessão iniciada com a palavra-passe da página
subscribe-as-lead = Receba um e-mail quando um incidente começar, mudar ou terminar.
subscribe-as-note = Termina automaticamente se perder o acesso a esta página.
subscribe-password-note = Enviamos-lhe primeiro uma ligação por e-mail para confirmar. Se a palavra-passe da página mudar, subscreva novamente com a nova.
notice-subscribed-as-body = As atualizações desta página são agora enviadas para { $email }.

## Private page sign-in email

email-access-subject = O seu código para { $page }
email-access-heading = Iniciar sessão
email-access-preheader = Código de utilização única, válido durante { $minutes } minutos.
email-access-lead = Aqui está o seu código para abrir { $page }. Introduza-o no separador onde começou:
email-access-link = Ou abra a página a partir deste e-mail:
email-access-button = Abrir { $page }
email-access-expiry = O código e a ligação expiram em { $minutes } minutos e funcionam uma vez.
email-access-not-you = Se não pediu para abrir esta página, pode ignorar esta mensagem.
