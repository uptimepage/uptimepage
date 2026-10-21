// Directory, launch-platform and AI-answer links arrive tagged (`?ref=` or
// `?utm_source=`) with the referrer stripped by rel="noreferrer", so Umami
// would file them under Direct. The tag becomes the referrer.
const SOURCE = /^[a-z0-9][a-z0-9._-]{0,31}$/i;
const SOURCE_HOSTS = new Map([
  ["ai-throne", "aithrone.lol"],
  ["bluesky", "bsky.app"],
  ["coderlegion", "coderlegion.com"],
  ["devto", "dev.to"],
  ["fazier", "fazier.com"],
  ["hashnode", "hashnode.com"],
  ["medium", "medium.com"],
  ["outbid", "outbid.lol"],
  ["perplexity", "perplexity.ai"],
  ["producthunt", "producthunt.com"],
  ["softrankings", "softrankings.com"],
  ["startupranking", "startupranking.com"],
  ["startupstash", "startupstash.com"],
  ["x", "x.com"],
]);

// A name URL rejects or rewrites (`2024` parses as an IPv4 address) is not a host.
const referrerOf = (source) => {
  const host = SOURCE_HOSTS.get(source) ?? source;
  try {
    const url = new URL(`https://${host}/`);
    return url.hostname === host ? url.href : "";
  } catch {
    return "";
  }
};

window.tagReferralSource = (_type, payload) => {
  const url = new URL(payload.url, location.origin);
  const ref = url.searchParams.get("ref");

  if (ref && SOURCE.test(ref) && !url.searchParams.get("utm_source")) {
    url.searchParams.set("utm_source", ref);
  }
  const source = url.searchParams.get("utm_source")?.toLowerCase();
  if (!payload.referrer && source && SOURCE.test(source)) {
    payload.referrer = referrerOf(source);
  }
  // Tool pages hand a visitor's own input along as `?url=`. It is theirs.
  url.searchParams.delete("url");
  payload.url = url.pathname + url.search + url.hash;

  return payload;
};
