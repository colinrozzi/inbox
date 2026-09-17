#!/bin/sh
# Derive a deployable manifest from an in-repo actor manifest (the tested source
# of truth) for ANY package target. Repoints `package` at <package_ref>, sets
# static_package=true, and injects the deploy-only TLS stanzas the in-repo
# skeletons omit. Prints the derived manifest TOML to stdout.
#
# Usage: ops/derive-manifest.sh <actor_dir> <tls> <package_ref>
#   <actor_dir>    dir containing manifest.toml (e.g. api-handler)
#   <tls>          client | server | ""   (client_tls@api-handler for outbound
#                                           STARTTLS; server_tls@smtp-handler for :25)
#   <package_ref>  the package value to set verbatim — an https release URL, a
#                  store node URL (http://<node>/by-hash/<sha256>), a store://
#                  ref, a local path, etc. The transform is agnostic to it.
#
# Single source of the derive transform, reused by:
#   - .github/workflows/release.yml  (package_ref = the release-asset wasm URL)
#   - the fleet store's `store publish` (package_ref = http://<node>/by-hash/<hash>)
# so the published manifests can never drift from the tested in-repo ones.
#
# NOTE: the server_tls cert/key paths are inbox-prod-specific (certbot on
# mail.colinrozzi.com). If a consumer needs per-node paths, promote them to args
# — flagged as a follow-up, kept hardcoded here for parity with the prior inline
# release.yml behavior.
set -eu

if [ "$#" -ne 3 ]; then
    echo "usage: $0 <actor_dir> <tls: client|server|''> <package_ref>" >&2
    exit 2
fi
actor_dir="$1"
tls="$2"
package_ref="$3"

manifest="${actor_dir%/}/manifest.toml"
if [ ! -f "$manifest" ]; then
    echo "derive-manifest: no manifest at '$manifest'" >&2
    exit 1
fi

awk -v pkg="$package_ref" -v tls="$tls" '
    /^package = /        { print "package = \"" pkg "\""; print "static_package = true"; next }
    /^static_package = / { next }
    { print }
    /^type = "tcp"$/ {
        if (tls == "client") {
            print ""; print "[handler.client_tls]"; print "enabled = true"; print "auto_handshake = false"
        } else if (tls == "server") {
            print ""; print "[handler.server_tls]"; print "enabled = true"
            print "cert = \"/etc/letsencrypt/live/mail.colinrozzi.com/fullchain.pem\""
            print "key = \"/etc/letsencrypt/live/mail.colinrozzi.com/privkey.pem\""
        }
    }
' "$manifest"
