#!/usr/bin/env bash
# The questions of `rusty-swarm init` when there is no swarm.env yet: a few dialogs (whiptail, when the machine has it and there is a terminal), else plain prompts; it writes swarm.env (mode 0600) and the
# files of the passwords it was given (STATE_DIR, mode 0600: a password is never written in swarm.env, only the path of its file). Sourced by rusty-swarm; not run on its own.
#
# RUSTY_SWARM_UI=plain forces the plain prompts (and lets a test feed the answers on the standard input); a cancelled dialog or an end of input stops the wizard and writes nothing.

UI=plain
if [ "${RUSTY_SWARM_UI:-}" != plain ] && command -v whiptail >/dev/null 2>&1 && [ -t 0 ] && [ -t 1 ]; then UI=whiptail; fi

wiz_abort() { echo "rusty-swarm: the questions were cancelled; nothing was written" >&2; exit 130; }

# ui_input TEXT DEFAULT -> the answer on the standard output
ui_input() {
  local text="$1" def="${2:-}" v
  if [ "$UI" = whiptail ]; then
    v="$(whiptail --title "Rusty Mail Server" --inputbox "$text" 11 74 "$def" 3>&1 1>&2 2>&3)" || wiz_abort
  else
    read -r -p "$text [$def]: " v >&2 || wiz_abort
  fi
  printf '%s\n' "${v:-$def}"
}

# ui_password TEXT -> the answer (not echoed)
ui_password() {
  local v
  if [ "$UI" = whiptail ]; then
    v="$(whiptail --title "Rusty Mail Server" --passwordbox "$1" 10 74 3>&1 1>&2 2>&3)" || wiz_abort
  else
    read -r -s -p "$1: " v >&2 || wiz_abort
    echo >&2
  fi
  printf '%s\n' "$v"
}

# ui_yesno TEXT DEFAULT(y|n) -> status 0 for yes
ui_yesno() {
  local v
  if [ "$UI" = whiptail ]; then
    if [ "${2:-y}" = n ]; then whiptail --title "Rusty Mail Server" --defaultno --yesno "$1" 10 74; else whiptail --title "Rusty Mail Server" --yesno "$1" 10 74; fi
    return
  fi
  read -r -p "$1 [$( [ "${2:-y}" = y ] && echo Y/n || echo y/N )]: " v >&2 || wiz_abort
  v="${v:-${2:-y}}"
  case "$v" in y|Y|yes|YES) return 0 ;; *) return 1 ;; esac
}

