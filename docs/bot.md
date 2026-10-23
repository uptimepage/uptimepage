# Probe identity

## If you found this link in your logs

A request to your site carried a `User-Agent` like this one:

```
Mozilla/5.0 (compatible; uptimepage/1.5.0; +https://github.com/uptimepage/uptimepage/blob/main/docs/bot.md)
```

uptimepage is open-source uptime monitoring. Anyone can run their own copy, and that is where this request came from: someone running uptimepage on their own servers asked it to check a URL on your site. A copy left on its default settings links this page, because its operator has not published one of their own.

The people who publish uptimepage do not run that copy and cannot stop it. To reach whoever does, look up the abuse contact for the IP address the request came from (`whois <address>` names the network that owns it).

What the probe does, whoever runs it:

- It requests the one URL it was given, on a schedule, and records whether it answered and how fast.
- It follows redirects, but not links. It does not crawl, scan ports or look for vulnerabilities.
- It does not read `robots.txt`, which is meant for crawlers.

To block it, match `uptimepage/` anywhere in the `User-Agent` and refuse the request at your firewall or web server.

If the link in the `User-Agent` points at `uptimepage.dev` instead, the request came from the hosted service, which publishes its own [bot page](https://uptimepage.dev/bot). A `User-Agent` is only a header, though, and any client can send one.

## If you run uptimepage

Your probes link this page until you give them one of your own. Publish a page that says who you are and how to reach you, then set `http_client.bot_url` to it on the control plane and on every agent:

```
UPTIMEPAGE_HTTP_CLIENT__BOT_URL=https://status.example.com/bot
```

With `deployment/docker-compose.agent.yml` it is `AGENT_BOT_URL` in the agent's `.env`; with the Helm charts, add the variable under `extraEnv`.

The `User-Agent` keeps its `Mozilla/5.0 (compatible; uptimepage/<version>; …)` shape, so site owners who allowlist `uptimepage/` keep letting your checks through. To replace the whole header instead, set `http_client.user_agent`; see [Configuration](configuration.md).
