#!/usr/bin/env bash
# Installs the default Piston runtimes into the local compose stack.
# Needs internet access. Run from the repository root while the stack is up.
set -euo pipefail

packages=(python node typescript go rust java gcc mono bash)

piston() {
  docker run --rm --network rustpad-piston curlimages/curl:8.10.1 -fsS "$@"
}

for package in "${packages[@]}"; do
  echo "Installing ${package}"
  piston -X POST http://piston:2000/api/v2/packages \
    -H 'Content-Type: application/json' \
    -d "{\"language\":\"${package}\",\"version\":\"*\"}" \
    || echo "  failed or already installed: ${package}"
  echo
done

echo "Installed runtimes:"
piston http://piston:2000/api/v2/runtimes
echo
