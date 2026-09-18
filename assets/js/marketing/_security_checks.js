// Pure interpretation of the existing probe reports. Unknown or incomplete
// evidence must never become a pass, nor a claim that a header is missing.
export function targetURLs(raw) {
    const value = raw.trim();
    if (!value || value.length > 2048) throw new Error("Enter a public website, such as example.com.");
    let url;
    try { url = new URL(value.includes("://") ? value : `https://${value}`); }
    catch { throw new Error("Enter a valid website URL, such as https://example.com."); }
    if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.port) {
        throw new Error("Use an HTTP or HTTPS URL on its default port, without a username or password.");
    }
    const host = url.hostname.replace(/\.$/, "");
    if (!host.includes(".") || /^[\d.]+$/.test(host) || host.includes(":") ||
        !host.split(".").every(label => /^[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?$/i.test(label))) {
        throw new Error("Enter a public domain name. IP addresses and local hostnames cannot be checked.");
    }
    url.hostname = host;
    url.hash = "";
    url.protocol = "https:";
    const https = url.href;
    url.protocol = "http:";
    return { host, https, http: url.href };
}

export function validReport(kind, r) {
    if (!r || r.ok !== true) return false;
    if (kind === "ssl") return typeof r.host === "string" && Number.isFinite(r.days_remaining) &&
        typeof r.expired === "boolean" && typeof r.name_matches === "boolean" &&
        Number.isFinite(Date.parse(r.not_before)) && Number.isFinite(Date.parse(r.not_after)) &&
        Number.isInteger(r.chain_len) && r.chain_len > 0 && typeof r.self_signed === "boolean";
    const webURL = value => {
        try { return ["http:", "https:"].includes(new URL(value).protocol); } catch { return false; }
    };
    return webURL(r.final_url) && Number.isInteger(r.final_status) &&
        typeof r.redirect_loop === "boolean" && typeof r.hop_limit_hit === "boolean" &&
        typeof r.headers_truncated === "boolean" && Array.isArray(r.headers) &&
        r.headers.every(pair => Array.isArray(pair) && pair.length === 2 && pair.every(v => typeof v === "string")) &&
        Array.isArray(r.hops) && r.hops.length > 0 &&
        r.hops.every(h => webURL(h.url) && Number.isInteger(h.status));
}

const finding = (id, status, title, evidence, advice) => ({ id, status, title, evidence, advice });
const unknown = (id, title, evidence) => finding(id, "unknown", title, evidence,
    "Retry when the site and checker can respond. An incomplete check does not establish a security problem.");
const complete = r => r && !r.redirect_loop && !r.hop_limit_hit;
// The probe ends a walk on a 3xx that carries no Location: a browser shows nothing there.
const dangling = r => r.final_status >= 300 && r.final_status < 400;
const secure = url => new URL(url).protocol === "https:";
const downgraded = r => r.hops.some((h, i) => i > 0 && secure(r.hops[i - 1].url) && !secure(h.url));

// Values cut by the header probe end in an ellipsis. Multiple policies and
// duplicate values need browser-specific interpretation, so defer them.
function header(r, name) {
    if (!complete(r)) return { unknown: "The HTTPS redirect chain did not finish." };
    const values = r.headers.filter(([key]) => key.toLowerCase() === name).map(([, value]) => value);
    if (r.headers_truncated || values.some(v => v.endsWith("…") || v === "<not text>")) {
        return { unknown: "The probe could not return the complete header evidence. Review the full response manually." };
    }
    if (values.length > 1) return { unknown: "Multiple header values were returned. Review their combined behavior manually." };
    return { value: values[0] ?? null };
}

function directives(value) {
    const map = new Map();
    for (const piece of value.split(";")) {
        const [name, ...tokens] = piece.trim().split(/\s+/);
        const key = name.toLowerCase();
        // CSP uses the first occurrence; keywords and schemes match case-insensitively.
        if (key && !map.has(key)) map.set(key, tokens.map(t => t.toLowerCase()));
    }
    return map;
}

