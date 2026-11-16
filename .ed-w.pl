use strict; use warnings; local $/;
sub slurp { open(my $h,"<",$_[0]) or die; my $s=<$h>; close $h; $s } sub spit { open(my $h,">",$_[0]) or die; print $h $_[1]; close $h }
my $f="deploy/swarm/wizard.sh"; my $s=slurp($f);
$s =~ s/(  case " \$allow " in \*" 0.0.0.0\/0 "\*\)[^\n]*\n)/$1  tlsmode="\$(ui_menu "Certificates for the mail and admin host names" acme "Let's Encrypt (the two names must point at this server and port 80 be open)" self "Self-signed (a test installation: clients will warn)")"\n/ or die 1;
$s =~ s/local out="\$1" where host pubip mailhost adminhost contact allow dbmode s3mode/local out="\$1" where host pubip mailhost adminhost contact allow dbmode s3mode tlsmode/ or die 2;
$s =~ s/CONTACT=\$contact\n/CONTACT=\$contact\nTLS_MODE=\$tlsmode\n/ or die 3;
spit($f,$s);
$f="scripts/tests/swarm-wizard.sh"; $s=slurp($f);
$s =~ s/"203.0.113.5\/32 198.51.100.0\/24" \\n  1 db.example.org/"203.0.113.5\/32 198.51.100.0\/24" \\n  1 1 db.example.org/ or die 4;
$s =~ s/"203.0.113.5" 2 2 \|/"203.0.113.5" 2 2 2 |/ or die 5;
$s =~ s/(grep -q '\^DB_HOST=db.example.org' "\$env1")/grep -q '^TLS_MODE=acme' "\$env1" || fail "the first choice (Let's Encrypt) was not written"\n$1/ or die 6;
spit($f,$s);
$f="deploy/swarm/stack.sh"; $s=slurp($f);
$s =~ s/TLS_MODE="\$\{TLS_MODE:-self\}"/TLS_MODE="\${TLS_MODE:-acme}"/ or die 7;
spit($f,$s);
