#!/usr/bin/env bash
set -xe
# If we have no $SERVER_NAME set, abort
if [ -z "$SERVER_NAME" ]; then
  echo "SERVER_NAME is not set, aborting"
  exit 1
fi

# If /complement/ca/ca.crt or /complement/ca/ca.key are missing, abort
if [ ! -f /complement/ca/ca.crt ] || [ ! -f /complement/ca/ca.key ]; then
  echo "/complement/ca/ca.crt or /complement/ca/ca.key is missing, aborting"
  exit 1
fi

# Add the root cert to the local trust store
echo 'Installing Complement CA certificate to local trust store'
cp /complement/ca/ca.crt /usr/local/share/ca-certificates/complement-ca.crt
update-ca-certificates

# Sign a certificate for our $SERVER_NAME
echo "Generating and signing certificate for $SERVER_NAME"
openssl genrsa -out "/$SERVER_NAME.key" 2048

echo "Generating CSR for $SERVER_NAME"
openssl req -new -sha256 \
  -key "/$SERVER_NAME.key" \
  -out "/$SERVER_NAME.csr" \
  -subj "/C=US/ST=CA/O=Continuwuity, Inc./CN=$SERVER_NAME"\
  -addext "subjectAltName=DNS:$SERVER_NAME"
openssl req -in "$SERVER_NAME.csr" -noout -text

echo "Signing certificate for $SERVER_NAME with Complement CA"
cat <<EOF > ./cert.ext
authorityKeyIdentifier=keyid,issuer
basicConstraints = CA:FALSE
keyUsage = digitalSignature, keyEncipherment, dataEncipherment, nonRepudiation
extendedKeyUsage = serverAuth
subjectAltName = @alt_names
[alt_names]
DNS.1 = *.docker.internal
DNS.2 = hs1
DNS.3 = hs2
DNS.4 = hs3
DNS.5 = hs4
DNS.6 = $SERVER_NAME
IP.1 = 127.0.0.1
EOF
openssl x509 \
  -req \
  -in "/$SERVER_NAME.csr" \
  -CA /complement/ca/ca.crt \
  -CAkey /complement/ca/ca.key \
  -CAcreateserial \
  -out "/$SERVER_NAME.crt" \
  -days 1 \
  -sha256 \
  -extfile ./cert.ext

# Tell continuwuity where to find the certs
export CONTINUWUITY_TLS__KEY="/$SERVER_NAME.key"
export CONTINUWUITY_TLS__CERTS="/$SERVER_NAME.crt"
# And who it is
export CONTINUWUITY_SERVER_NAME="$SERVER_NAME"

echo "Starting Continuwuity with SERVER_NAME=$SERVER_NAME"

# Start conduwuit in the background, teeing output so we can parse the
# first-run registration token from the welcome banner.
LOG_FILE="/tmp/conduwuit_startup.log"
/usr/local/bin/conduwuit --config /etc/continuwuity/config.toml 2>&1 | tee "$LOG_FILE" &
CONDUWUIT_PID=$!

# Wait for conduwuit to be ready (listening on port 8008)
echo "Waiting for Continuwuity to start..."
for _ in $(seq 1 30); do
  if curl -sf "http://127.0.0.1:8008/_matrix/client/versions" > /dev/null 2>&1; then
    echo "Continuwuity is ready!"
    break
  fi
  if ! kill -0 "$CONDUWUIT_PID" 2>/dev/null; then
    echo "Continuwuity exited unexpectedly"
    exit 1
  fi
  sleep 0.5
done

# Bootstrap the first admin user to exit first-run mode.
# During first-run the server generates a one-time registration token and
# prints it in the welcome banner. We parse it from the log output, then
# complete the two-step UIAA flow (dummy + registration_token) to create
# the first user. This disables first-run mode so subsequent Complement
# test registrations can use plain m.login.dummy auth.
echo "Bootstrapping first admin user to exit first-run mode..."

# Give the banner a moment to print
sleep 1

# Extract the token from "registration token XXXXX ."
TOKEN=$(grep -oP 'registration token \K\S+' "$LOG_FILE" | head -1)

if [ -z "$TOKEN" ]; then
  echo "WARNING: Could not find registration token in startup log."
  echo "Log contents:"
  cat "$LOG_FILE"
else
  echo "Found first-run registration token: $TOKEN"

  # Step 1: Initial request without auth to get a session
  RESP=$(curl -s -X POST "http://127.0.0.1:8008/_matrix/client/v3/register" \
    -H "Content-Type: application/json" \
    -d '{"username":"admin","password":"complement_admin_password"}')
  SESSION=$(echo "$RESP" | python3 -c "import sys,json; print(json.load(sys.stdin).get('session',''))" 2>/dev/null || true)

  if [ -n "$SESSION" ]; then
    # Step 2: Complete the registration_token stage
    RESP2=$(curl -s -X POST "http://127.0.0.1:8008/_matrix/client/v3/register" \
      -H "Content-Type: application/json" \
      -d "{\"username\":\"admin\",\"password\":\"complement_admin_password\",\"auth\":{\"type\":\"m.login.registration_token\",\"token\":\"$TOKEN\",\"session\":\"$SESSION\"}}")
    echo "First user registration response: $RESP2"

    # Check if we got an access_token back (success)
    if echo "$RESP2" | python3 -c "import sys,json; d=json.load(sys.stdin); assert 'access_token' in d" 2>/dev/null; then
      echo "Successfully bootstrapped first admin user! First-run mode disabled."
    else
      echo "WARNING: First user registration may have failed. Response: $RESP2"
    fi
  else
    echo "WARNING: Could not get UIAA session. Response: $RESP"
  fi
fi

# Keep conduwuit running in the foreground for Complement
wait "$CONDUWUIT_PID"
