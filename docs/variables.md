# Variables and secrets

A variable is a reusable named value scoped to your org. A monitor references it with a `{{key}}` placeholder in its HTTP request fields, and the value is substituted in before the check runs. Define a value once and every monitor that references it stays in sync: edit the variable and the change reaches all of them on their next check, with no need to open each monitor.

A **secret** variable is the same idea for credentials. Its value is write-only: it is sealed at rest, never returned by any read API, never rendered back in the UI, and only allowed in fields where it cannot leak. Use it for API keys, bearer tokens, and other request secrets so the literal value lives in one place instead of being copied into every monitor.

Manage variables under **Settings → Variables**, or through the [REST API](api.md#operator-endpoints-variables). An [MCP](mcp.md) client can read the key list to build a check against one, but never creates, edits or reads a variable — see [From an MCP client](#from-an-mcp-client).

## References

A reference is the variable key wrapped in double braces, for example `{{api_key}}`. A key's first character must be a lowercase letter, and the rest may be lowercase letters, digits, and underscores (`api_key`, `base_url`, `region2`). Inner spaces are trimmed, so `{{ api_key }}` and `{{api_key}}` mean the same thing.

References resolve in these HTTP request fields:

| Field | Plain variable | Secret variable |
|---|---|---|
| URL | yes | no |
| Header value | yes | yes |
| Request body | yes | yes |
| Body assertion (expected body contains) | yes | no |
| Flow step `fill` value | yes | yes |

A secret is confined to header values, the request body, and the value a flow step types into a form field. It is rejected in the URL and in the body assertion, because those surface through logs, redirects, or the captured response and would leak the value. A plain variable is allowed anywhere on the request side.

A flow monitor's `fill` value is the one place a credential would otherwise be stored literally, since a login script needs the password. Reference a secret there and the stored config keeps only the `{{key}}`.

Substitution is single pass: a value that itself contains `{{x}}` is inserted as plain text and is never scanned for further references, so one variable can never expand into another.

## Defining a variable

Each variable has a key, a value, and a secret flag set at creation. To change whether a value is secret, delete it and recreate it.

A plain variable shows its value in the list. A secret shows `••••`; once saved, its value is never displayed again. Editing a secret means rotating it: you type a new value, which replaces the old one. Rotating reaches monitors on their next check, the same as editing a credential.

The list shows a **used by** count: how many monitors reference each variable. A variable that is still referenced cannot be deleted; remove the references first. This makes the blast radius of a change visible before you make it.

## The authentication picker

On the monitor form, under **headers**, the **use a secret variable** control is a shortcut for the common case of sending a credential as an auth header. Pick a secret variable and a scheme, and it adds the matching header for you:

- `Authorization: Bearer {{key}}`
- `x-api-key: {{key}}`

The monitor stores only the `{{key}}` reference, not the value. The picker appears once you have at least one secret variable. You can also type a reference into any header by hand; the picker is a convenience, not the only path.

The interpolable fields (URL, header value, body, assertion) also offer an inline helper: type `{{` and a list of your variable keys appears. In the URL and the body assertion, secret keys are shown greyed out, since a secret is not allowed there.

## How resolution works

References resolve on the control plane, in the same step that decrypts a monitor's stored credentials. A region's probe agent receives a request that is already complete, so the agent needs no database access and never sees a variable key, only the finished request. A secret variable becomes just another decrypted credential the agent holds for the duration of the check.

Propagation is by the next configuration pull. Editing a variable changes the config fingerprint for every org that references it, so agents re-pull and start probing with the new value on their next cycle, exactly as they do when you change a stored credential.

If a referenced variable cannot resolve (the key does not exist, a secret cannot be opened, or the result is not a valid URL), that one monitor is dropped from the served set with a warning, the same rule a monitor with an unreadable credential follows. One broken monitor never affects the others in the region. A save-time check rejects a monitor whose references do not all resolve, so this is rare in practice.

## Security

Where secrets live: a secret value is sealed with the application key (KEK) before it is written, using the same cipher that protects stored `basic_auth` and `bearer_token` credentials. A database or backup dump without the key yields nothing usable.

No-key fallback: if no KEK is configured (a self-hosted instance that has not set one), a secret is stored in plaintext, the same fallback that target credentials use. The value is still write-only in the API and the UI, but it is not encrypted at rest. Configure a KEK if you need encryption at rest.

What is redacted: a secret value is never serialized by any read API and never rendered in the UI after it is saved. The public status page never resolves variables, so nothing reaches a shared surface. An interactive **test** or **check now** runs against the real resolved request, and the captured response is scrubbed of any secret value it echoed back, so the test surface cannot reveal a secret either. Values shorter than four characters are left alone: replacing a one or two character string would mangle unrelated response text and protects nothing worth protecting.

Repointing a URL variable: if a monitor builds its URL from a plain variable and also sends a secret in a header, editing that URL variable sends the credential to whatever host it now names. Nothing blocks this, because it is sometimes what you intend, but the save is logged with the keys involved. Treat a URL variable on a monitor that carries a secret as a credential-scoped setting.

Redirects: when a monitor follows a redirect to a different origin, the probe strips the standard sensitive headers (`Authorization`, your configured cookies, `x-api-key`, `api-key`, and similar) so a credential cannot follow a hostile `Location`. Cookies a redirect sets are sent on the later hops of the same run wherever their domain and path allow, and a configured cookie wins over one with the same name. The authentication picker writes its credential into `Authorization` or `x-api-key`, both of which are covered. If you place a secret in a non-standard header name of your own and your monitor follows redirects to hosts you do not control, that header is not on the strip list and could follow the redirect; prefer the picker's headers for credentials.

Agent hosts hold credentials: because resolution happens before the request reaches the agent, a probe agent holds decrypted secret values in memory for the duration of each check. Regional agents run on separate hosts from the control plane; treat those hosts as credential-bearing.

## From an MCP client

A connected LLM client can list the org's variable keys with `list_variables` (`variables:read`), which returns the key and whether it is a secret — never a value, and a secret's value is never even read. That is enough to build an authenticated check: write the reference into a request header or the body, as `Bearer {{ api_key }}`.

`variables:write` is not grantable to a connector, so a variable is always created and rotated in the app or over the REST API. Nor will the MCP surface accept a pasted credential: a header named `authorization`, `proxy-authorization`, `x-api-key`, `api-key` or `cookie` has to hold a reference, since a literal would be echoed into the chat transcript and stored in the monitor's spec as plaintext rather than sealed. See [How creation is guarded](mcp.md#how-creation-is-guarded).

## Managing variables over the API

The endpoints are gated on the `variables:read` and `variables:write` scopes and are scoped to the caller's active org.

```bash
# Create a secret
curl -X POST https://app.uptimepage.dev/api/v1/variables \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
  -H 'Content-Type: application/json' \
  -d '{"key":"api_key","is_secret":true,"value":"sk-live-..."}'
# → { "id": "...", "key": "api_key", "is_secret": true, "value": null, "used_by": 0 }

# List (secret values come back null)
curl https://app.uptimepage.dev/api/v1/variables \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN"

# Rotate a value
curl -X PATCH https://app.uptimepage.dev/api/v1/variables/$ID \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN" \
  -H 'Content-Type: application/json' -d '{"value":"sk-live-new"}'

# Delete (409 while still referenced by a monitor)
curl -X DELETE https://app.uptimepage.dev/api/v1/variables/$ID \
  -H "Authorization: Bearer $UPTIMEPAGE_TOKEN"
```

See the [REST API](api.md#operator-endpoints-variables) for the full endpoint contract.
