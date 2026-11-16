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

## One place to scrape

Owner's choice (2026-11-16): the operator's Prometheus reads ONE endpoint, so that no service is missed and the scrape configuration does not grow with the services. The admin web is that endpoint (it already
sees the platform; no new container). `[[metrics.scrape]]` (admin web only; a service of another role refuses it) lists the services to read: `service` (a label value) and `address` (`host:port` of the
service's `[metrics]` listener). A name that resolves to many addresses (Docker's DNS name of a scaled service, `tasks.<service>` in a swarm) is many instances, up to 64 per target.

* A scrape of the admin web reads every instance in parallel (plain HTTP/1.0, two seconds and 1 MiB each) and answers with its own samples and theirs in one exposition: one HELP and TYPE per name, every
  sample with `instance` (the address read; `local` for the admin web's own) and `service`.
* `rusty_scrape_target_up{service,instance}` is 1 when the read worked and 0 when it did not (a bad status, a refusal, a timeout, an oversized or unreadable body, a name that does not resolve: the
  instance is then the configured address). A dead service shows as 0, never as nothing.
* A line that already names an `instance` is dropped; one that names a `service` (`rusty_build_info`, the admin web's `rusty_service_up{service}`) keeps its own.
* The services allow the admin web by its NAME (`[metrics] allowed_hosts = ["tasks.admin", "admin"]`: `tasks.admin` is the swarm form, `admin` the compose one). The names are resolved again every five seconds, a name that fails keeps its last answer, and until a name resolves nobody is let in; no address or subnet is fixed anywhere. The admin web's own endpoint allows the operator's networks (`allowed_peers`) or names (`allowed_hosts`, for a Prometheus that is a service itself). Prometheus scrapes it with `honor_labels: true`.
* If the admin web is down nothing is watched: `RustyCentralEndpointDown` (on `up{job="rusty"}`) is the alert for it, and the only one that needs an outside check.

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
DNS problems, a certificate close to expiry, the disk filling) and `deploy/monitoring/prometheus.yml` a scrape configuration that finds every replica through Docker's DNS. The stack does not run a Prometheus (owner's decision, 2026-11-16: the
endpoints are enough): `./setup.sh --metrics-allow CIDR` turns the listeners on for the networks named, and the operator's own Prometheus scrapes them (`docker.md`).

## Tests

The encoder (escaping, grouping, one HELP and TYPE per name, names and labels valid), the counters from service events, the server (only the allowed peer is answered, `404` elsewhere, a failing collector
does not break a scrape), the configuration section (shape, bounds, the role matrix), the admin collector against crafted heartbeats, and `promtool check rules` on the sample rules in CI.
