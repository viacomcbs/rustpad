#!/usr/bin/env bash
# Installs the default Piston runtimes into the local compose stack.
# Needs internet access. Run while the stack is up.
set -euo pipefail

compose_file="$(dirname "$0")/compose.yml"
packages=(python node typescript go rust java gcc mono bash)

# Calls Piston's API on localhost from inside its own container. The Piston
# image has no curl, so the request runs through its python3.
piston() {
  podman compose -f "$compose_file" exec -T piston python3 - "$@" <<'PY'
import sys, urllib.error, urllib.request

method, path = sys.argv[1], sys.argv[2]
data = sys.argv[3].encode() if len(sys.argv) > 3 else None
request = urllib.request.Request(
    "http://localhost:2000" + path,
    data=data,
    method=method,
    headers={"Content-Type": "application/json"},
)
try:
    with urllib.request.urlopen(request, timeout=600) as response:
        print(response.read().decode())
except urllib.error.HTTPError as e:
    print(e.read().decode())
    sys.exit(1)
PY
}

for package in "${packages[@]}"; do
  echo "Installing ${package}"
  piston POST /api/v2/packages "{\"language\":\"${package}\",\"version\":\"*\"}" \
    || echo "  failed or already installed: ${package}"
  echo
done

echo "Installed runtimes:"
piston GET /api/v2/runtimes
echo
