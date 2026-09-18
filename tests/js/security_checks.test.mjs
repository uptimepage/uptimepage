import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

const source = readFileSync(new URL("../../assets/js/marketing/_security_checks.js", import.meta.url), "utf8");
const { assess, targetURLs, validReport } = await import(`data:text/javascript;base64,${Buffer.from(source).toString("base64")}`);
const target = targetURLs("example.com/account");
const cert = { ok: true, host: "example.com", days_remaining: 60, expired: false, name_matches: true,
    not_before: "2026-09-01T00:00:00Z", not_after: "2026-12-01T00:00:00Z", chain_len: 1, self_signed: false };
const headers = [
    ["strict-transport-security", "max-age=31536000; includeSubDomains"],
    ["content-security-policy", "default-src 'self'; object-src 'none'; base-uri 'self'; frame-ancestors 'none'"],
    ["x-content-type-options", "nosniff"], ["referrer-policy", "strict-origin-when-cross-origin"],
];
function web(overrides = {}) {
    return { ok: true, final_url: target.https, final_status: 200, total_ms: 120, redirect_loop: false, hop_limit_hit: false,
        headers_truncated: false, headers, hops: [{ url: target.https, status: 200, ms: 120 }], ...overrides };
}
function checks(r = web(), c = cert, h = web({ hops: [{ url: target.http, status: 301, ms: 40 }, { url: target.https, status: 200, ms: 80 }] })) {
    return assess(target, c ? { report: c } : { error: "TLS read failed" }, r ? { report: r } : { error: "HTTPS unavailable" },
        h ? { report: h } : { error: "Rate limited" }, Date.parse("2026-09-18T00:00:00Z"));
}
const byId = (list, id) => list.find(c => c.id === id);

test("normalizes URL and rejects credentials, non-web schemes, IPs and alternate ports", () => {
    assert.deepEqual(targetURLs(" http://EXAMPLE.com:80/a?q=1#fragment "), { host: "example.com", https: "https://example.com/a?q=1", http: "http://example.com/a?q=1" });
    for (const raw of ["", "localhost", "127.0.0.1", "https://[::1]/", "https://user:pass@example.com", "ftp://example.com", "example.com:8443", "example.com:80", "https://bad_.example.com", "https://example.com:80"]) {
        assert.throws(() => targetURLs(raw), undefined, raw);
    }
});

test("a complete report passes scoped checks without inventing missing intermediates", () => {
    assert(checks().every(c => c.status === "pass"));
    assert.match(byId(checks(), "trust").advice, /chain length alone does not prove completeness/);
});

test("expired within the current day, future certificates and hostname mismatch fail first", () => {
    const result = checks(web(), { ...cert, days_remaining: 0, expired: true, name_matches: false });
    assert.equal(result[0].status, "fail");
    assert.equal(byId(result, "certificate").status, "fail");
    assert.equal(byId(result, "hostname").status, "fail");
    assert.equal(byId(checks(web(), { ...cert, not_before: "2026-10-01T00:00:00Z" }), "certificate").status, "fail");
    assert.equal(byId(checks(web(), { ...cert, days_remaining: 0 }), "certificate").status, "warning");
});

test("unavailable probes yield unknowns; certificate evidence survives HTTP failure", () => {
    assert(checks(null, null, null).every(c => c.status === "unknown"));
    const result = checks(null);
    assert.equal(byId(result, "certificate").status, "pass");
    assert.equal(byId(result, "trust").status, "unknown");
    assert.equal(byId(result, "hsts").status, "unknown");
});

test("a rejected TLS handshake fails validation even when the unvalidated read looks fine", () => {
    const rejected = assess(target, { report: cert }, { error: "The host refused the TLS handshake. Its certificate may be expired, self-signed or missing an intermediate." },
        { report: web({ hops: [{ url: target.http, status: 301, ms: 1 }, { url: target.https, status: 200, ms: 1 }] }) }, Date.parse("2026-09-18T00:00:00Z"));
    assert.equal(rejected[0].id, "trust");
    assert.equal(rejected[0].status, "fail");
    assert.equal(byId(rejected, "certificate").status, "pass");
    assert.equal(byId(rejected, "https").status, "unknown");
    assert.equal(byId(rejected, "csp").status, "unknown");
    assert.equal(byId(assess(target, { report: cert }, { error: "The host did not answer in time." }, { error: "x" }), "trust").status, "unknown");
});

test("downgrades that later return to HTTPS still fail", () => {
    const chain = web({ hops: [{ url: target.https, status: 302, ms: 1 }, { url: target.http, status: 301, ms: 1 }, { url: target.https, status: 200, ms: 1 }] });
    assert.equal(byId(checks(chain), "https").status, "fail");
    assert.equal(byId(checks(web(), cert, chain), "http").status, "fail");
    assert.equal(byId(checks(web({ final_url: target.http })), "hsts").status, "unknown");
});

test("loops and hop limits do not become missing-header warnings", () => {
    for (const flag of ["redirect_loop", "hop_limit_hit"]) {
        const result = checks(web({ [flag]: true, headers: [] }));
        assert.equal(byId(result, "https").status, "fail");
        assert.equal(byId(result, "csp").status, "unknown");
    }
});

