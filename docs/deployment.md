# Deployment

## Full stack (new install)

`deploy/` has a complete compose stack. See the [Quick start](../README.md#quick-start).

```
deploy/
  docker-compose.yaml   postgres, redis, inventree server + worker, inventory-assistant, searxng, caddy, (ollama)
  Caddyfile             TLS + routes: /static, /media, /capture/ (+ forward_auth), everything else -> InvenTree
  env.example           copy to .env
  searxng/settings.yml  enables the JSON API, disables the limiter (internal-only instance)
```

Everything persistent lives in `deploy/data/`: the Postgres data, InvenTree media and static files, the assistant's
spool (`data/assistant`) and Caddy's certificates. **Back this directory up.** It is your whole inventory.

The assistant container runs as uid `10001`. If you bind-mount an existing spool directory, run
`chown -R 10001 data/assistant` first.

## Adding to an existing InvenTree

1. Copy the `inventory-assistant` and `inventree-searxng` services into your compose file, and set
   `INVENTREE_URL` to an address the container can reach.
2. Add the `/capture/*` block from `deploy/Caddyfile` to your proxy. With nginx, use `auth_request` to InvenTree's
   `/auth/` endpoint for `/capture/api/` and proxy `/capture/` to `inventory-assistant:3000` with the prefix stripped.
3. Create an API token for a user who can add parts, stock and locations, and set `INVENTREE_TOKEN`.

The UI is built for `/capture/`. To serve it elsewhere, build with `--build-arg PUBLIC_URL=/your/path/`.

## TLS and phones

The camera input works over plain HTTP, but PWAs ("Add to Home Screen") and InvenTree's mobile app work best
with HTTPS. Options, from simplest:

- **Public hostname:** remove the `tls internal` line and Caddy gets a Let's Encrypt certificate.
- **LAN only (default):** `tls internal`. Install Caddy's root certificate
  (`data/caddy/caddy/pki/authorities/local/root.crt`) on each phone.
- **Your own CA:** put `fullchain.pem` and `privkey.pem` in `deploy/ssl/` and use
  `tls /certs/fullchain.pem /certs/privkey.pem`.

## Vision model

Any Ollama model with vision works. Set `OLLAMA_MODEL`. The default `qwen3.5:4b` takes about 10 s per photo on a
CPU and handles common modules well, but it is weaker on obscure boards and sometimes invents markings. Larger
models read markings noticeably better, and a GPU brings latency down to seconds. Because the queue decouples capture
from identification, a slow model never slows down capturing.

## Upgrading

- **InvenTree:** bump `INVENTREE_TAG`, then `docker compose pull && docker compose up -d`. Migrations run automatically
  (`INVENTREE_AUTO_UPDATE=True`), but static files do not: also run
  `docker compose exec inventree-server invoke static`, or the web UI shows *"INVE-E1 … issue with your update"*.
- **inventory-assistant:** bump `ASSISTANT_TAG` (or `latest`), then `docker compose pull inventory-assistant && docker compose up -d`.
