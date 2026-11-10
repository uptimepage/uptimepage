import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import vm from "node:vm";

const read = name => readFileSync(new URL(`../../assets/js/ui/${name}`, import.meta.url), "utf8");

// The page under the button reads a status cached for seconds, and the result
// reaches this reply before it is stored, so no refresh is modelled here: the
// pill has only the reply to go on.
async function runCheckNow(reply) {
    const pill = {
        className: "",
        innerHTML: "",
        textContent: "",
        classList: { remove() {} },
        hasAttribute: () => true,
        setAttribute() {},
    };
    const button = {
        dataset: { targetId: "t-1" },
        disabled: false,
        addEventListener(_, handler) { this.click = handler; },
    };
    const found = { "[data-detail-test-now]": button, "[data-detail-test-result]": pill };
    const document = {
        querySelector: selector => found[selector] ?? null,
        querySelectorAll: () => [],
        getElementById: () => null,
        addEventListener() {},
        body: { addEventListener() {} },
    };
    const window = {};
    const context = { window, document, location: {}, setInterval() {}, Date, URL };
    vm.runInNewContext(read("api_form.js"), context);
    window.smRunCheckNow = async () => reply;
    vm.runInNewContext(read("target_detail.js"), context);
    await button.click();
    return pill;
}

test("a failed check now names its cause in the pill, with no page refresh to lean on", async () => {
    const pill = await runCheckNow({
        ok: true,
        status: 200,
        body: { status: "error", duration_ms: 41, error: "tls handshake reset", error_text: "TLS handshake reset" },
    });
    assert.match(pill.className, /test-result--bad/);
    assert.match(pill.innerHTML, /ERROR/);
    assert.match(pill.innerHTML, /TLS handshake reset/);
    assert.doesNotMatch(pill.innerHTML, /tls handshake reset/);
});

test("a passing check now shows no cause", async () => {
    const pill = await runCheckNow({ ok: true, status: 200, body: { status: "up", duration_ms: 9, response_code: 200 } });
    assert.match(pill.className, /test-result--ok/);
    assert.doesNotMatch(pill.innerHTML, /test-result__meta mt-1"/);
});
