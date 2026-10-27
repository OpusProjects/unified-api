#!/bin/sh
# Sample ssh connector script (location: instance): streamed to each host
# and run there; prints that host's operating system as its variables.
j() { printf '%s' "$1" | tr -d '"\\'; }
ID=; NAME=; VERSION_ID=
[ -r /etc/os-release ] && . /etc/os-release
printf '{"osinfo":{"id":"%s","name":"%s","version":"%s","kernel":"%s","arch":"%s"}}\n' \
  "$(j "$ID")" "$(j "$NAME")" "$(j "$VERSION_ID")" "$(uname -r)" "$(uname -m)"
