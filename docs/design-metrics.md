# Design: metrics and alerts

Status: being built (owner's choice 2026-11-16: a Prometheus endpoint on every service plus sample alert rules; the admin web's Monitor page stays). This page says what is measured, how it is
exposed and how it is kept safe.

## Exposure

* Every service can serve `GET /metrics` in the Prometheus text format (0.0.4) on **a listener of its own**, never on a mail or admin port: `[metrics] listen = "0.0.0.0:9100"` and
  `allowed_peers = [...]`. The section is optional (absent: no listener). `allowed_peers` is required and cannot be empty or hold every address: the endpoint has no login, so only the scraper may connect
  (the same peer check as the admin web's, `PeerAllow`: any other peer is dropped before a byte is read).
* No new third-party dependency: the exposition format is written by the small `rusty-metrics` crate itself (counters and gauges, fixed names, labels from fixed sets, escaped values). Labels never hold
  an address, a user or a domain name: the cardinality is bounded by the code, not by the traffic.
* A scrape never blocks a service: the counters are atomics read at scrape time, and a collector that needs the database (below) has a two-second limit; if it fails the scrape still answers
  and `rusty_scrape_errors_total{collector}` counts it.

## What is measured

Every service: `rusty_build_info{service,version}`, `rusty_process_uptime_seconds`, `rusty_connections_accepted_total{listener}`, `rusty_connections_rejected_total{listener,reason}`,
`rusty_connections_closed_total{listener}`, `rusty_session_panics_total{listener}`, `rusty_auth_attempts_total{listener,result}`.

`rusty-smtp-in`: `rusty_filter_results_total{scanner,result}` (`clean`, `spam`, `virus`, `error`: a scanner that cannot be reached shows as `error`, so "clamd is down" is an alert on its rate).

The admin web, as the one place that sees the platform (it reads the heartbeats and the queue like the Monitor page does): `rusty_service_up{service}` (1 when at least one live instance beats),
`rusty_service_instances{service,state}` (`live`, `stale`), `rusty_outbound_queue_jobs{state}`, `rusty_outbound_queue_oldest_due_seconds`, `rusty_storage_stores{status}`,
`rusty_dns_problem_domains`.

What the platform's own metrics do not cover is left to the standard exporters the sample rules name: disk space (`node_exporter`), certificate expiry (`blackbox_exporter`'s `probe_ssl_earliest_cert_expiry`
against the public host names), the database server (`postgres_exporter`).

## Alerts

`deploy/monitoring/rusty.rules.yml` holds sample Prometheus rules (a service down, a replica lost, the queue growing or too old, dead jobs, a scanner failing, an authentication storm, storage checks failing,
DNS problems, a certificate close to expiry, the disk filling) and `deploy/monitoring/prometheus.yml` a scrape configuration that finds every replica through Docker's DNS. The standalone stack has an optional
`monitoring` profile that runs a Prometheus with them (no port published; the operator adds Alertmanager or a remote receiver).

## Tests

The encoder (escaping, grouping, one HELP and TYPE per name, names and labels valid), the counters from service events, the server (only the allowed peer is answered, `404` elsewhere, a failing collector
does not break a scrape), the configuration section (shape, bounds, the role matrix), the admin collector against crafted heartbeats, and `promtool check rules` on the sample rules in CI.
