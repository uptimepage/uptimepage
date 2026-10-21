import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import vm from "node:vm";

import { tagSource } from "../../assets/js/_source.js";

const source = readFileSync(new URL("../../assets/js/ui/auth_url_scrub.js", import.meta.url), "utf8")
    .replace(/^import .*;\n/gm, "");

function send(url, referrer = "") {
    const window = {};
    const location = { hostname: "app.uptimepage.test", origin: "https://app.uptimepage.test" };
    vm.runInNewContext(source, { window, URL, tagSource, location });
    return window.smScrubAuthUrl("event", { url, referrer });
}

test("a sign-in token never reaches analytics", () => {
    assert.equal(send("/auth/magic-link/verify?token=secret").url, "/auth/magic-link/verify");
});

test("only the source tag survives", () => {
    const payload = send("/login?utm_source=devto&redirect_after=/targets&utm_content=jane@example.com&token=t");
    assert.equal(payload.url, "/login?utm_source=devto");
    assert.equal(payload.referrer, "https://dev.to/");
    assert.equal(send("/login?utm_source=jane@example.com").url, "/login");
});

test("a ref link lands as its source", () => {
    const payload = send("/login?ref=producthunt");
    assert.equal(payload.url, "/login?utm_source=producthunt");
    assert.equal(payload.referrer, "https://producthunt.com/");
});

test("a click from our own site stays unattributed", () => {
    assert.equal(send("/login", "https://uptimepage.test/pricing").referrer, "");
    assert.equal(send("/login?utm_source=uptimepage.test", "https://uptimepage.test/pricing").referrer, "");
    assert.equal(send("/login?utm_source=uptimepage.test").referrer, "");
});

test("a referrer the browser sent beats the tag and loses its query", () => {
    const payload = send("/login?ref=devto", "https://accounts.google.com/o/oauth2?state=s");
    assert.equal(payload.referrer, "https://accounts.google.com/o/oauth2");
});
