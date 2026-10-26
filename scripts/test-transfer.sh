#!/usr/bin/env bash
# Isolated transport fixture: actual OpenSSH SFTP, no network / production credentials.
set -euo pipefail
: "${TF_TEST_SFTP_SERVER:=/usr/lib/openssh/sftp-server}"
test -x "$TF_TEST_SFTP_SERVER"
export TF_TEST_SFTP_SERVER
TF_FIXTURE_DIR="$(mktemp -d)"
export TF_FIXTURE_DIR
trap 'rm -rf -- "$TF_FIXTURE_DIR"' EXIT
mkdir "$TF_FIXTURE_DIR/bin"
cat > "$TF_FIXTURE_DIR/bin/ssh" <<'PY'
#!/usr/bin/python3
import os,sys,pathlib
args=sys.argv[1:]
if args[-1] == 'sftp':
    os.execv(os.environ['TF_TEST_SFTP_SERVER'], ['sftp-server'])
host=args[args.index('--')+1]
home=pathlib.Path(os.environ['TF_FIXTURE_DIR'])/host
home.mkdir(exist_ok=True)
os.environ['HOME']=str(home)
os.execv('/bin/sh', ['sh','-c',args[-1]])
PY
chmod 700 "$TF_FIXTURE_DIR/bin/ssh"
cat > "$TF_FIXTURE_DIR/bin/sftp" <<'SH'
#!/bin/sh
exec /usr/bin/sftp -S "$TF_FIXTURE_DIR/bin/ssh" "$@"
SH
chmod 700 "$TF_FIXTURE_DIR/bin/sftp"
export PATH="$TF_FIXTURE_DIR/bin:$PATH"
cargo test --locked real_sftp_resume_and_multihop_roundtrip -- --ignored --nocapture
