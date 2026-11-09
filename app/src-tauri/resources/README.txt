For a local build: put your server CA certificate here as forever_root_ca.crt (from certs/ca/ after
scripts/gen-certs.sh), or set the FOREVER_CA_CERT variable to its text. It is public; never copy the .key.
GitHub Actions uses the FOREVER_CA_CERT repository variable instead (see the README).
