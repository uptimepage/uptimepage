// Wall-clock time in an IANA zone ↔ instants, resolved the way the server's
// `local_to_utc` resolves them: the earlier of two on a fall-back hour, and a
// time the spring-forward gap skips moved on by whole hours until it exists.

export function knowsZone(tz) {
    try {
        new Intl.DateTimeFormat("en-US", { timeZone: tz });
        return true;
    } catch {
        return false;
    }
}

// One formatter per zone: building one is the costly part.
const formatters = new Map();
function formatter(tz) {
    let f = formatters.get(tz);
    if (!f) {
        f = new Intl.DateTimeFormat("en-US", {
            timeZone: tz, hourCycle: "h23",
            year: "numeric", month: "2-digit", day: "2-digit",
            hour: "2-digit", minute: "2-digit", second: "2-digit",
        });
        formatters.set(tz, f);
    }
    return f;
}

// Offset of `tz` from UTC at instant `ms`, in milliseconds.
function offsetAt(ms, tz) {
    const parts = formatter(tz).formatToParts(new Date(ms));
    const n = (type) => Number(parts.find((p) => p.type === type).value);
    const asUtc = Date.UTC(n("year"), n("month") - 1, n("day"), n("hour"), n("minute"), n("second"));
    return asUtc - Math.floor(ms / 1000) * 1000;
}

// The instant that wall-clock `y-m-d h:min` (month 0-based; out-of-range
// days roll over like Date.UTC) names in `tz`.
export function zonedInstant(tz, y, m, d, h = 0, min = 0) {
    const wall = Date.UTC(y, m, d, h, min);
    for (const step of [0, 1, 2, 3, 4]) {
        const hit = earliestAt(wall + step * 3600000, tz);
        if (hit !== null) return new Date(hit);
    }
    return new Date(wall - offsetAt(wall, tz));
}

// The earliest instant whose wall clock in `tz` reads `wall`, or null.
function earliestAt(wall, tz) {
    const hits = [offsetAt(wall - 86400000, tz), offsetAt(wall + 86400000, tz)]
        .map((off) => wall - off)
        .filter((ms) => ms + offsetAt(ms, tz) === wall)
        .sort((a, b) => a - b);
    return hits.length ? hits[0] : null;
}

// "YYYY-MM-DDTHH:MM", as a datetime-local input holds it, read in `tz`.
export function parseZoned(local, tz) {
    const m = /^(\d{4})-(\d{2})-(\d{2})T(\d{2}):(\d{2})$/.exec(local);
    return m ? zonedInstant(tz, +m[1], +m[2] - 1, +m[3], +m[4], +m[5]) : null;
}

// Today's year and 0-based month in `tz`.
export function zonedToday(tz) {
    const parts = new Intl.DateTimeFormat("en-US", { timeZone: tz, year: "numeric", month: "2-digit" })
        .formatToParts(new Date());
    const n = (type) => Number(parts.find((p) => p.type === type).value);
    return { year: n("year"), month: n("month") - 1 };
}