function headerFindings(r, reason) {
    const specs = [
        ["hsts", "HTTPS memory (HSTS)", "strict-transport-security"],
        ["csp", "Content Security Policy", "content-security-policy"],
        ["framing", "Framing protection", "x-frame-options"],
        ["nosniff", "Content type protection", "x-content-type-options"],
        ["referrer", "Referrer policy", "referrer-policy"],
    ];
    if (!complete(r) || !secure(r.final_url) || dangling(r)) return specs.map(([id, title]) => unknown(id, title,
        reason || (r && dangling(r) ? "The chain ended on a redirect without a destination, so there is no page response to review." :
            "No complete final HTTPS response is available for header review.")));
    const csp = header(r, "content-security-policy");
    return specs.map(([id, title, name]) => {
        const h = header(r, name);
        if (h.unknown) return unknown(id, title, h.unknown);
        const raw = h.value;
        const evidence = raw === null ? `${name}: not present on this response` : `${name}: ${raw}`;
        const result = (status, advice, detail = evidence) => finding(id, status, title, detail, advice);
        if (id === "hsts") {
            const parts = raw?.split(";").map(s => s.trim()) ?? [];
            const ages = parts.filter(s => /^max-age\s*=/i.test(s));
            const age = ages.length === 1 ? /^max-age\s*=\s*(?:"(\d+)"|(\d+))$/i.exec(ages[0]) : null;
            const seconds = age ? Number(age[1] ?? age[2]) : 0;
            if (!age || !Number.isFinite(seconds) || seconds === 0 || raw.includes(",")) return result("warning",
                "Serve one Strict-Transport-Security header over HTTPS with a positive max-age. Start with a short rollout; add includeSubDomains only when every affected subdomain supports HTTPS.");
            if (seconds < 15552000) return result("warning",
                "HSTS is enabled with a short lifetime. After testing HTTPS across your deployment, consider a max-age of at least 15552000 seconds (180 days). This is a hardening recommendation, not a browser validity requirement.");
            return result("pass", "A positive HSTS lifetime of at least 180 days was observed. Preload status and parent-domain policies were not checked.");
        }
        if (id === "csp") {
            if (!raw) return result("warning", "Define a Content-Security-Policy for your actual scripts and resources. Test changes using Content-Security-Policy-Report-Only before enforcing them; report-only policies do not block loads.");
            if (raw.includes(",")) return unknown(id, title, "A combined CSP policy was returned. Review the policies together manually.");
            const policy = directives(raw);
            const scripts = policy.get("script-src-elem") ?? policy.get("script-src") ?? policy.get("default-src");
            const general = policy.get("script-src") ?? policy.get("default-src");
            // Under 'strict-dynamic' with a nonce or hash, browsers ignore host,
            // scheme and 'unsafe-inline' sources; they are the compatibility fallback.
            const strict = list => list.includes("'strict-dynamic'") && list.some(s => /^'(nonce|sha(256|384|512))-/.test(s));
            const effective = list => strict(list) ? list.filter(s => s.startsWith("'") && s !== "'unsafe-inline'") : list;
            const risky = ["*", "http:", "https:", "data:", "'unsafe-inline'", "'unsafe-eval'"];
            if (!scripts || !general || [...effective(scripts), ...effective(general)].some(s => risky.includes(s) || s.startsWith("http://") || s.includes("*")) ||
                policy.get("script-src-attr")?.includes("'unsafe-inline'") ||
                (policy.get("object-src") ?? policy.get("default-src"))?.join(" ") !== "'none'" ||
                !["'none'", "'self'"].includes(policy.get("base-uri")?.join(" "))) {
                return result("warning", "Review script sources, inline/eval allowances, object-src and base-uri. Prefer narrowly scoped sources or nonces/hashes, object-src 'none', and a restricted base-uri. Nonces and strict-dynamic can change how allowances behave; this basic check does not fully evaluate CSP.");
            }
            return result("pass", "An enforced CSP with basic script, object and base restrictions was observed. Source trust, policy syntax and application behavior still need a full CSP review.");
        }
        if (id === "framing") {
            if (csp.unknown || csp.value?.includes(",")) return unknown(id, title, csp.unknown || "Combined CSP policies need manual framing review.");
            const ancestors = csp.value ? directives(csp.value).get("frame-ancestors") : undefined;
            if (ancestors) {
                // A host source may omit the scheme and lead with a wildcard label; a bare
                // scheme or a lone wildcard is not a restriction.
                const origin = /^(?:https:\/\/)?(?:\*\.)?[a-z0-9-]+(?:\.[a-z0-9-]+)*(?::\d+)?$/;
                const restricted = ancestors.length > 0 && (ancestors.join(" ") === "'none'" ||
                    ancestors.every(s => s === "'self'" || origin.test(s)));
                return result(restricted ? "pass" : "warning",
                    restricted ? "An explicit framing restriction was observed. Confirm that permitted embedding origins match your application." :
                        "Review frame-ancestors. Use 'none' to disallow embedding, 'self' for same-origin embedding, or specific trusted origins. Enforced frame-ancestors takes precedence over X-Frame-Options.",
                    `content-security-policy frame-ancestors: ${ancestors.join(" ")}`);
            }
            return result(raw && /^(DENY|SAMEORIGIN)$/i.test(raw.trim()) ? "pass" : "warning",
                raw && /^(DENY|SAMEORIGIN)$/i.test(raw.trim()) ? "X-Frame-Options restricts framing. CSP frame-ancestors offers more flexible control." :
                    "Set CSP frame-ancestors to match your embedding needs, or X-Frame-Options: DENY / SAMEORIGIN. ALLOW-FROM is obsolete.");
        }
        if (id === "nosniff") return result(raw?.trim().toLowerCase() === "nosniff" ? "pass" : "warning",
            raw?.trim().toLowerCase() === "nosniff" ? "The response asks browsers to respect declared content types. Verify that your server also sends the correct Content-Type." :
                "Set X-Content-Type-Options: nosniff and serve resources with their correct Content-Type.");
        const recognized = ["no-referrer", "no-referrer-when-downgrade", "origin", "origin-when-cross-origin", "same-origin", "strict-origin", "strict-origin-when-cross-origin", "unsafe-url"];
        const tokens = (raw ?? "").split(",").map(s => s.trim());
        const effective = tokens.filter(s => recognized.includes(s)).at(-1);
        const preferred = ["no-referrer", "same-origin", "strict-origin", "strict-origin-when-cross-origin"].includes(effective);
        return result(preferred ? "pass" : "warning", preferred ?
            `The last recognized policy is ${effective}; it limits referrer details across origins or on downgrades.` :
            "Consider Referrer-Policy: strict-origin-when-cross-origin, or a stricter policy if appropriate. Modern browsers already have a restrictive default; a missing header alone does not prove a leak.");
    });
}

export function assess(target, ssl, https, http, now = Date.now()) {
    const checks = [];
    const cert = ssl.report;
    if (cert) {
        const notYet = Date.parse(cert.not_before) > now;
        checks.push(finding("certificate", cert.expired || notYet ? "fail" : cert.days_remaining <= 30 ? "warning" : "pass",
            "Certificate dates", `${target.host}: ${cert.not_before} → ${cert.not_after}; ${cert.days_remaining} days remaining`,
            cert.expired ? "Renew and deploy the certificate on this hostname, then check again." : notYet ?
                "Deploy a certificate that is valid now and verify your issuance and server clocks." : cert.days_remaining <= 30 ?
                    "Check that automated renewal is working and set up an expiry alert." : "The observed certificate is within its validity period. Keep automated renewal and expiry monitoring enabled."));
        checks.push(finding("hostname", cert.name_matches ? "pass" : "fail", "Certificate hostname",
            `${target.host}: ${cert.name_matches ? "covered by" : "does not match"} the presented certificate`,
            cert.name_matches ? "The certificate covers the requested hostname." : "Issue and deploy a certificate that includes this hostname in its Subject Alternative Names."));
        if (cert.self_signed) checks.push(finding("self-issued", "warning", "Self-issued certificate",
            "The certificate subject and issuer names match.", "This can indicate a self-signed certificate; matching names alone do not verify its signature. Check the validated HTTPS result and deploy a publicly trusted chain for a public website."));
    } else {
        checks.push(unknown("certificate", "Certificate dates and hostname", ssl.error));
    }
    const web = https.report;
    if (web) {
        checks.push(finding("trust", "pass", "HTTPS certificate validation",
            `The HTTPS request to ${target.https} completed a validated TLS connection.`,
            `Our HTTP client's trust store accepted the connection. ${cert ? `The separate certificate read observed ${cert.chain_len} certificate(s); chain length alone does not prove completeness.` : "Certificate details could not be read separately."} Revocation and all client trust stores were not tested.`));
        const bad = downgraded(web) || !secure(web.final_url);
        checks.push(finding("https", bad || !complete(web) || dangling(web) ? "fail" : web.final_status >= 400 ? "warning" : "pass",
            "HTTPS response and redirects", `${web.final_status} at ${web.final_url}${web.redirect_loop ? " · redirect loop" : ""}${web.hop_limit_hit ? " · redirect limit reached" : ""}${dangling(web) ? " · redirect without Location" : ""}`,
            bad ? "Remove redirects from HTTPS to HTTP. Keep every hop after the initial secure connection on HTTPS." : !complete(web) ?
                "Fix the redirect loop or shorten the chain so the request can reach a final response." : dangling(web) ?
                    "The chain ended on a redirect with no Location header, which browsers show as an empty page. Send a destination or return the page directly." : web.final_status >= 400 ?
                        "The server returned an error or challenge. Header findings below describe that response, which may differ from your normal application page." : "The observed chain stayed on HTTPS. Header checks below describe its final response."));
    } else {
        // The header probe validates certificates; its TLS sentence is a verdict, not a gap.
        // It does not say which hop failed, so the advice covers the whole chain.
        checks.push(/TLS handshake/.test(https.error ?? "") ?
            finding("trust", "fail", "HTTPS certificate validation", `${target.https}: ${https.error}`,
                "A validating client could not complete TLS on the HTTPS chain from this URL. Check the certificate chain, dates and hostname on every host the chain visits. A protocol or cipher mismatch, or a server that drops non-browser clients, reads the same way.") :
            unknown("trust", "HTTPS certificate validation", https.error));
        checks.push(unknown("https", "HTTPS response and redirects", https.error));
    }
    const plain = http.report;
    if (plain) {
        const upgrade = complete(plain) && secure(plain.final_url) && !downgraded(plain);
        checks.push(finding("http", upgrade ? "pass" : "fail", "HTTP to HTTPS redirect",
            `${target.http} → ${plain.final_url}${plain.redirect_loop ? " · redirect loop" : ""}${plain.hop_limit_hit ? " · redirect limit reached" : ""}`,
            upgrade ? "The HTTP URL reached HTTPS without an observed downgrade." : "Configure the HTTP URL to redirect to HTTPS. Remove loops and any later downgrade to HTTP."));
    } else checks.push(unknown("http", "HTTP to HTTPS redirect", http.error));
    checks.push(...headerFindings(web, https.error));
    const order = { fail: 0, warning: 1, unknown: 2, pass: 3 };
    return checks.sort((a, b) => order[a.status] - order[b.status]);
}
