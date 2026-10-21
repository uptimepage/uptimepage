import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import vm from "node:vm";

import { tagSource } from "../../assets/js/_source.js";

const source = readFileSync(new URL("../../assets/js/marketing/referral_source.js", import.meta.url), "utf8")
    .replace(/^import .*;\n/gm, "");

function send(url, referrer = "") {
    const window = {};
    vm.runInNewContext(source, { window, URL, tagSource, location: { origin: "https://uptimepage.test" } });
    return window.tagReferralSource("event", { url, referrer });
}

test("a bare ref name becomes its site as the referrer", () => {
    const payload = send("/pricing?ref=devto");
    assert.equal(payload.referrer, "https://dev.to/");
    assert.equal(payload.url, "/pricing?ref=devto&utm_source=devto");
});

test("a utm_source that is already a host is the referrer as written", () => {
    assert.equal(send("/about?utm_source=chatgpt.com").referrer, "https://chatgpt.com/");
});

test("an unknown name still names the referrer, lowercased", () => {
    assert.equal(send("/?ref=LastSpot").referrer, "https://lastspot/");
    assert.equal(send("/?ref=constructor").referrer, "https://constructor/");
});

test("a blank utm_source does not hide the ref", () => {
    assert.equal(send("/?ref=devto&utm_source=").referrer, "https://dev.to/");
});

test("a referrer the browser sent is kept", () => {
    assert.equal(send("/?ref=x", "https://t.co/abc").referrer, "https://t.co/abc");
    assert.equal(send("/docs?utm_source=devto", "/pricing").referrer, "/pricing");
});

test("an untagged or malformed source leaves the visit direct", () => {
    assert.equal(send("/pricing").referrer, "");
    assert.equal(send("/?utm_source=evil.com/path").referrer, "");
    assert.equal(send(`/?ref=${"a".repeat(40)}`).referrer, "");
});

test("a name that is not a host stays out of the referrer", () => {
    assert.equal(send("/?utm_source=595586999").referrer, "");
    assert.equal(send("/?ref=release-v1.2").referrer, "");
});

test("a tool's own input never reaches analytics", () => {
    assert.equal(send("/tools/dns?url=https://private.example&ref=devto").url, "/tools/dns?ref=devto&utm_source=devto");
});