# ui_menu TEXT TAG ITEM [TAG ITEM ...] -> the chosen tag
ui_menu() {
  local text="$1" v i=0 n; shift
  if [ "$UI" = whiptail ]; then
    v="$(whiptail --title "Rusty Mail Server" --notags --menu "$text" 16 78 6 "$@" 3>&1 1>&2 2>&3)" || wiz_abort
    printf '%s\n' "$v"; return
  fi
  local tags=() items=()
  while [ $# -gt 1 ]; do tags+=("$1"); items+=("$2"); shift 2; done
  echo "$text" >&2
  for n in "${!tags[@]}"; do i=$((n + 1)); echo "  $i) ${items[$n]}" >&2; done
  read -r -p "Choose [1]: " v >&2 || wiz_abort
  v="${v:-1}"
  [[ "$v" =~ ^[0-9]+$ ]] && [ "$v" -ge 1 ] && [ "$v" -le "${#tags[@]}" ] || { echo "rusty-swarm: '$v' is not one of the choices" >&2; wiz_abort; }
  printf '%s\n' "${tags[$((v - 1))]}"
}

# ask_valid TEXT DEFAULT REGEX MESSAGE -> an answer that matches (asked again up to three times)
ask_valid() {
  local text="$1" def="$2" re="$3" msg="$4" v
  for _ in 1 2 3; do
    v="$(ui_input "$text" "$def")"
    if [[ "$v" =~ $re ]]; then printf '%s\n' "$v"; return; fi
    if [ "$UI" = whiptail ]; then whiptail --title "Rusty Mail Server" --msgbox "$msg" 9 74; else echo "  $msg" >&2; fi
  done
  echo "rusty-swarm: no valid answer to: $text" >&2; wiz_abort
}

# secret_to_file NAME VALUE -> the path of the file (0600) that holds it
secret_to_file() {
  local f="$STATE_DIR/$1"
  ( umask 077; mkdir -p "$STATE_DIR"; chmod 0700 "$STATE_DIR"; printf '%s' "$2" > "$f" )
  printf '%s\n' "$f"
}

run_wizard() {
  local out="$1" where host pubip mailhost adminhost contact allow dbmode s3mode
  local nodes db s3
  local re_host='^[A-Za-z0-9]([A-Za-z0-9.-]{0,251}[A-Za-z0-9])?$' re_ip='^[0-9]{1,3}(\.[0-9]{1,3}){3}$'
  echo "No inventory at $out: a few questions first (nothing is changed until they are answered)." >&2

  where="$(ui_menu "Where does the mail server run?" local "This machine (one server; run this on it)" ssh "Another server, over SSH (one manager node)")"
  if [ "$where" = local ]; then
    pubip="$(ask_valid "Public IPv4 address of this machine (the address mail leaves from and the names point to)" "$(hostname -I 2>/dev/null | awk '{ print $1 }')" "$re_ip" "An IPv4 address such as 203.0.113.10.")"
    nodes="  \"server|local|manager|outbound=true,av=true,db=true,s3=true|$pubip\""
  else
    host="$(ask_valid "The server as USER@HOST (SSH with a key; the user must be root or able to sudo)" "" '^[A-Za-z0-9._-]+@[A-Za-z0-9._:-]+$' "Something like ops@203.0.113.10.")"
    pubip="$(ask_valid "Public IPv4 address of that server" "${host#*@}" "$re_ip" "An IPv4 address such as 203.0.113.10.")"
    nodes="  \"server|ssh:$host|manager|outbound=true,av=true,db=true,s3=true|$pubip\""
  fi
  mailhost="$(ask_valid "Host name of the mail server (what mail clients connect to), for example mail.example.org" "" "$re_host" "A host name such as mail.example.org.")"
  adminhost="$(ask_valid "Host name of the admin web, for example admin.example.org" "" "$re_host" "A host name such as admin.example.org.")"
  contact="$(ask_valid "E-mail address for the certificate authority (Let's Encrypt writes to it)" "" '^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}$' "An e-mail address.")"
  allow="$(ask_valid "Networks that may open the admin web (an address or a block, separated by spaces; for example 203.0.113.5/32)" "" '^[0-9]{1,3}(\.[0-9]{1,3}){3}(/[0-9]{1,2})?( [0-9]{1,3}(\.[0-9]{1,3}){3}(/[0-9]{1,2})?)*$' "IPv4 addresses or blocks.")"
  case " $allow " in *" 0.0.0.0/0 "*) echo "rusty-swarm: 0.0.0.0/0 would open the admin web to everyone" >&2; wiz_abort ;; esac

  dbmode="$(ui_menu "PostgreSQL" external "Use my own PostgreSQL (recommended)" stack "Run a default PostgreSQL in the cluster (one instance, no failover)")"
  db=""
  if [ "$dbmode" = external ]; then
    local dbhost dbport dbname dbuser dbpw dbtls=1
    dbhost="$(ask_valid "PostgreSQL host" "" "$re_host" "A host name or address.")"
    dbport="$(ask_valid "PostgreSQL port" "5432" '^[0-9]{1,5}$' "A port number.")"
    dbname="$(ask_valid "Database name (created if it does not exist)" "rusty" '^[a-z][a-z0-9_]{0,62}$' "Lower case letters, digits and _.")"
    dbuser="$(ask_valid "Login that may create roles (it stays yours: it is used for the preparation only and is never put in the cluster)" "postgres" '^[A-Za-z][A-Za-z0-9_]{0,62}$' "A role name.")"
    dbpw="$(ui_password "Password of $dbuser")"
    [ -n "$dbpw" ] || { echo "rusty-swarm: the password is empty" >&2; wiz_abort; }
    ui_yesno "Encrypt the connection to PostgreSQL (TLS, certificate checked)? Answer no only on a trusted private network." y || dbtls=0
    db="DB_HOST=$dbhost
DB_PORT=$dbport
DB_NAME=$dbname
DB_ADMIN_USER=$dbuser
DB_ADMIN_PASSWORD_FILE=\"$(secret_to_file db-admin.pw "$dbpw")\"
DB_TLS=$dbtls"
  else
    db="# DB_HOST is empty: the cluster runs a default PostgreSQL (the superuser password is made for you and kept in the state directory)"
  fi

  s3mode="$(ui_menu "Storage for message bodies (S3)" external "Use my own S3 (recommended)" stack "Run a default RustFS in the cluster (one instance, no failover)")"
  s3=""
  if [ "$s3mode" = external ]; then
    local ep region bucket ak sk
    ep="$(ask_valid "S3 endpoint URL" "https://s3.example.org" '^https?://[A-Za-z0-9._:-]+(/.*)?$' "An http(s) URL.")"
    region="$(ask_valid "S3 region" "us-east-1" '^[a-z0-9-]{2,32}$' "A region such as us-east-1.")"
    bucket="$(ask_valid "Bucket" "rusty-mail" '^[a-z0-9][a-z0-9.-]{2,62}$' "A bucket name.")"
    ak="$(ask_valid "Access key" "" '^[A-Za-z0-9+/=_-]{3,128}$' "An access key.")"
    sk="$(ui_password "Secret key")"
    [ -n "$sk" ] || { echo "rusty-swarm: the secret key is empty" >&2; wiz_abort; }
    s3="S3_ENDPOINT=$ep
S3_REGION=$region
S3_BUCKET=$bucket
S3_ACCESS_KEY=$ak
S3_SECRET_KEY_FILE=\"$(secret_to_file s3-secret "$sk")\""
  else
    s3="# S3_ENDPOINT is empty: the cluster runs a default RustFS (its keys are made for you and kept in the state directory)"
  fi

  ( umask 077
    cat > "$out" <<EOF
# Written by rusty-swarm init (the questions). Edit it by hand to change anything; passwords are in files, never here.
NODES=(
$nodes
)
ENCRYPTED=0
STATE_DIR="$STATE_DIR"
MAIL_HOST=$mailhost
ADMIN_HOST=$adminhost
CONTACT=$contact
ADMIN_ALLOW="$allow"
$db
$s3
EOF
  )
  chmod 0600 "$out"
  echo "rusty-swarm: the answers are in $out (mode 0600)" >&2
}
