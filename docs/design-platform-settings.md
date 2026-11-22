# Platform settings

`GET`/`PUT /api/v1/settings` (platform administrators; a domain administrator gets `403`) read and change the platform-wide limits: `quota_bytes`, `max_message_bytes`, `messages_per_hour`. They are keys of the reserved plan row `__global__`, the last layer of the configuration an account resolves (account, plan, domain, global), so a change reaches the next delivery or sign-in without a restart: there is no cache to flush. A value left empty is removed and the server's own default applies. The page is "Platform settings" in the monitoring group.

Not here: the settings that are read once at start (listener addresses, TLS, database), which stay in the configuration files; and the protocol switches, which have their own page.
