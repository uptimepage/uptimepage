+++
title = "Markdown in incident and maintenance text"
date = "2026-10-30"
summary = "Incident updates, maintenance notes and postmortems take bold, italics, links, lists and code, on the page and in subscriber mail."
+++

**Format what you write.** Incident updates, maintenance descriptions and maintenance updates, and a postmortem's summary, root cause and impact now take Markdown: `**bold**`, `_italic_`, `[links](https://…)`, bullet and numbered lists, and `code`. It shows formatted on the status page, the incident page, the console, the RSS feed and in the emails subscribers get. The console forms say so under the text box.

**Plain text keeps working.** Every line break you type still shows as a break, so text written before reads as it did. HTML is not markup here: a tag shows as typed rather than disappearing. A heading reads as a plain paragraph, an image as its description, and a link needs a full web or mail address.

**Where it stays plain.** History rows, the dashboard banner and the inbox preview line show the words without the markup, and a history row opens to the formatted version. The REST API, the MCP server and subscriber webhooks return the text exactly as written.

Docs: [formatting](/docs/public-status#formatting).
