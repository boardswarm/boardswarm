
# Authentication via JWT token

Generate jwks set:

$ rnbyc -j -g  Ed25519  -x   -o private.jwks -p auth.jwks

Create user token (expiry 01-01-2025)

$ rnbyc  -s '{"exp": 1735686000 }' -K private.jwks -o token

