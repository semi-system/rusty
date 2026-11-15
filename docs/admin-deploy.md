# Deploying the admin web

The admin web is **one service, one image**: `docker build --target admin -f deploy/docker/Dockerfile .` makes `rusty-admin`, a static binary with the API and the front end embedded
(a distroless image, numeric non-root user, no shell, no volume but `/etc/rusty`). It holds no mail data; it needs the database and, to seal secrets, the **public** sealing key.

## 1. Configuration

Copy `admin.example.toml` to `/etc/rusty/admin.toml` and set at least `hostname`, `[admin] public_origin`, `[store]` and `[sealing]`. The service refuses any section that belongs to another
service. `[admin] public_ips`, `spf_include` and `via_relay` are what the DNS page needs (the same values as the outbound service's `[outbound]`).

Use the database role **`rusty_app`** (the one the other services use). The admin service never migrates the schema (`rustyctl db migrate` does).

## 2. The edge

Put HAProxy in front of it, on its own host name, with an **IP allow-list** (design: 2FA is not in the first version, the allow-list is the interim control):

```haproxy
frontend admin_https
    bind :443 ssl crt /run/rusty-certs/admin.example.org.pem alpn h2,http/1.1
    acl admin_host hdr(host) -i admin.example.org
    acl admin_net  src 198.51.100.0/24 203.0.113.7
    http-request deny if admin_host !admin_net
    use_backend be_admin if admin_host
backend be_admin
    server admin rusty-admin:8080 check
```

HAProxy terminates TLS (the private key stays in a file at the edge, never in the web). The service sends its own security headers (CSP, HSTS, nosniff, ...); the edge adds none.
`public_origin` must equal the origin the browser shows, or every state-changing request is refused with `403`.

## 3. The first administrator

On the first start **with no administrator**, the service prints one line on standard output: a 256-bit **setup token**. Read it with `docker logs`, open `https://admin.example.org/`, enter the token and the
address and password of the first platform administrator. The page disappears for good the moment an administrator exists; a new token is made at every start until then. If you lose access:
`rustyctl admin grant <address>` (an account with a password) makes any account an administrator.

## 4. Running it

```
docker run -d --name rusty-admin --read-only --cap-drop ALL --security-opt no-new-privileges:true \
  -v /etc/rusty:/etc/rusty:ro -v /run/secrets:/run/secrets:ro --network app-net rusty-admin:TAG
```

`scripts/check-admin-image.sh IMAGE` checks the properties above (non-root, no shell, static, front end embedded, fail closed without a configuration); CI runs it on every pull request that touches the image.

Not yet: a health check inside the image (the image has no shell and the binary has no `healthcheck` command; probe `GET /api/v1/session` from the edge, a `401` means it is up), a published admin image in the
release workflow, and Valkey-backed sessions (a restart signs everyone out).
