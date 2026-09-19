-- `name/version` from the MCP initialize handshake.
ALTER TABLE mcp_audit ADD COLUMN client TEXT;
