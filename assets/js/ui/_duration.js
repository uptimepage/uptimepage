// The duration grammar the server's exact_duration renders: a whole number
// with an optional unit, `90s`, `15m`, `2h`, `30d`. A bare number is seconds
// and `""` is 0, so an empty optional field reads as off.
const UNITS = { s: 1, m: 60, h: 3600, d: 86400 };

export function parseDuration(raw) {
    const t = String(raw ?? "").trim().toLowerCase();
    if (t === "") return 0;
    const m = t.match(/^(\d+)\s*([smhd])?$/);
    if (!m) return null;
    return parseInt(m[1], 10) * (UNITS[m[2]] || 1);
}

// The API stores durations as a 32-bit count of seconds.
export const MAX_SECS = 2147483647;

// As parseDuration, for fields meant in minutes or hours: a bare number other
// than 0 is refused rather than read as seconds, and so is anything longer
// than the API's 32-bit second count.
export function parseSpan(raw) {
    const t = String(raw ?? "").trim();
    if (t === "" || (/^\d+$/.test(t) && Number(t) !== 0)) return null;
    const secs = parseDuration(t);
    return secs !== null && secs <= MAX_SECS ? secs : null;
}


// Mirrors exact_duration: the largest unit that divides, days from two up.
export function formatDuration(secs) {
    if (secs === 0) return "0s";
    if (secs >= 172800 && secs % 86400 === 0) return `${secs / 86400}d`;
    if (secs % 3600 === 0) return `${secs / 3600}h`;
    if (secs % 60 === 0) return `${secs / 60}m`;
    return `${secs}s`;
}