test("truncated lists, cut values and duplicate policies cannot pass", () => {
    assert.equal(byId(checks(web({ headers_truncated: true })), "hsts").status, "unknown");
    assert.equal(byId(checks(web({ headers: [["content-security-policy", "default-src 'self'…"]] })), "csp").status, "unknown");
    assert.equal(byId(checks(web({ headers: [...headers, headers[1]] })), "csp").status, "unknown");
    assert.equal(byId(checks(web({ headers: [["content-security-policy", "default-src 'self', script-src *"]] })), "csp").status, "unknown");
});

test("disabled, malformed and short HSTS receive actionable warnings", () => {
    for (const value of ["max-age=0", "max-age=60", "max-age=no", "max-age=\"31536000", "max-age=31536000; max-age=0", "max-age=31536000, max-age=0"]) {
        assert.equal(byId(checks(web({ headers: [["strict-transport-security", value]] })), "hsts").status, "warning", value);
    }
});

test("report-only CSP and permissive script policies do not pass", () => {
    assert.equal(byId(checks(web({ headers: [["content-security-policy-report-only", headers[1][1]]] })), "csp").status, "warning");
    for (const value of ["default-src *", "script-src 'unsafe-inline'; object-src 'none'; base-uri 'self'", "frame-ancestors 'none'", "default-src 'self'; script-src-elem https:; object-src 'none'; base-uri 'none'",
        "script-src 'strict-dynamic' 'unsafe-inline' https:; object-src 'none'; base-uri 'none'"]) {
        assert.equal(byId(checks(web({ headers: [["content-security-policy", value]] })), "csp").status, "warning", value);
    }
});

test("strict-dynamic with a nonce passes despite its fallback sources; default-src 'none' covers object-src", () => {
    for (const value of ["script-src 'nonce-r4nd0m' 'strict-dynamic' 'unsafe-inline' https: http:; object-src 'none'; base-uri 'none'",
        "default-src 'none'; script-src 'self'; base-uri 'self'"]) {
        assert.equal(byId(checks(web({ headers: [["content-security-policy", value]] })), "csp").status, "pass", value);
    }
    assert.equal(byId(checks(web({ headers: [["content-security-policy", "script-src 'nonce-r4nd0m' 'strict-dynamic' 'unsafe-eval'; object-src 'none'; base-uri 'none'"]] })), "csp").status, "warning");
});

test("a redirect without a destination fails and leaves headers unreviewed", () => {
    const result = checks(web({ final_status: 302 }));
    assert.equal(byId(result, "https").status, "fail");
    assert.match(byId(result, "https").evidence, /without Location/);
    assert.equal(byId(result, "hsts").status, "unknown");
});

test("framing accepts wildcard subdomains and scheme-less hosts, never a bare scheme or wildcard", () => {
    for (const [value, expected] of [["frame-ancestors 'self' https://*.example.com", "pass"], ["frame-ancestors example.com:8443", "pass"],
        ["frame-ancestors https:", "warning"], ["frame-ancestors *", "warning"], ["frame-ancestors http://example.com", "warning"]]) {
        assert.equal(byId(checks(web({ headers: [["content-security-policy", value]] })), "framing").status, expected, value);
    }
});

test("CSP keywords and schemes are matched case-insensitively", () => {
    assert.equal(byId(checks(web({ headers: [["content-security-policy", "script-src 'self' HTTPS:; object-src 'none'; base-uri 'none'"]] })), "csp").status, "warning");
    assert.equal(byId(checks(web({ headers: [["content-security-policy", "default-src 'SELF'; object-src 'NONE'; base-uri 'Self'; frame-ancestors 'None'"]] })), "csp").status, "pass");
    assert.equal(byId(checks(web({ headers: [["content-security-policy", "frame-ancestors 'NONE'"]] })), "framing").status, "pass");
});

test("CSP framing overrides XFO and default-src does not substitute for frame-ancestors", () => {
    assert.equal(byId(checks(web({ headers: [["content-security-policy", "frame-ancestors *"], ["x-frame-options", "DENY"]] })), "framing").status, "warning");
    assert.equal(byId(checks(web({ headers: [["content-security-policy", "default-src 'none'"]] })), "framing").status, "warning");
    assert.equal(byId(checks(web({ headers: [["x-frame-options", "ALLOW-FROM https://example.com"]] })), "framing").status, "warning");
});

test("referrer fallbacks use the last recognized token", () => {
    for (const [value, expected] of [["unsafe-url, no-referrer", "pass"], ["no-referrer, unsafe-url", "warning"], ["no-referrer, unknown", "pass"]]) {
        assert.equal(byId(checks(web({ headers: [["referrer-policy", value]] })), "referrer").status, expected);
    }
});

test("error responses are identified and incomplete data is rejected", () => {
    assert.equal(byId(checks(web({ final_status: 403 })), "https").status, "warning");
    assert(validReport("ssl", cert));
    assert(validReport("headers", web()));
    assert(!validReport("ssl", { ...cert, not_before: "invalid" }));
    assert(!validReport("headers", { ok: true }));
    assert(!validReport("headers", web({ headers: [["name", null]] })));
    assert(!validReport("headers", web({ hops: [{ url: target.https, status: 200 }] })));
});
