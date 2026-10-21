import { tagSource } from "../_source.js";

// Auth URLs carry single-use credentials (magic-link and invitation tokens) and
// Umami stores the query string on every send, so only the source tag survives.
// A trailing dot is a valid FQDN and survives URL parsing, so it would slip the suffix match.
const bare = (host) => host.replace(/\.$/, "");
const site = bare(location.hostname).split(".").slice(-2).join(".");

const ours = (host) => {
    const h = bare(host);
    return h === site || h.endsWith(`.${site}`);
};

const parse = (value) => {
    try {
        return new URL(value, location.origin);
    } catch {
        return null;
    }
};

const path = (url) => (url.origin === location.origin ? url.pathname : url.origin + url.pathname);

window.smScrubAuthUrl = (_type, payload) => {
    const url = parse(payload.url);
    const source = url ? tagSource(url, payload) : "";
    payload.url = url ? path(url) + (source ? `?utm_source=${source}` : "") : "";

    // Umami only blanks a same-origin referrer, so the marketing host would
    // otherwise file its own click-throughs as an acquisition source.
    if (payload.referrer) {
        const ref = parse(payload.referrer);
        payload.referrer = !ref || ours(ref.hostname) ? "" : path(ref);
    }

    return payload;
};
