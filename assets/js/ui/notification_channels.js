(function () {
    const form = document.getElementById("channel-form");
    if (!form) return;

    const isEdit = form.dataset.mode === "edit";
    const replaceCb = form.querySelector("[data-replace-config]");
    const configFs = form.querySelector("[data-config]");
    const kindFs = form.querySelector("[data-kind-chooser]");
    const nameInput = form.querySelector("[data-name-input]");
    const resultEl = form.querySelector("[data-test-result]");

    function currentKind() {
        return form.querySelector("input[name='kind']:checked")?.value || "slack";
    }

    const storedKind = currentKind();

    // True when the form's transport config is live: create, or edit with
    // "replace config" on. Gates BOTH what save submits and what the test
    // button exercises — false routes the test to the stored config.
    function usesFormConfig() {
        return !isEdit || !!(replaceCb && replaceCb.checked);
    }

    function syncTestHint() {
        const hint = form.querySelector("[data-test-hint]");
        if (!hint) return;
        hint.textContent = usesFormConfig() ? hint.dataset.hintForm : hint.dataset.hintStored;
    }

    function hideTestResult() {
        resultEl?.classList.add("hidden");
    }

    function showStatus(el, text, cls) {
        el.textContent = text;
        el.className = `font-mono text-xs ${cls}`;
        el.classList.remove("hidden");
    }

    // margin 4 = the QR spec's full quiet zone, needed against the dark
    // page for picky scanners.
    function renderQr(el, url) {
        if (typeof qrcode !== "function") throw new Error("QR library failed to load — refresh and try again");
        smRenderQr(el, url, { margin: 4 });
    }

    // First text node only — the name span may carry disabled/managed chips.
    function cardName(el) {
        return el.querySelector(".check-type-card__name")?.childNodes[0]?.textContent.trim() || "monitor";
    }

    // Make `channelId`'s presence in the monitor's alert bindings match
    // `bound`. GET-then-PATCH replaces the whole alerts array from a fresh
    // snapshot — last write wins, same as the monitor form's own save.
    async function setBinding(channelId, targetId, bound) {
        const headers = {
            "Accept": "application/json",
            "X-Requested-With": "uptimepage",
        };
        const res = await fetch(`/api/v1/targets/${targetId}`, { headers });
        if (!res.ok) throw new Error(`monitor fetch failed (${res.status})`);
        let alerts = (await res.json()).alerts || [];
        const present = alerts.some((b) => b.channel_id === channelId);
        if (bound === present) return;
        if (bound) alerts.push({ channel_id: channelId });
        else alerts = alerts.filter((b) => b.channel_id !== channelId);
        const patch = await fetch(`/api/v1/targets/${targetId}`, {
            method: "PATCH",
            headers: { ...headers, "Content-Type": "application/json" },
            body: JSON.stringify({ alerts }),
        });
        if (!patch.ok) {
            throw new Error(await window.smApiErrorMessage(patch, `update failed (${patch.status})`));
        }
    }

    function showVariant(kind) {
        form.querySelectorAll("[data-variant]").forEach(el => {
            el.classList.toggle("hidden", el.dataset.variant !== kind);
        });
        syncAckToggle(kind);
        syncCentralTelegram(kind);
    }

    // The bot finishes a one-tap create; the form submits nothing for it.
    function oneTapCreate(kind) {
        return (kind === "telegram_app" || kind === "whatsapp_app") && !isEdit;
    }

    function syncAckToggle(kind) {
        const el = form.querySelector("[data-acknowledge-toggle]");
        if (!el) return;
        const offered = el.dataset.kinds.split(" ").includes(kind);
        el.classList.toggle("hidden", !offered || oneTapCreate(kind));
    }

    // SMS is one kind with a per-gateway sub-form; show only the picked one.
    function currentSmsProvider() {
        return form.querySelector("input[name='sms_provider']:checked")?.value || "twilio";
    }

    function showSmsProvider(provider) {
        form.querySelectorAll("[data-sms-provider]").forEach(el => {
            el.classList.toggle("hidden", el.dataset.smsProvider !== provider);
        });
    }

    // One-tap create has no submittable config — the webhook creates the
    // channel — so the submit/test/bind affordances yield to connect.
    function syncCentralTelegram(kind) {
        const hide = oneTapCreate(kind);
        form.querySelector("button[type=submit]")?.classList.toggle("hidden", hide);
        const testRow = form.querySelector("[data-send-test]")?.parentElement;
        testRow?.classList.toggle("hidden", hide);
        if (hide) hideTestResult();
        form.querySelector("#used-by")?.classList.toggle("hidden", hide);
    }

    function syncNamePlaceholder() {
        const kind = currentKind();
        const slug = { slack_app: "slack", discord_app: "discord", telegram_app: "telegram", whatsapp_app: "whatsapp" }[kind] || kind;
        if (nameInput) nameInput.placeholder = `ops-${slug}`;
    }

    // On edit the secret is never shown; the config inputs stay disabled
    // until the operator opts to replace the whole transport config. The
    // type cards lock with them (one fieldset.disabled flip) — the kind is
    // part of the config, so a diverged card with no config submit would
    // lie about what gets saved. The revert below is cosmetic: the server
    // derives the stored kind from the config on PATCH regardless.
    function syncConfigEnabled() {
        if (!isEdit) return;
        const on = !!(replaceCb && replaceCb.checked);
        configFs.querySelectorAll("input, textarea, select").forEach(el => {
            // The Acknowledge toggle is not part of the secret config, and a
            // connected channel's editable part never touches its secret.
            if (el === replaceCb || el.closest("[data-acknowledge-toggle], [data-managed-edit]")) return;
            el.disabled = !on;
        });
        if (kindFs) kindFs.disabled = !on;
        if (!on) {
            const stored = form.querySelector(`input[name='kind'][value='${storedKind}']`);
            if (stored) stored.checked = true;
            showVariant(storedKind);
            syncNamePlaceholder();
        }
        form.querySelector("[data-kind-locked-hint]")?.classList.toggle("hidden", on);
        syncTestHint();
    }

    function syncAll() {
        showVariant(currentKind());
        showSmsProvider(currentSmsProvider());
        syncNamePlaceholder();
        syncConfigEnabled();
        syncTestHint();
    }

    syncAll();
    // Browsers restore form-control state on reload / history navigation
    // without firing change events — re-derive everything from it.
    window.addEventListener("pageshow", syncAll);

    form.addEventListener("change", (evt) => {
        if (evt.target.name === "kind") {
            showVariant(currentKind());
            syncNamePlaceholder();
            hideTestResult();
        }
        if (evt.target.name === "sms_provider") {
            showSmsProvider(currentSmsProvider());
            hideTestResult();
        }
    });
    // A green "✓ delivered" must not vouch for a config edited after the
    // test ran.
    form.addEventListener("input", (evt) => {
        if (evt.target.closest("[data-config]") && !evt.target.closest("[data-acknowledge-toggle]")) {
            hideTestResult();
        }
    });
    if (replaceCb) replaceCb.addEventListener("change", syncConfigEnabled);

    // Auto-fill the channel name from the destination until the user types one
    // of their own; a prefilled (edit) name counts as already set.
    if (nameInput) {
        nameInput.dataset.userTouched = nameInput.value.trim() ? "1" : "";

        // The name input auto-grows via field-sizing where supported; mirror
        // its width manually otherwise so it never sits in a wide box.
        let autosizeName = () => {};
        if (!(window.CSS && CSS.supports && CSS.supports("field-sizing", "content"))) {
            const cs = getComputedStyle(nameInput);
            const mirror = document.createElement("span");
            mirror.setAttribute("aria-hidden", "true");
            mirror.style.cssText = "position:absolute;visibility:hidden;white-space:pre;";
            for (const p of ["fontFamily", "fontSize", "fontWeight", "letterSpacing"]) mirror.style[p] = cs[p];
            nameInput.insertAdjacentElement("afterend", mirror);
            autosizeName = () => {
                mirror.textContent = nameInput.value || nameInput.placeholder || "";
                const cap = nameInput.parentElement.clientWidth || 9999;
                nameInput.style.width = `${Math.min(mirror.offsetWidth + 6, cap)}px`;
            };
            nameInput.addEventListener("input", autosizeName);
            autosizeName();
        }

        nameInput.addEventListener("input", () => {
            nameInput.dataset.userTouched = nameInput.value.trim() ? "1" : "";
        });

        const SCHEME_RE = /^[a-z][a-z0-9+.-]*:\/\//i;
        const hostName = (raw) => {
            const v = (raw || "").trim();
            if (!v) return "";
            try {
                return new URL(SCHEME_RE.test(v) ? v : `https://${v}`).hostname.replace(/^www\./i, "");
            } catch {
                return "";
            }
        };
        const trimmed = (v) => (v || "").trim();
        // Each kind's most identifying destination field → a human name.
        const SOURCES = [
            ["slack_webhook_url", hostName],
            ["discord_webhook_url", hostName],
            ["msteams_webhook_url", hostName],
            ["google_chat_webhook_url", hostName],
            ["webhook_url", hostName],
            ["email_to", trimmed],
            ["ntfy_topic", trimmed],
            ["gotify_server_url", hostName],
            ["mattermost_webhook_url", hostName],
            ["whatsapp_to", trimmed],
            ["sms_to", trimmed],
        ];
        for (const [field, derive] of SOURCES) {
            const src = form.querySelector(`[name='${field}']`);
            src?.addEventListener("input", () => {
                if (nameInput.dataset.userTouched) return;
                const derived = derive(src.value);
                if (derived) {
                    nameInput.value = derived;
                    autosizeName();
                }
            });
        }
    }

    // "Test now": exercises the same notifier path a real incident uses.
    // Create / replace-config: POSTs the form's config without saving;
    // locked edit: tests the stored config by id.
    const testBtn = form.querySelector("[data-send-test]");
    if (testBtn) {
        const showResult = (text, cls) => showStatus(resultEl, text, cls);
        testBtn.addEventListener("click", async () => {
            clearErrors();
            let url = `${form.dataset.action}/test`;
            let body = null;
            if (usesFormConfig()) {
                const built = buildConfig();
                if (built.error) {
                    showResult(`✗ ${built.error}`, "flash-text flash-text--bad");
                    if (built.field) form.querySelector(`[name="${built.field}"]`)?.focus();
                    return;
                }
                // The prefilled "***" masks are not real secrets; the API
                // would reject them with PATCH-flavoured advice that the
                // test button can't follow.
                if (JSON.stringify(built.config).includes("***")) {
                    showResult(
                        isEdit
                            ? "✗ masked secrets (***) can't be tested — re-enter the real value, or untick \"Replace transport config\" to test the stored config"
                            : "✗ masked secrets (***) can't be tested — enter the real value",
                        "flash-text flash-text--bad",
                    );
                    return;
                }
                url = "/api/v1/notification-channels/test";
                body = JSON.stringify({ config: built.config });
            }
            testBtn.disabled = true;
            showResult("# sending test alert…", "text-quiet");
            try {
                const res = await fetch(url, {
                    method: "POST",
                    headers: {
                        "Content-Type": "application/json",
                        "Accept": "application/json",
                        "X-Requested-With": "uptimepage",
                    },
                    body,
                });
                if (res.ok) {
                    const enabledOn = !!form.querySelector("input[name='enabled']")?.checked;
                    showResult(
                        enabledOn
                            ? "✓ test alert delivered — check the destination"
                            : "✓ test alert delivered — note: with Enabled off, bound monitors won't alert through this channel",
                        "flash-text flash-text--ok font-medium",
                    );
                } else {
                    const msg = await window.smApiErrorMessage(res, `delivery failed (${res.status})`);
                    showResult(`✗ ${msg}`, "flash-text flash-text--bad font-medium");
                }
            } catch (err) {
                showResult(`✗ network error: ${err.message || err}`, "flash-text flash-text--bad font-medium");
            } finally {
                testBtn.disabled = false;
            }
        });
    }

    // Resend the verification mail for an unverified email channel (edit only).
    const resendBtn = form.querySelector("[data-resend-verification]");
    if (resendBtn) {
        const resendResult = form.querySelector("[data-resend-result]");
        const showResend = (text, cls) => showStatus(resendResult, text, cls);
        resendBtn.addEventListener("click", async () => {
            resendBtn.disabled = true;
            showResend("# sending…", "text-quiet");
            try {
                const res = await fetch(
                    `/api/v1/notification-channels/${resendBtn.dataset.channelId}/resend-verification`,
                    {
                        method: "POST",
                        headers: {
                            "Accept": "application/json",
                            "X-Requested-With": "uptimepage",
                        },
                    },
                );
                if (res.ok) {
                    showResend("✓ verification mail sent — check the inbox", "flash-text flash-text--ok font-medium");
                } else {
                    const msg = await window.smApiErrorMessage(res, `resend failed (${res.status})`);
                    showResend(`✗ ${msg}`, "flash-text flash-text--bad font-medium");
                }
            } catch (err) {
                showResend(`✗ network error: ${err.message || err}`, "flash-text flash-text--bad font-medium");
            } finally {
                resendBtn.disabled = false;
            }
        });
    }

    // Telegram setup helper: a t.me QR so the phone reaches the bot without
    // typing (the bot can't message anyone until they press Start), then a
    // chat-id probe over getUpdates. Both talk to the Bot API straight from
    // the browser with the token already sitting in the form.
    // ntfy: our side only publishes — the phone still has to SUBSCRIBE to
    // the topic. QR-first: the panel opens with a minted unguessable topic
    // (on ntfy.sh the name is the access control) and a live QR/link the
    // phone scans; the fields sit behind the advanced disclosure and the QR
    // follows them as they're edited.
    const ntfyBox = form.querySelector("[data-ntfy-qr-box]");
    if (ntfyBox) {
        const ntfyResult = form.querySelector("[data-ntfy-result]");
        const qrImg = form.querySelector("[data-ntfy-qr-img]");
        const qrLink = form.querySelector("[data-ntfy-qr-link]");
        const serverInput = form.querySelector("[name='ntfy_server_url']");
        const topicInput = form.querySelector("[name='ntfy_topic']");
        const suggestBtn = form.querySelector("[data-ntfy-suggest]");

        function suggestTopic() {
            const alphabet = "abcdefghijklmnopqrstuvwxyz0123456789";
            const bytes = new Uint8Array(12);
            crypto.getRandomValues(bytes);
            const suffix = Array.from(bytes, (b) => alphabet[b % alphabet.length]).join("");
            const base = (nameInput?.value || "")
                .toLowerCase()
                .replace(/[^a-z0-9]+/g, "-")
                .replace(/^-+|-+$/g, "")
                .slice(0, 24)
                .replace(/-+$/g, "");
            return `${base || "alerts"}-${suffix}`;
        }

        // Mint a topic when the panel opens empty, then keep the QR in sync
        // with whatever the fields hold.
        function syncNtfy() {
            if (currentKind() !== "ntfy") return;
            if (topicInput && !topicInput.disabled && usesFormConfig() && !topicInput.value.trim()) {
                topicInput.value = suggestTopic();
            }
            const topic = (topicInput?.value || "").trim();
            const server = ((serverInput?.value || "").trim() || "https://ntfy.sh").replace(/\/+$/, "");
            const url = `${server}/${topic}`;
            // Mirrors the server's https-only rule; also keeps a typo'd
            // scheme out of the link href.
            let parsed = null;
            try { parsed = new URL(url); } catch { /* shown below */ }
            if (!topic || !parsed || parsed.protocol !== "https:") {
                ntfyBox.classList.add("hidden");
                showStatus(
                    ntfyResult,
                    !topic ? "✗ enter a topic under advanced to get the QR" : "✗ the server URL must be a valid https:// URL",
                    "flash-text flash-text--bad",
                );
                return;
            }
            try {
                renderQr(qrImg, url);
            } catch (err) {
                ntfyBox.classList.add("hidden");
                showStatus(ntfyResult, `✗ ${err.message || err}`, "flash-text flash-text--bad");
                return;
            }
            qrLink.textContent = url;
            qrLink.href = url;
            ntfyResult?.classList.add("hidden");
            ntfyBox.classList.remove("hidden");
        }

        syncNtfy();
        window.addEventListener("pageshow", syncNtfy);
        form.addEventListener("change", (evt) => {
            if (evt.target.name === "kind" || evt.target === replaceCb) syncNtfy();
        });
        [serverInput, topicInput].forEach((el) => el?.addEventListener("input", syncNtfy));

        suggestBtn?.addEventListener("click", () => {
            // Locked edit: the input is disabled — writing into it anyway
            // would show a value that won't submit.
            if (!topicInput || topicInput.disabled) return;
            topicInput.value = suggestTopic();
            topicInput.dispatchEvent(new Event("input", { bubbles: true }));
        });
    }

    const tgQrBtn = form.querySelector("[data-tg-qr]");
    const tgDetectBtn = form.querySelector("[data-tg-detect]");
    if (tgQrBtn && tgDetectBtn) {
        const tgResult = form.querySelector("[data-tg-result]");
        const qrBox = form.querySelector("[data-tg-qr-box]");
        const qrImg = form.querySelector("[data-tg-qr-img]");
        const qrLink = form.querySelector("[data-tg-qr-link]");
        const tokenInput = form.querySelector("[name='telegram_bot_token']");
        const chatInput = form.querySelector("[name='telegram_chat_id']");
        const showTg = (text, cls) => showStatus(tgResult, text, cls);

        function tgToken() {
            const tok = (tokenInput?.value || "").trim();
            if (tok.includes("***")) {
                showTg("✗ the stored token is masked — tick \"Replace transport config\" and paste the real token first", "flash-text flash-text--bad");
                return null;
            }
            if (!/^\d+:[\w-]+$/.test(tok)) {
                showTg("✗ enter the bot token first (looks like 123456:ABC-DEF…)", "flash-text flash-text--bad");
                tokenInput?.focus();
                return null;
            }
            return tok;
        }

        function hideTgHelper() {
            tgResult?.classList.add("hidden");
            qrBox?.classList.add("hidden");
        }
        tokenInput?.addEventListener("input", hideTgHelper);
        form.addEventListener("change", (evt) => {
            if (evt.target.name === "kind") hideTgHelper();
        });

        async function tgApi(token, method) {
            const res = await fetch(`https://api.telegram.org/bot${token}/${method}`);
            const json = await res.json().catch(() => null);
            if (!json?.ok) {
                const desc = json?.description || `telegram api error (${res.status})`;
                if (/webhook/i.test(desc)) {
                    throw new Error("this bot has a webhook configured, which blocks the chat-id probe — use @userinfobot for the id, or delete the webhook if the bot is dedicated to alerts");
                }
                throw new Error(desc);
            }
            return json.result;
        }

        tgQrBtn.addEventListener("click", async () => {
            const token = tgToken();
            if (!token) return;
            tgQrBtn.disabled = true;
            showTg("# asking the bot api…", "text-quiet");
            try {
                const me = await tgApi(token, "getMe");
                if (!me.username) throw new Error("the bot has no username");
                const url = `https://t.me/${me.username}`;
                renderQr(qrImg, url);
                qrLink.textContent = url;
                qrLink.href = url;
                qrBox.classList.remove("hidden");
                showTg(`✓ @${me.username} — scan, press Start, then detect the chat ID`, "flash-text flash-text--ok font-medium");
            } catch (err) {
                showTg(`✗ ${err.message || err}`, "flash-text flash-text--bad font-medium");
            } finally {
                tgQrBtn.disabled = false;
            }
        });

        tgDetectBtn.addEventListener("click", async () => {
            const token = tgToken();
            if (!token) return;
            tgDetectBtn.disabled = true;
            showTg("# checking the bot's recent messages…", "text-quiet");
            try {
                // offset=-100 biases the window toward the newest pending
                // updates on busy bots.
                const updates = await tgApi(token, "getUpdates?offset=-100");
                const chats = new Map();
                for (const u of updates) {
                    // A my_chat_member update only counts when the bot was
                    // added — a kick/leave must not nominate that chat.
                    const joined = ["member", "administrator", "creator"]
                        .includes(u.my_chat_member?.new_chat_member?.status);
                    const c = u.message?.chat || u.channel_post?.chat
                        || (joined ? u.my_chat_member?.chat : null);
                    if (!c) continue;
                    // Re-insert so Map order is "last seen", not "first seen".
                    chats.delete(c.id);
                    chats.set(c.id, c);
                }
                if (!chats.size) {
                    throw new Error("no messages yet — press Start in the bot chat (or write anything in the group), then try again");
                }
                const chat = [...chats.values()].pop();
                chatInput.value = String(chat.id);
                chatInput.dispatchEvent(new Event("input", { bubbles: true }));
                const who = chat.title || (chat.username && `@${chat.username}`) || [chat.first_name, chat.last_name].filter(Boolean).join(" ");
                showTg(`✓ chat ID ${chat.id}${who ? ` (${who})` : ""} filled in`, "flash-text flash-text--ok font-medium");
            } catch (err) {
                showTg(`✗ ${err.message || err}`, "flash-text flash-text--bad font-medium");
            } finally {
                tgDetectBtn.disabled = false;
            }
        });
    }

    // One-tap connect (telegram, whatsapp): mint a code, show the deep
    // link + QR, poll until the provider webhook creates the channel, then
    // jump to it. `group` selectors are telegram-only.
    function initOneTapConnect(o) {
        const btn = form.querySelector(o.btn);
        if (!btn) return;
        const statusEl = form.querySelector(o.status);
        const qrBox = form.querySelector(o.qrBox);
        const qrImg = form.querySelector(o.qrImg);
        const linkEl = form.querySelector(o.link);
        const groupCb = o.group ? form.querySelector(o.group) : null;
        const show = (text, cls) => showStatus(statusEl, text, cls);
        const headers = { "Accept": "application/json", "X-Requested-With": "uptimepage" };
        let pollTimer = null;
        let mintedLinks = null;

        function reset() {
            if (pollTimer) clearInterval(pollTimer);
            pollTimer = null;
            mintedLinks = null;
            qrBox.classList.add("hidden");
            statusEl.classList.add("hidden");
            btn.disabled = false;
        }

        // One code serves both destinations; the toggle only swaps which
        // deep link (start vs startgroup) is shown.
        function renderDest() {
            if (!mintedLinks) return;
            const url = groupCb?.checked ? mintedLinks.group_deep_link : mintedLinks.deep_link;
            linkEl.textContent = url;
            linkEl.href = url;
            // The deep link works without the QR — degrade, don't dead-end.
            try {
                renderQr(qrImg, url);
                qrImg.classList.remove("hidden");
            } catch {
                qrImg.classList.add("hidden");
            }
            qrBox.classList.remove("hidden");
            show(groupCb?.checked ? o.waitGroupText : o.waitText, "text-quiet");
        }
        groupCb?.addEventListener("change", renderDest);
        form.addEventListener("change", (evt) => {
            if (evt.target.name === "kind") reset();
        });
        // bfcache restore: the poll handle is gone, the code may be stale.
        window.addEventListener("pageshow", (evt) => {
            if (evt.persisted) reset();
        });

        function pollLink(id) {
            pollTimer = setInterval(async () => {
                let body;
                try {
                    const res = await fetch(`${o.endpoint}/${id}`, { headers });
                    if (!res.ok) return; // transient; expiry resolves it
                    body = await res.json();
                } catch { return; }
                if (body.status === "consumed" && body.channel_id) {
                    clearInterval(pollTimer);
                    pollTimer = null;
                    show("✓ linked — opening the new channel…", "flash-text flash-text--ok font-medium");
                    window.location = `/settings/notifications/${body.channel_id}/edit`;
                } else if (body.status !== "pending") {
                    clearInterval(pollTimer);
                    pollTimer = null;
                    qrBox.classList.add("hidden");
                    btn.disabled = false;
                    show("✗ the link expired before it was used — connect again for a fresh one", "flash-text flash-text--bad font-medium");
                }
            }, 2000);
        }

        btn.addEventListener("click", async () => {
            clearErrors();
            reset();
            btn.disabled = true;
            show("# creating a single-use link…", "text-quiet");
            const name = (nameInput?.value || "").trim();
            let body;
            try {
                const res = await fetch(o.endpoint, {
                    method: "POST",
                    headers: { ...headers, "Content-Type": "application/json" },
                    body: JSON.stringify(name ? { name } : {}),
                });
                if (!res.ok) {
                    const msg = await window.smApiErrorMessage(res, `link failed (${res.status})`);
                    show(`✗ ${msg}`, "flash-text flash-text--bad font-medium");
                    btn.disabled = false;
                    return;
                }
                body = await res.json();
            } catch (err) {
                show(`✗ network error: ${err.message || err}`, "flash-text flash-text--bad font-medium");
                btn.disabled = false;
                return;
            }
            mintedLinks = body;
            renderDest();
            pollLink(body.id);
        });
    }

    initOneTapConnect({
        btn: "[data-tga-connect]",
        status: "[data-tga-status]",
        qrBox: "[data-tga-qr-box]",
        qrImg: "[data-tga-qr-img]",
        link: "[data-tga-link]",
        group: "[data-tga-group]",
        endpoint: "/api/v1/notification-channels/telegram-link",
        waitText: "# waiting — open the link (or scan) and press Start…",
        waitGroupText: "# waiting — open the link (or scan), pick the group, and confirm…",
    });
    initOneTapConnect({
        btn: "[data-waa-connect]",
        status: "[data-waa-status]",
        qrBox: "[data-waa-qr-box]",
        qrImg: "[data-waa-qr-img]",
        link: "[data-waa-link]",
        endpoint: "/api/v1/notification-channels/whatsapp-link",
        waitText: "# waiting — open the link (or scan) and send the prefilled message…",
    });

    // Provider OAuth connect ("add to Slack" / "add to Discord"): the
    // button is a full-page OAuth redirect; the QR variant fetches the same
    // single-use authorize URL for a phone that is signed in here. Callback
    // bounces land back with ?<provider>=<outcome>.
    form.querySelectorAll("[data-oauth-connect]").forEach((box) => {
        const provider = box.dataset.oauthConnect;
        const startUrl = box.dataset.oauthStart;
        const note = box.querySelector("[data-oauth-note]");
        const qrBox = box.querySelector("[data-oauth-qr-box]");
        const qrEl = box.querySelector("[data-oauth-qr-img]");
        const qrBtn = box.querySelector("[data-oauth-qr]");
        const showNote = (text, cls) => showStatus(note, text, cls);
        const outcome = new URLSearchParams(window.location.search).get(provider);
        if (outcome === "cancelled") {
            showNote(`# ${provider} connect cancelled — nothing was created`, "text-quiet");
        } else if (outcome === "quota") {
            showNote("✗ notification-channel limit reached for this plan", "flash-text flash-text--bad font-medium");
        } else if (outcome === "failed") {
            showNote(`✗ ${provider} connect failed — try again, or paste a webhook URL below`, "flash-text flash-text--bad font-medium");
        }
        qrBtn?.addEventListener("click", async () => {
            qrBtn.disabled = true;
            try {
                const res = await fetch(`${startUrl}?format=json`, {
                    headers: { "Accept": "application/json", "X-Requested-With": "uptimepage" },
                });
                if (!res.ok) throw new Error(`${provider} start failed (${res.status})`);
                const body = await res.json();
                renderQr(qrEl, body.url);
                qrBox.classList.remove("hidden");
                showNote("# scan with a phone that's signed in here and finish the consent screen — single-use, expires in 10 minutes", "text-quiet");
            } catch (err) {
                showNote(`✗ ${err.message || err}`, "flash-text flash-text--bad font-medium");
            } finally {
                qrBtn.disabled = false;
            }
        });
    });

    const submitBtn = form.querySelector("button[type=submit]");
    form.addEventListener("submit", async (evt) => {
        evt.preventDefault();
        if (submitBtn.disabled) return;
        clearErrors();
        const built = buildBody();
        if (built.error) {
            renderClientError(built.error);
            if (built.field) form.querySelector(`[name="${built.field}"]`)?.focus();
            return;
        }

        const label = submitBtn.textContent;
        submitBtn.disabled = true;
        submitBtn.textContent = "saving…";
        let navigating = false;
        try {
            let res;
            try {
                res = await fetch(form.dataset.action, {
                    method: form.dataset.method,
                    headers: {
                        "Content-Type": "application/json",
                        "Accept": "application/json",
                        "X-Requested-With": "uptimepage",
                    },
                    body: JSON.stringify(built.payload),
                });
            } catch (err) {
                renderClientError(`Network error: ${err.message || err}`);
                return;
            }
            if (res.ok) {
                if (!isEdit) {
                    const bound = await bindSelected(res);
                    if (bound.failed.length) {
                        // The channel exists now — a re-submit would dead-end
                        // on CHANNEL_NAME_TAKEN (or duplicate the channel
                        // under a tweaked name) and re-bind the already-bound
                        // picks. The edit page shows the true state.
                        if (bound.channelId) {
                            navigating = true;
                            window.location = `/settings/notifications/${bound.channelId}/edit`;
                            return;
                        }
                        renderClientError(
                            `Channel created, but binding failed for ${bound.failed.join(", ")} — open the channel from the list to finish.`,
                        );
                        return;
                    }
                }
                navigating = true;
                window.location = "/settings/notifications";
                return;
            }
            let body;
            try { body = await res.json(); }
            catch { renderClientError(`Request failed (${res.status})`); return; }
            renderApiError(body, res.status);
        } finally {
            if (!navigating) { submitBtn.disabled = false; submitBtn.textContent = label; }
        }
    });

    // Create-mode "bind to monitors": bind each picked monitor to the
    // channel just created. Returns the new channel id and descriptions of
    // the bindings that failed (empty = all good).
    async function bindSelected(res) {
        const picked = [...form.querySelectorAll("[data-used-by-grid] [data-bound-card]")];
        if (!picked.length) return { channelId: null, failed: [] };
        let channelId = null;
        try { channelId = (await res.json())?.id; } catch { /* handled below */ }
        if (!channelId) {
            return { channelId, failed: ["every monitor (the create response carried no channel id)"] };
        }
        const failed = [];
        for (const [i, card] of picked.entries()) {
            if (picked.length > 3) submitBtn.textContent = `binding ${i + 1}/${picked.length}…`;
            try {
                await setBinding(channelId, card.dataset.targetId, true);
            } catch (err) {
                failed.push(`${cardName(card)} (${err.message || err})`);
            }
        }
        return { channelId, failed };
    }

    function buildBody() {
        const data = new FormData(form);
        const name = (data.get("name") || "").trim();
        if (!name) return { error: "Name is required.", field: "name" };
        const payload = {
            name,
            enabled: data.get("enabled") === "on",
            acknowledge_button: data.get("acknowledge_button") === "on",
            // Always sent: an emptied field is how a rule is cleared.
            auto_bind_tags: ruleTags(),
        };

        // Edit + "replace config" unchecked: omit config so the stored
        // secret is preserved (the API rejects a re-submitted "***"). A
        // connected channel with a part people edit sends that part alone;
        // the API keeps the connection.
        if (usesFormConfig() || form.querySelector("[data-managed-edit]")) {
            const built = buildConfig();
            if (built.error) return built;
            payload.config = built.config;
        }
        return { payload };
    }

    // Chips, not a delimited field: a tag may contain a space or a comma.
    // Read from the DOM rather than the chip script, so a rule survives a save
    // even where that script never ran.
    const ruleChips = form.querySelector('[data-tag-chips="rule"]');
    function ruleTags() {
        if (!ruleChips) return [];
        return Array.from(ruleChips.querySelectorAll("[data-tag-pick]"))
            .filter((c) => c.checked)
            .map((c) => c.value);
    }

    function monitorTags(card) {
        try {
            return JSON.parse(card.dataset.tagsJson || "[]");
        } catch {
            return [];
        }
    }

    // A tag nothing carries otherwise reads as a working rule that pages nobody.
    const ruleMatch = form.querySelector("[data-rule-match]");
    if (ruleChips && ruleMatch) {
        const showMatches = () => {
            const rule = ruleTags();
            if (!rule.length) {
                ruleMatch.textContent = "";
                return;
            }
            let hits = 0;
            for (const card of form.querySelectorAll("[data-bound-card], [data-bind-monitor]")) {
                const tags = monitorTags(card);
                if (window.smTagRuleCovers && window.smTagRuleCovers(rule, tags)) hits += 1;
            }
            ruleMatch.textContent = hits === 1 ? "# matches 1 monitor" : `# matches ${hits} monitors`;
        };
        ruleChips.addEventListener("change", showMatches);
        new MutationObserver(showMatches).observe(ruleChips, { childList: true });
        showMatches();
    }

    // The transport config exactly as the API expects it, from the current
    // form values. Shared by save and "test now".
    function buildConfig() {
        const data = new FormData(form);
        const kind = currentKind();
        if (["slack", "discord", "msteams", "google_chat", "mattermost"].includes(kind)) {
            const config = {
                type: kind,
                webhook_url: (data.get(`${kind}_webhook_url`) || "").trim(),
            };
            const mention = (data.get(`${kind}_mention`) || "").trim();
            if (mention) config.mention = mention;
            return { config };
        }
        if (kind === "webhook") {
            let headers;
            try {
                const raw = (data.get("webhook_headers") || "").trim();
                headers = raw ? JSON.parse(raw) : {};
                if (typeof headers !== "object" || headers === null || Array.isArray(headers)) {
                    throw new Error("not an object");
                }
            } catch {
                return { error: "Headers must be a JSON object — e.g. {\"Authorization\": \"Bearer …\"}", field: "webhook_headers" };
            }
            const config = {
                type: "webhook",
                url: (data.get("webhook_url") || "").trim(),
                headers,
            };
            const secret = (data.get("webhook_secret") || "").trim();
            if (secret) {
                config.secret = secret;
            }
            return { config };
        }
        if (kind === "whatsapp") {
            const template = (data.get("whatsapp_template_name") || "").trim();
            if (!template) {
                return {
                    error: "Template name is required — WhatsApp silently drops template-less alerts outside the 24-hour service window.",
                    field: "whatsapp_template_name",
                };
            }
            const config = {
                type: "whatsapp",
                access_token: (data.get("whatsapp_access_token") || "").trim(),
                phone_number_id: (data.get("whatsapp_phone_number_id") || "").trim(),
                to: (data.get("whatsapp_to") || "").trim(),
                template_name: template,
            };
            const language = (data.get("whatsapp_language_code") || "").trim();
            if (language) config.language_code = language;
            return { config };
        }
        if (kind === "email") {
            return {
                config: {
                    type: "email",
                    to: (data.get("email_to") || "").trim().toLowerCase(),
                },
            };
        }
        if (kind === "pagerduty") {
            return {
                config: {
                    type: "pagerduty",
                    routing_key: (data.get("pagerduty_routing_key") || "").trim(),
                },
            };
        }
        if (kind === "ntfy") {
            const config = {
                type: "ntfy",
                server_url: (data.get("ntfy_server_url") || "").trim().replace(/\/+$/, ""),
                topic: (data.get("ntfy_topic") || "").trim(),
            };
            const token = (data.get("ntfy_access_token") || "").trim();
            if (token) config.access_token = token;
            return { config };
        }
        if (kind === "gotify") {
            return {
                config: {
                    type: "gotify",
                    server_url: (data.get("gotify_server_url") || "").trim().replace(/\/+$/, ""),
                    token: (data.get("gotify_token") || "").trim(),
                },
            };
        }
        if (kind === "sms") {
            const provider = currentSmsProvider();
            const to = (data.get("sms_to") || "").trim();
            if (provider === "telnyx") {
                const config = {
                    type: "sms",
                    provider: "telnyx",
                    to,
                    from: (data.get("sms_telnyx_from") || "").trim(),
                    api_key: (data.get("sms_telnyx_api_key") || "").trim(),
                };
                const mp = (data.get("sms_telnyx_messaging_profile_id") || "").trim();
                if (mp) config.messaging_profile_id = mp;
                return { config };
            }
            if (provider === "vonage") {
                return {
                    config: {
                        type: "sms",
                        provider: "vonage",
                        to,
                        from: (data.get("sms_vonage_from") || "").trim(),
                        api_key: (data.get("sms_vonage_api_key") || "").trim(),
                        api_secret: (data.get("sms_vonage_api_secret") || "").trim(),
                    },
                };
            }
            if (provider === "plivo") {
                return {
                    config: {
                        type: "sms",
                        provider: "plivo",
                        to,
                        from: (data.get("sms_plivo_from") || "").trim(),
                        auth_id: (data.get("sms_plivo_auth_id") || "").trim(),
                        auth_token: (data.get("sms_plivo_auth_token") || "").trim(),
                    },
                };
            }
            if (provider === "sinch") {
                return {
                    config: {
                        type: "sms",
                        provider: "sinch",
                        to,
                        from: (data.get("sms_sinch_from") || "").trim(),
                        service_plan_id: (data.get("sms_sinch_service_plan_id") || "").trim(),
                        api_token: (data.get("sms_sinch_api_token") || "").trim(),
                        region: (data.get("sms_sinch_region") || "us").trim(),
                    },
                };
            }
            return {
                config: {
                    type: "sms",
                    provider: "twilio",
                    to,
                    from: (data.get("sms_twilio_from") || "").trim(),
                    account_sid: (data.get("sms_twilio_account_sid") || "").trim(),
                    auth_token: (data.get("sms_twilio_auth_token") || "").trim(),
                },
            };
        }
        if (kind === "pushover") {
            const config = {
                type: "pushover",
                token: (data.get("pushover_token") || "").trim(),
                user: (data.get("pushover_user") || "").trim(),
            };
            const device = (data.get("pushover_device") || "").trim();
            if (device) config.device = device;
            config.emergency = data.get("pushover_emergency") === "on";
            return { config };
        }
        if (kind === "slack_app" || kind === "discord_app") {
            const config = { type: kind };
            const mention = (data.get(`${kind}_mention`) || "").trim();
            if (mention) config.mention = mention;
            return { config };
        }
        if (kind === "telegram_app") {
            // The API rejects this kind in request bodies.
            return { error: "Linked telegram channels have no config to submit — use \"connect telegram\", or untick \"Replace transport config\" to keep the stored link." };
        }
        if (kind === "whatsapp_app") {
            // The API rejects this kind in request bodies.
            return { error: "Linked whatsapp channels have no config to submit — use \"connect whatsapp\", or untick \"Replace transport config\" to keep the stored link." };
        }
        return {
            config: {
                type: "telegram",
                bot_token: (data.get("telegram_bot_token") || "").trim(),
                chat_id: (data.get("telegram_chat_id") || "").trim(),
            },
        };
    }

    // Monitor binding picker, both modes. Edit: each pick/unpick PATCHes
    // immediately. Create: picks are local until the submit handler binds
    // them. Cards move between the grids in place — no reload, so unsaved
    // form edits survive.
    const bindRoot = form.querySelector("[data-bind-root]");
    if (bindRoot) {
        const channelId = bindRoot.dataset.channelId; // empty on create
        const addBtn = bindRoot.querySelector("[data-add-monitor]");
        const picker = bindRoot.querySelector("#bind-picker");
        const bindResult = bindRoot.querySelector("[data-bind-result]");
        const usedByGrid = bindRoot.querySelector("[data-used-by-grid]");
        const bindGrid = bindRoot.querySelector("[data-bind-grid]");
        const searchInput = bindRoot.querySelector("[data-picker-search]");
        const showDisabledCb = bindRoot.querySelector("[data-picker-show-disabled]");
        const pagerEl = bindRoot.querySelector("[data-picker-pager]");
        const pagerInfo = bindRoot.querySelector("[data-pager-info]");
        const pagerPrev = bindRoot.querySelector("[data-pager-prev]");
        const pagerNext = bindRoot.querySelector("[data-pager-next]");
        const PAGE_SIZE = 20;
        let page = 0;
        const showBindResult = (text, cls) => showStatus(bindResult, text, cls);
        const setPickerOpen = (open) => {
            picker.classList.toggle("hidden", !open);
            addBtn.setAttribute("aria-expanded", String(open));
        };
        addBtn.addEventListener("click", () => {
            setPickerOpen(picker.classList.contains("hidden"));
        });

        // One derive-everything-from-DOM render: counts, empty-state notes,
        // and the search/show-disabled/pagination window.
        function applyPicker() {
            const bound = usedByGrid.querySelectorAll("[data-bound-card]").length;
            const all = [...bindGrid.querySelectorAll("[data-bind-monitor]")];
            const avail = all.length;

            const someEl = document.querySelector("[data-bound-some]");
            if (someEl) {
                someEl.classList.toggle("hidden", bound === 0);
                document.querySelector("[data-bound-none]")?.classList.toggle("hidden", bound > 0);
                const count = someEl.querySelector("[data-bound-count]");
                if (count) count.textContent = `${bound} monitor${bound === 1 ? "" : "s"}`;
            }
            bindRoot.querySelector("[data-used-by-note]")?.classList.toggle("hidden", bound > 0);
            bindRoot.querySelector("[data-picker-none]")?.classList.toggle("hidden", bound + avail > 0);
            bindRoot.querySelector("[data-picker-allbound]")?.classList.toggle("hidden", avail > 0 || bound === 0);
            bindRoot.querySelector("[data-picker-desc]")?.classList.toggle("hidden", avail === 0);
            bindRoot.querySelector("[data-picker-controls]")?.classList.toggle("hidden", avail === 0);
            bindGrid.classList.toggle("hidden", avail === 0);

            const q = (searchInput?.value || "").trim().toLowerCase();
            const showDisabled = !!showDisabledCb?.checked;
            const matched = all.filter((b) =>
                (showDisabled || b.dataset.enabled !== "false")
                && (!q || `${b.textContent} ${b.dataset.tags || ""}`.toLowerCase().includes(q)));
            const pages = Math.max(1, Math.ceil(matched.length / PAGE_SIZE));
            page = Math.min(page, pages - 1);
            const visible = new Set(matched.slice(page * PAGE_SIZE, (page + 1) * PAGE_SIZE));
            all.forEach((b) => b.classList.toggle("hidden", !visible.has(b)));
            pagerEl.classList.toggle("hidden", matched.length <= PAGE_SIZE);
            pagerInfo.textContent = `page ${page + 1}/${pages} · ${matched.length} match${matched.length === 1 ? "" : "es"}`;
            pagerPrev.disabled = page === 0;
            pagerNext.disabled = page >= pages - 1;
            bindRoot.querySelector("[data-picker-nomatch]")?.classList
                .toggle("hidden", matched.length > 0 || avail === 0);
        }

        // Enter in the picker search must not submit the form.
        searchInput?.addEventListener("keydown", (evt) => {
            if (evt.key === "Enter") evt.preventDefault();
        });
        // `/` jumps to the picker search (opening the picker if needed) —
        // same hotkey as the monitors list toolbar.
        document.addEventListener("keydown", (evt) => {
            if (evt.key !== "/" || evt.metaKey || evt.ctrlKey || evt.altKey) return;
            if (document.querySelector("dialog[open]")) return;
            const t = evt.target;
            if (t && (t.tagName === "INPUT" || t.tagName === "TEXTAREA" || t.isContentEditable)) return;
            if (!searchInput) return;
            evt.preventDefault();
            setPickerOpen(true);
            searchInput.focus();
            searchInput.select();
        });
        searchInput?.addEventListener("input", () => { page = 0; applyPicker(); });
        showDisabledCb?.addEventListener("change", () => { page = 0; applyPicker(); });
        pagerPrev?.addEventListener("click", () => { page -= 1; applyPicker(); });
        pagerNext?.addEventListener("click", () => { page += 1; applyPicker(); });
        applyPicker();

        function moveToUsedBy(btn) {
            const id = btn.dataset.targetId;
            const wrap = document.createElement("div");
            wrap.className = "relative min-w-0";
            wrap.dataset.boundCard = "";
            wrap.dataset.targetId = id;
            wrap.dataset.enabled = btn.dataset.enabled;
            wrap.dataset.tags = btn.dataset.tags || "";
            // The rule counter reads this one; dropping it on a rebuild makes
            // a bound monitor look untagged and undercounts the rule.
            wrap.dataset.tagsJson = btn.dataset.tagsJson || "[]";
            // On create the card must not navigate away mid-form; once the
            // binding is live (edit) it links to the monitor.
            let card;
            if (channelId) {
                card = document.createElement("a");
                card.href = `/targets/${id}/edit`;
            } else {
                card = document.createElement("div");
            }
            card.className = btn.className;
            card.classList.remove("text-left", "hidden");
            card.classList.add("h-full");
            card.innerHTML = btn.innerHTML;
            const unbind = document.createElement("button");
            unbind.type = "button";
            unbind.dataset.unbindMonitor = "";
            unbind.dataset.targetId = id;
            unbind.setAttribute("aria-label", `unbind ${cardName(btn)}`);
            unbind.title = channelId
                ? "unbind — stop alerting through this channel"
                : "remove — won't be bound when the channel is created";
            unbind.className = "unbind-btn";
            unbind.textContent = "×";
            wrap.append(card, unbind);
            usedByGrid.insertBefore(wrap, addBtn);
            btn.remove();
        }

        function moveToPicker(wrap) {
            const card = wrap.querySelector(".check-type-card");
            const btn = document.createElement("button");
            btn.type = "button";
            btn.dataset.bindMonitor = "";
            btn.dataset.targetId = wrap.dataset.targetId;
            btn.dataset.enabled = wrap.dataset.enabled;
            btn.dataset.tags = wrap.dataset.tags || "";
            btn.dataset.tagsJson = wrap.dataset.tagsJson || "[]";
            btn.className = card.className;
            btn.classList.remove("h-full");
            btn.classList.add("text-left");
            btn.innerHTML = card.innerHTML;
            const name = cardName(btn).toLowerCase();
            const after = [...bindGrid.querySelectorAll("[data-bind-monitor]")]
                .find((b) => cardName(b).toLowerCase() > name);
            bindGrid.insertBefore(btn, after || null);
            wrap.remove();
        }

        bindRoot.addEventListener("click", async (evt) => {
            const bindBtn = evt.target.closest("[data-bind-monitor]");
            const unbindBtn = evt.target.closest("[data-unbind-monitor]");
            if (!bindBtn && !unbindBtn) return;
            const name = cardName(bindBtn || unbindBtn.closest("[data-bound-card]"));

            // Create mode: local selection only — the submit handler binds.
            if (!channelId) {
                if (bindBtn) moveToUsedBy(bindBtn);
                else moveToPicker(unbindBtn.closest("[data-bound-card]"));
                bindResult.classList.add("hidden");
                applyPicker();
                return;
            }

            if (unbindBtn) {
                let body = `${name} stops alerting through this channel. You can re-add it any time.`;
                try {
                    const r = await fetch(`/api/v1/targets/${unbindBtn.dataset.targetId}`, {
                        headers: { "Accept": "application/json", "X-Requested-With": "uptimepage" },
                    });
                    if (r.ok && ((await r.json()).alerts || []).length === 1) {
                        body = `This is the only channel ${name} alerts through — unbinding silences it entirely. You can re-add it any time.`;
                    }
                } catch { /* default copy */ }
                const ok = await window.smConfirm({
                    title: "Unbind monitor?",
                    body,
                    confirmLabel: "unbind",
                    danger: true,
                });
                if (!ok) return;
            }
            bindRoot.querySelectorAll("[data-bind-monitor], [data-unbind-monitor]")
                .forEach((b) => { b.disabled = true; });
            showBindResult(bindBtn ? "# binding…" : "# unbinding…", "text-quiet");
            try {
                if (bindBtn) {
                    await setBinding(channelId, bindBtn.dataset.targetId, true);
                    moveToUsedBy(bindBtn);
                    showBindResult(`✓ ${name} now alerts through this channel`, "flash-text flash-text--ok font-medium");
                } else {
                    await setBinding(channelId, unbindBtn.dataset.targetId, false);
                    moveToPicker(unbindBtn.closest("[data-bound-card]"));
                    showBindResult(`✓ ${name} unbound — it no longer alerts through this channel`, "flash-text flash-text--ok font-medium");
                }
                applyPicker();
            } catch (err) {
                // Surface the failure even if the picker was collapsed
                // while the request was in flight.
                setPickerOpen(true);
                showBindResult(`✗ ${err.message || err}`, "flash-text flash-text--bad font-medium");
            } finally {
                bindRoot.querySelectorAll("[data-bind-monitor], [data-unbind-monitor]")
                    .forEach((b) => { b.disabled = false; });
            }
        });
    }

    function clearErrors() {
        window.smClearFormErrors(document.getElementById("form-errors"));
    }

    function renderClientError(msg) {
        window.smRenderClientError(document.getElementById("form-errors"), msg);
    }

    function renderApiError(json, status) {
        window.smRenderApiError(document.getElementById("form-errors"), json, status);
    }
})();
