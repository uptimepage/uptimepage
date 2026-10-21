import { tagSource } from "../_source.js";

window.tagReferralSource = (_type, payload) => {
  const url = new URL(payload.url, location.origin);
  tagSource(url, payload);
  // Tool pages hand a visitor's own input along as `?url=`. It is theirs.
  url.searchParams.delete("url");
  payload.url = url.pathname + url.search + url.hash;

  return payload;
};
