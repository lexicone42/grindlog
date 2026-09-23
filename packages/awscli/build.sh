#!/bin/sh
set -ex

# As the registry's httpie: let pip resolve and install the dependency tree
# into this package's own site-packages.
pip3 install --root "$OUTPUT_DIR" .

if [ ! -x "$OUTPUT_DIR/usr/bin/aws" ]; then
  echo "awscli: pip did not install the 'aws' entrypoint" >&2
  exit 1
fi
