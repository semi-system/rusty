use strict; use warnings; local $/;
sub rep { my ($s,$old,$new,$w)=@_; my $i=index($$s,$old); die "nf: $w" if $i<0; substr($$s,$i,length($old),$new); }
sub edit { my($f,$c)=@_; open(my $h,"<",$f) or die $f; my $s=<$h>; close $h; $c->(\$s); open($h,">",$f) or die; print $h $s; close $h; }
my $PROM = "prom/prometheus\@sha256:1f0f50f06acaceb0f5670d2c8a658a599affe7b0d8e78b898c1035653849a702";

edit("scripts/ci-gates.sh", sub { my $s = shift;
  rep($s, "    bash scripts/check-docker-pins.sh\n", "    bash scripts/check-docker-pins.sh\n    bash scripts/check-prom-rules.sh\n", "gate");
});

edit("deploy/standalone/compose.yaml", sub { my $s = shift;
  # every service with a metrics endpoint joins the metrics network (dynamic addresses: they scale)
  for my $svc (qw(smtp-in submission imap outbound gc admin)) {
    my $at = index($$s, "\n  $svc:\n"); die "svc $svc" if $at < 0;
    my $end = index($$s, "\n\n", $at + 1); die "end of $svc" if $end < 0;
    my $blk = substr($$s, $at, $end - $at);
    if ($blk =~ /\A(.*?\n    networks:\n)(      db: \{\}\n)/s) {
      my $pre = $1; my $db = $2;
      substr($blk, length($pre), length($db), $db . "      metrics: {}\n");
    } elsif ($blk =~ s/\n    networks: \[db\]\n/\n    networks:\n      db: {}\n      metrics: {}\n/) {
    } else { die "networks of $svc"; }
    substr($$s, $at, $end - $at) = $blk;
  }
  my $svc = <<'YAML';
  # ---- the `monitoring` profile (docs/design-metrics.md): a Prometheus that scrapes every replica of every service and evaluates the sample alert rules of deploy/monitoring. Start with `./setup.sh --monitoring ...`.
  # No port is published: reach it with `docker compose exec prometheus ...` or add a port in an override file; send the alerts to your own Alertmanager or remote receiver.
  prometheus:
    profiles: [monitoring]
    image: PROMIMAGE
    <<: *hardened
    user: "65534:65534"
    command:
      - --config.file=/etc/prometheus/prometheus.yml
      - --storage.tsdb.path=/prometheus
      - --storage.tsdb.retention.time=15d
      - --web.listen-address=0.0.0.0:9090
    tmpfs: [/tmp]
    volumes:
      - ../monitoring/prometheus.yml:/etc/prometheus/prometheus.yml:ro
      - ../monitoring/rusty.rules.yml:/etc/prometheus/rusty.rules.yml:ro
      - promdata:/prometheus
    networks:
      # The one scraper the services allow (`[metrics] allowed_peers`): it keeps an address.
      metrics: { ipv4_address: 10.79.0.2 }

YAML
  $svc =~ s/PROMIMAGE/$PROM/;
  rep($s, "  # rustyctl for the operator:", $svc . "  # rustyctl for the operator:", "svc");
  rep($s, "  mail-data:\n", "  mail-data:\n  promdata:\n", "vol");
  rep($s, "  # Out to the internet: the edge", "  # The metrics endpoints and the Prometheus that scrapes them, nothing else.\n  metrics:\n    internal: true\n    ipam: { config: [{ subnet: 10.79.0.0/24 }] }\n  # Out to the internet: the edge", "net");
});

for my $svc (qw(smtp-in submission imap outbound gc admin)) {
  edit("deploy/standalone/templates/$svc.toml.in", sub { my $s = shift;
    $$s .= "\n# The Prometheus endpoint (./setup.sh --monitoring): only the Prometheus of the monitoring profile may scrape it.\n#MONITORING [metrics]\n#MONITORING listen = \"0.0.0.0:9100\"\n#MONITORING allowed_peers = [\"10.79.0.2/32\"]\n";
  });
}

edit("deploy/standalone/setup.sh", sub { my $s = shift;
  rep($s, "BUILD=0; VALKEY=0;", "BUILD=0; VALKEY=0; MONITORING=0;", "vars");
  rep($s, "    --valkey) VALKEY=1; shift ;;\n", "    --valkey) VALKEY=1; shift ;;\n    --monitoring) MONITORING=1; shift ;;\n", "arg");
  rep($s, "if [ \"\$VALKEY\" = 1 ]; then profiles=\"\${profiles:+\$profiles,}valkey\"; fi;", "if [ \"\$VALKEY\" = 1 ]; then profiles=\"\${profiles:+\$profiles,}valkey\"; fi; if [ \"\$MONITORING\" = 1 ]; then profiles=\"\${profiles:+\$profiles,}monitoring\"; fi;", "profile");
  rep($s, "vkeep=\"#NOVALKEY_\"; [ \"\$FILTER\" = 1 ] && fkeep=\"#FILTER \"; [ \"\$VALKEY\" = 1 ] && vkeep=\"#VALKEY \";", "vkeep=\"#NOVALKEY_\" mkeep=\"#NOMONITORING_\"; [ \"\$FILTER\" = 1 ] && fkeep=\"#FILTER \"; [ \"\$VALKEY\" = 1 ] && vkeep=\"#VALKEY \"; [ \"\$MONITORING\" = 1 ] && mkeep=\"#MONITORING \";", "render1");
  rep($s, "-e \"s|^\${vkeep}||\"", "-e \"s|^\${vkeep}||\" -e \"s|^\${mkeep}||\"", "render2");
});
