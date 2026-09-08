#!/usr/bin/env bash
# The keypair and minter F15 needs, generated per run.
#
#   ./forge/e2e/f15-keys.sh <keydir>
#
# ONLY THE PUBLIC HALF EVER LEAVES THIS MACHINE — it goes into a Secret
# the door mounts read-only. The private key is the drill's issuer and
# is shredded with the rest of the run's key material at teardown, which
# is why it belongs in the session scratchpad and not in the repository.
#
# `other.pem` is a second, well-formed key the door is never told about:
# it is P1's control for "a valid signature from the wrong signer", and
# without it that leg could pass on any malformed input.
set -euo pipefail
D=${1:?usage: f15-keys.sh <keydir>}
mkdir -p "$D"
umask 077
for k in jwt other; do
  [ -f "$D/$k.pem" ] || openssl genpkey -algorithm RSA -pkeyopt rsa_keygen_bits:2048 -out "$D/$k.pem" 2>/dev/null
  chmod 600 "$D/$k.pem"
done
openssl rsa -in "$D/jwt.pem" -pubout -out "$D/jwt.pub" 2>/dev/null
chmod 644 "$D/jwt.pub"

cat > "$D/mint.sh" <<'EOF'
#!/usr/bin/env bash
# An RS256 JWT, with openssl and nothing else.
#   mint.sh <priv.pem> <kid> <iss> <sub> <aud> <iat> <exp>
# Verified against the door's own verifier before F15 was written, so a
# refusal on the wire is the door's answer and not a malformed token.
set -euo pipefail
b64() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }
KEY=$1 KID=$2 ISS=$3 SUB=$4 AUD=$5 IAT=$6 EXP=$7
H=$(printf '{"alg":"RS256","typ":"JWT","kid":"%s"}' "$KID" | b64)
P=$(printf '{"iss":"%s","sub":"%s","aud":"%s","iat":%s,"exp":%s}' "$ISS" "$SUB" "$AUD" "$IAT" "$EXP" | b64)
S=$(printf '%s.%s' "$H" "$P" | openssl dgst -sha256 -sign "$KEY" -binary | b64)
printf '%s.%s.%s' "$H" "$P" "$S"
EOF
chmod 700 "$D/mint.sh"

echo "keydir:     $D"
echo "public key: $D/jwt.pub   (mount this; deploy with JWT_PUBKEY=$D/jwt.pub)"
echo "private:    $D/jwt.pem   (mode 600, shred at teardown)"
