// The hop list the header probe walked, drawn the same way on every page
// that shows one: code, URL, time, and the raw Location under it.
export function statusClass(code) {
    if (code >= 200 && code < 300) return "tool-hdr__code--ok";
    if (code >= 300 && code < 400) return "tool-hdr__code--hop";
    if (code >= 400 && code < 500) return "tool-hdr__code--warn";
    return "tool-hdr__code--down";
}

function node(tag, cls, text) {
    const n = document.createElement(tag);
    n.className = cls;
    n.textContent = text;
    return n;
}

export function chain(r) {
    const list = node("ol", "tool-hdr__chain mk-mono", "");
    for (const hop of r.hops) {
        const row = node("li", "tool-hdr__hop", "");
        row.append(node("span", `tool-hdr__code ${statusClass(hop.status)}`, String(hop.status)));
        row.append(node("span", "tool-hdr__url", hop.url));
        row.append(node("span", "tool-hdr__ms", Number.isFinite(hop.ms) ? `${hop.ms} ms` : ""));
        // The raw value, not the resolved one: a relative Location that a
        // browser joins differently is the bug people come here to find.
        if (hop.location) {
            row.append(node("span", "tool-hdr__loc", `→ ${hop.location}`));
        }
        list.append(row);
    }
    return list;
}
