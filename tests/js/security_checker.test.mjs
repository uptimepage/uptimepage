import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import vm from "node:vm";

class Element {
    constructor(tag = "div") { this.tagName = tag; this.children = []; this.dataset = {}; this.attributes = {}; this.value = ""; this.textContent = ""; }
    append(...children) { this.children.push(...children); }
    replaceChildren(...children) { this.children = children; }
    setAttribute(name, value) { this.attributes[name] = value; }
    addEventListener(name, fn) { this[name] = fn; }
    focus() { this.focused = true; }
}
const module = name => readFileSync(new URL(`../../assets/js/marketing/${name}.js`, import.meta.url), "utf8").replaceAll("export function", "function");
const checksSource = module("_security_checks") + "\n" + module("_redirect_chain");
const source = module("security_checker").replace(/^import .*;\n/gm, "");
function harness(fetcher, search = "") {
    const ids = Object.fromEntries(["security-form", "security-url", "security-submit", "security-status", "security-result"].map(id => [id, new Element()]));
    ids["security-form"].dataset = { sslProbe: "/ssl", headerProbe: "/headers" };
    ids["security-url"].value = "https://example.com/?secret=private";
    const events = [];
    const context = vm.createContext({ document: { getElementById: id => ids[id], createElement: tag => new Element(tag), createDocumentFragment: () => new Element("fragment") },
        window: { location: { search } }, fetch: fetcher, URL, URLSearchParams, AbortController, setTimeout, clearTimeout,
        toolUsed: (...args) => events.push(args), toolError: (...args) => events.push(args) });
    vm.runInContext(checksSource + "\n" + source, context);
    return { ids, context, events };
}
const nodes = root => [root, ...root.children.flatMap(nodes)];
const answer = { ok: true, final_url: "https://example.com/", final_status: 200, total_ms: 90, redirect_loop: false, hop_limit_hit: false, headers_truncated: false,
    headers: [["content-security-policy", "<img src=x onerror=alert(1)>"]], hops: [{ url: "https://example.com/", status: 200, ms: 90 }] };
const response = body => ({ ok: true, status: 200, json: async () => body });

test("partial results survive a TLS error, hostile values remain text, analytics omit input", async () => {
    const h = harness(async url => response(url.startsWith("/ssl") ? { ok: false, error: "TLS read failed" } : answer));
    await vm.runInContext("run()", h.context);
    const rendered = nodes(h.ids["security-result"]);
    assert(rendered.some(n => n.textContent.includes("<img src=x")));
    assert(!rendered.some(n => n.tagName === "img"));
    assert(rendered.some(n => n.textContent === "not checked"));
    assert(rendered.some(n => /^\d+ to review$/.test(n.textContent) && n.className.includes("headline")));
    assert(rendered.some(n => n.href === "/start?kind=tls_cert&url=example.com"));
    assert(rendered.some(n => n.textContent === "copy report"));
    assert(rendered.some(n => n.tagName === "details" && n.children.some(c => c.tagName === "ol")));
    assert(!JSON.stringify(h.events).includes("example.com"));
    assert(!JSON.stringify(h.events).includes("private"));
    assert.equal(h.ids["security-submit"].disabled, false);
    assert.equal(h.ids["security-form"].attributes["aria-busy"], "false");
});

test("double submission cannot start a second batch", async () => {
    let finish;
    let calls = 0;
    const pending = new Promise(resolve => { finish = resolve; });
    const h = harness(() => { calls++; return pending; });
    const running = vm.runInContext("run()", h.context);
    assert.equal(calls, 2);
    assert.equal(h.ids["security-submit"].disabled, true);
    await vm.runInContext("run()", h.context);
    assert.equal(calls, 2);
    finish(response({ ok: false, error: "Unavailable" }));
    await running;
    assert.equal(calls, 3);
    assert.equal(h.ids["security-submit"].disabled, false);
});

test("rate limits, invalid JSON, timeouts and invalid data render unknown, never pass", async () => {
    for (const fetcher of [
        async () => ({ ok: false, status: 429, json: async () => ({ error: "Wait a minute." }) }),
        async () => ({ ok: true, json: async () => { throw new Error("HTML"); } }),
        async () => { throw new Error("Offline"); },
        async () => { const e = new Error(); e.name = "AbortError"; throw e; },
        async () => response({ ok: true }),
    ]) {
        const h = harness(fetcher);
        await vm.runInContext("run()", h.context);
        const rendered = nodes(h.ids["security-result"]);
        assert(!rendered.some(n => n.textContent === "pass"));
        assert(!rendered.some(n => n.href?.startsWith("/start")));
        assert(!rendered.some(n => n.textContent === "copy report"));
        assert(rendered.some(n => /· 0 passed$/.test(n.textContent)));
        assert(rendered.some(n => /not checked$/.test(n.textContent) && n.className.includes("headline")));
        assert.equal(h.ids["security-submit"].disabled, false);
        assert.equal(h.ids["security-status"].textContent, "report ready");
    }
});

test("prefill does not trigger network requests and invalid input focuses the field", async () => {
    let calls = 0;
    const h = harness(() => { calls++; }, "?url=https%3A%2F%2Fexample.org%2F");
    assert.equal(h.ids["security-url"].value, "https://example.org/");
    assert.equal(calls, 0);
    h.ids["security-url"].value = "https://user:pass@example.com";
    await vm.runInContext("run()", h.context);
    assert.equal(calls, 0);
    assert.equal(h.ids["security-url"].attributes["aria-invalid"], "true");
    assert(h.ids["security-url"].focused);
});
