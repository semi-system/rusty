use strict; use warnings; local $/;
sub slurp { open(my $h,"<",$_[0]) or die; my $s=<$h>; close $h; $s } sub spit { open(my $h,">",$_[0]) or die; print $h $_[1]; close $h }
my $f="deploy/swarm/profiles.sh"; my $s=slurp($f);
# split the resolver from the scanners
$s =~ s/gen_filter_services\(\) \{\n  local f\n  # The resolver: validating, DNSSEC, answers the private ranges only\. Port 53 needs the capability to bind below 1024\.\n/gen_resolver_service() {\n  # The resolver: validating, DNSSEC, answers the private ranges only. Port 53 needs the capability to bind below 1024. It is started first: its address goes into the scanners' configuration.\n/ or die 1;
$s =~ s/(  printf '    networks:\n      - filter\n      - backend\n'\n)  # The spam scorer\.\n/$1}\n\ngen_scanner_services() {\n  local f\n  # The spam scorer.\n/ or die 2;
# valkey: the extra volume before user\/command
$s =~ s/  y_service_common valkey "\$\(lock_ref 'valkey\/valkey:9-alpine'\)"\n  printf '    user: "65532:65532"\n    command: \[(.*?)\]\n'\n  # the data directory is memory only; nothing in the store is worth keeping\n  printf '      - type: tmpfs\n        target: \/data\n        tmpfs: \{ mode: 1777 \}\n'\n/  y_service_common valkey "\$(lock_ref 'valkey\/valkey:9-alpine')"\n  # the data directory is memory only; nothing in the store is worth keeping\n  printf '      - type: tmpfs\n        target: \/data\n        tmpfs: { mode: 1777 }\n'\n  printf '    user: "65532:65532"\n    command: [$1]\n'\n/s or die 3;
spit($f,$s);
