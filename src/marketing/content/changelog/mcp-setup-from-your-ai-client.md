+++
title = "MCP: set up a whole org from your AI client"
date = "2026-09-17"
summary = "Status page authoring, batch monitor create, authenticated checks and regions on create, plus one setup page per client."
+++

The MCP server now covers the setup path, not just reads. 16 read tools and 15 write tools, every write behind a confirmation in the client.

- `create_status_page`, `update_status_page`, `add_status_page_components` and `update_status_page_component`. Owner-gated, same bar as the REST API.
- `create_monitors` puts a batch behind one confirmation instead of one prompt per monitor.
- HTTP checks take request headers and a body. A credential is referenced as `Bearer {{ key }}` from your org variables, never pasted. A pasted literal is refused.
- Monitor create takes regions and binds notification channels, so a monitor made from a client pages someone from the first check.
- `get_org_usage` tells you which org the token is bound to. Scope and org refusals say how to rebind.

One setup page per client, each with the exact config for that client: [Claude](/mcp-server/claude), [Cursor](/mcp-server/cursor), [VS Code](/mcp-server/vscode), [Grok](/mcp-server/grok). Hub: [/mcp-server](/mcp-server).

Reference: [MCP server docs](/docs/mcp).
