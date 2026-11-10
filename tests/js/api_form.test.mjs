import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import vm from "node:vm";

const source = readFileSync(new URL("../../assets/js/ui/api_form.js", import.meta.url), "utf8");

function render(error) {
    const window = {};
    vm.runInNewContext(source, { window, document: {} });
    const banner = {
        innerHTML: "",
        classList: { remove() {} },
        insertAdjacentHTML(_, html) { this.innerHTML += html; },
        scrollIntoView() {},
    };
    const fields = [];
    window.smRenderApiError(banner, { error }, 400, { onField: field => fields.push(field) });
    return { html: banner.innerHTML, fields };
}

test("an SSRF block keeps the server's words and adds the way out, whichever field it lands on", () => {
    for (const field of ["check.url", "check.host", "check.resolver", "check.steps"]) {
        const message = "step 3: target address 10.0.0.53 is in a blocked range";
        const { html, fields } = render({ code: "SSRF_BLOCKED", message, field });
        assert.match(html, /step 3: target address 10\.0\.0\.53 is in a blocked range\. To monitor internal services, run a monitor agent inside that network\./);
        assert.doesNotMatch(html, /URL/);
        assert.deepEqual(fields, [field]);
    }
});

test("any other refusal shows the server's message", () => {
    const { html } = render({
        code: "INVALID_DNS_PARAMS",
        message: "dns resolver must be an IP or ip:port",
        field: "check.resolver",
    });
    assert.match(html, /<strong>INVALID_DNS_PARAMS<\/strong>: dns resolver must be an IP or ip:port/);
});

function api() {
    const window = {};
    vm.runInNewContext(source, { window, document: {} });
    return window;
}

function panel() {
    return { className: "", innerHTML: "", classList: { remove() {} } };
}

test("a result shows the error in plain words, never its terse code", () => {
    const el = panel();
    api().smRenderCheckResult(el, { status: "error", duration_ms: 12, error: "tls handshake reset" }, {
        errorText: "TLS handshake reset",
    });
    assert.match(el.innerHTML, /TLS handshake reset/);
    assert.doesNotMatch(el.innerHTML, /tls handshake reset/);
});

const ran = advice => ({ result: { status: advice ? "down" : "up" }, advice });
const unreached = { result: null, advice: null };

test("regions that fail the same way share their advice", () => {
    const ping = { source: "hint", code: "icmp_no_reply", text: "No reply to ping." };
    const agreed = api().smAgreedAdvice;
    assert.equal(agreed([ran(ping), ran({ ...ping })]), ping);
    assert.equal(agreed([ran(ping)]), ping);
});

test("one region passing or failing its own way leaves no advice", () => {
    const ping = { source: "hint", code: "icmp_no_reply", text: "No reply to ping." };
    const slow = { source: "hint", code: "no_answer", text: "The connection never completed." };
    const agreed = api().smAgreedAdvice;
    assert.equal(agreed([ran(ping), ran(null)]), null);
    assert.equal(agreed([ran(null), ran(ping)]), null);
    assert.equal(agreed([ran(ping), ran(slow)]), null);
    assert.equal(agreed([]), null);
});

test("a diagnosed block and a bare status are not the same advice", () => {
    const blocked = { source: "diagnostic", code: "access_interference", text: "use an authenticated health endpoint" };
    const refused = { source: "hint", code: "access_refused", text: "The server answered and refused the request." };
    const agreed = api().smAgreedAdvice;
    assert.equal(agreed([ran(blocked), ran(refused)]), null);
    assert.equal(agreed([ran(blocked), ran({ ...blocked })]), blocked);
});

test("a region whose request produced no result does not vote", () => {
    const ping = { source: "hint", code: "icmp_no_reply", text: "No reply to ping." };
    const agreed = api().smAgreedAdvice;
    assert.equal(agreed([unreached, ran(ping), ran({ ...ping })]), ping);
    assert.equal(agreed([ran(ping), unreached, ran(null)]), null);
    assert.equal(agreed([unreached, unreached]), null);
});
