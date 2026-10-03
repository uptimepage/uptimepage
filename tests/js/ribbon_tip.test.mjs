import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import vm from "node:vm";

// The label reads local calendar days, so pin a zone with a fall-back night.
process.env.TZ = "America/New_York";

const source = readFileSync(new URL("../../assets/js/ui/ribbon_tip.js", import.meta.url), "utf8");

function tipLabel() {
    const zone = { timeZone: process.env.TZ, hourCycle: "h23" };
    const time = new Intl.DateTimeFormat("en-US", { ...zone, hour: "2-digit", minute: "2-digit" });
    const dayTime = new Intl.DateTimeFormat("en-US", { ...zone, month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
    const window = { smLocalFmt: { time: d => time.format(d), dayTime: d => dayTime.format(d) } };
    const body = { appendChild() {}, addEventListener() {} };
    const document = { readyState: "complete", body, addEventListener() {} };
    vm.runInNewContext(source, { window, document, Date, isNaN });
    return window.smRibbonTipLabel;
}

const label = tipLabel();

function cell(from, to, withDate) {
    const attrs = { "data-tip-ts": from, "data-tip-to": to };
    return { getAttribute: key => attrs[key], closest: () => (withDate ? {} : null) };
}

test("a cell clamped to now just after it began shows one time", () => {
    assert.equal(label(cell("2026-10-03T16:00:00Z", "2026-10-03T16:00:30Z", true)), "Oct 3, 12:00");
    assert.equal(label(cell("2026-10-03T16:00:00Z", "2026-10-03T16:00:30Z", false)), "12:00");
});

test("a full cell shows its range", () => {
    assert.equal(label(cell("2026-10-03T16:00:00Z", "2026-10-03T16:30:00Z", false)), "12:00 – 12:30");
    assert.equal(label(cell("2026-10-03T16:00:00Z", "2026-10-03T17:00:00Z", true)), "Oct 3, 12:00 – 13:00");
});

test("a multi-day cell crossing local midnight dates its end", () => {
    assert.equal(label(cell("2026-10-04T01:00:00Z", "2026-10-04T07:00:00Z", true)), "Oct 3, 21:00 – Oct 4, 03:00");
});

test("the repeated hour on the fall-back night stays a range", () => {
    assert.equal(label(cell("2026-11-01T05:00:00Z", "2026-11-01T06:00:00Z", true)), "Nov 1, 01:00 – 01:00");
});
