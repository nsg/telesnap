<div align="center">
  <h1>Telesnap</h1>
  <p>A short-lived HTTP API for installing and exercising test snaps.</p>
</div>

## About

Telesnap wraps the Ubuntu `snap` command for disposable snap testing. It downloads an unsigned `.snap` from an HTTP(S) URL, verifies that it is a SquashFS snap with strict confinement, installs it with `snap install --dangerous`, and purges it after a required lifetime.

The supplied systemd unit runs Telesnap as root and waits for snapd to finish seeding before the API starts. Telesnap can read and change snap configuration, read snap service logs, control services, and remove or purge only the snaps recorded in its own state file.

## Features

- Install unsigned, strict-confinement snaps from HTTP(S) URLs.
- Require a lifetime for every install and retry automatic purge after expiry.
- Reconcile interrupted installs and externally removed snaps at startup.
- List the snaps managed by Telesnap with pending/installed state and expiry times.
- Read, set, and unset snap configuration keys.
- Read logs for all services in a snap or for one named service.
- Start, stop, and restart all services in a snap or one named service.
- Remove a snap normally or purge it with `snap remove --purge`.
- Protect every `/v1` endpoint with bearer-token authentication.

## Quick Start

Install Rust 1.86 or newer with [rustup](https://rustup.rs/); Ubuntu 24.04's packaged Rust compiler is too old for the current dependency lockfile. Then install the native build and runtime packages:

```bash
sudo apt-get update
sudo apt-get install --yes \
  build-essential ca-certificates cmake curl openssl pkg-config \
  snapd squashfs-tools

rustc --version
cargo build --locked
```

Install the binary, systemd unit, and environment file:

```bash
export TELESNAP_API_TOKEN="$(openssl rand -hex 32)"

sudo install --mode 0755 target/debug/telesnap /usr/local/bin/telesnap
sudo install --mode 0644 packaging/telesnap.service \
  /etc/systemd/system/telesnap.service
sudo install --mode 0600 /dev/null /etc/default/telesnap
printf 'TELESNAP_API_TOKEN=%s\nTELESNAP_BIND=127.0.0.1:8080\n' \
  "$TELESNAP_API_TOKEN" | sudo tee /etc/default/telesnap >/dev/null

sudo systemctl daemon-reload
sudo systemctl enable --now telesnap.service
curl --fail-with-body http://127.0.0.1:8080/health
```

Inspect service logs with `sudo journalctl --unit telesnap --follow`.

Set reusable values for the API examples. Replace `SNAP_URL` with a direct URL that returns the snap file without redirecting:

```bash
export BASE_URL="http://127.0.0.1:8080"
export SNAP_URL="https://downloads.example.com/hello-world.snap"
export SNAP_NAME="hello-world"
export AUTH_HEADER="Authorization: Bearer $TELESNAP_API_TOKEN"
```

## Configuration

| Environment variable | Required | Default | Purpose |
| --- | --- | --- | --- |
| `TELESNAP_API_TOKEN` | Yes | None | Bearer token; must contain at least 32 visible ASCII bytes. |
| `TELESNAP_BIND` | No | `127.0.0.1:8080` | Listen address. |
| `TELESNAP_STATE_PATH` | No | `/var/lib/telesnap/expirations.json` | Persistent managed-snap and expiry state. |
| `TELESNAP_DOWNLOAD_DIR` | No | `/var/lib/telesnap/downloads` | Directory for temporary snap downloads. |
| `TELESNAP_MAX_DOWNLOAD_BYTES` | No | `536870912` | Maximum downloaded snap size in bytes. |
| `TELESNAP_MAX_LIFETIME_SECONDS` | No | `86400` | Maximum accepted install lifetime. |
| `TELESNAP_COMMAND_TIMEOUT_SECONDS` | No | `300` | Timeout for each `snap` or `unsquashfs` command. |
| `TELESNAP_PENDING_TIMEOUT_SECONDS` | No | Command timeout + 300 | Maximum time before an unfinished install change is aborted. |
| `TELESNAP_DOWNLOAD_TIMEOUT_SECONDS` | No | `300` | Timeout for the complete HTTP(S) download. |
| `TELESNAP_ALLOW_PRIVATE_URLS` | No | `false` | Permit download hosts that resolve to private or otherwise non-public IP addresses. |
| `RUST_LOG` | No | `telesnap=info` | Tracing filter for daemon logs. |

Numeric limits and timeouts must be greater than zero. The pending timeout must be at least as long as the command timeout. See [`packaging/telesnap.env.example`](packaging/telesnap.env.example) for an environment-file template and [`packaging/telesnap.service`](packaging/telesnap.service) for the systemd unit.

## API

`GET /health` is unauthenticated. Every `/v1` request requires `Authorization: Bearer <token>`.

| Method | Path | Success | Purpose |
| --- | --- | --- | --- |
| `GET` | `/health` | `200`, `503` | Report whether snapd is seeded and reachable. |
| `GET` | `/v1/snaps` | `200` | List managed snaps and their timestamps. |
| `POST` | `/v1/snaps/install` | `201` | Download and install a snap for a required lifetime. |
| `DELETE` | `/v1/snaps/{snap}` | `204` | Run `snap remove` and remove its Telesnap state. |
| `DELETE` | `/v1/snaps/{snap}/purge` | `204` | Run `snap remove --purge` and remove its Telesnap state. |
| `GET` | `/v1/snaps/{snap}/config` | `200` | Read all configuration as JSON. |
| `GET` | `/v1/snaps/{snap}/config/{key}` | `200` | Read one dot-separated configuration key as JSON. |
| `PUT` | `/v1/snaps/{snap}/config/{key}` | `204` | Set one key from a JSON `value`. |
| `DELETE` | `/v1/snaps/{snap}/config/{key}` | `204` | Unset one key. |
| `GET` | `/v1/snaps/{snap}/logs` | `200` | Read snap service logs. |
| `POST` | `/v1/snaps/{snap}/services/start` | `200` | Start snap services. |
| `POST` | `/v1/snaps/{snap}/services/stop` | `200` | Stop snap services. |
| `POST` | `/v1/snaps/{snap}/services/restart` | `200` | Restart snap services. |

Install a snap for 15 minutes:

```bash
curl --fail-with-body \
  --request POST \
  --header "$AUTH_HEADER" \
  --json "{\"url\":\"$SNAP_URL\",\"lifetime_seconds\":900}" \
  "$BASE_URL/v1/snaps/install"
```

The response contains `name`, `installed_at`, and `expires_at`. The lifetime must be from 1 through `TELESNAP_MAX_LIFETIME_SECONDS` and begins only after snapd confirms the installation. Expiry and interrupted snapd changes are reconciled about every five seconds.

List managed snaps (including their `pending` or `installed` status):

```bash
curl --fail-with-body --header "$AUTH_HEADER" "$BASE_URL/v1/snaps"
```

Pending entries also expose their request time, requested lifetime, and snapd change ID when one has been assigned. On restart, Telesnap recovers interrupted changes, aborts installs that outlive the configured pending timeout, removes definitively failed pending entries, and purges any install that completes after its pending deadline. Installed entries are removed from state if they were removed outside Telesnap.

Read all configuration, read one key, set it to a typed JSON value, and unset it:

```bash
curl --fail-with-body --header "$AUTH_HEADER" \
  "$BASE_URL/v1/snaps/$SNAP_NAME/config"

curl --fail-with-body --header "$AUTH_HEADER" \
  "$BASE_URL/v1/snaps/$SNAP_NAME/config/server.port"

curl --fail-with-body \
  --request PUT \
  --header "$AUTH_HEADER" \
  --json '{"value":8080}' \
  "$BASE_URL/v1/snaps/$SNAP_NAME/config/server.port"

curl --fail-with-body \
  --request DELETE \
  --header "$AUTH_HEADER" \
  "$BASE_URL/v1/snaps/$SNAP_NAME/config/server.port"
```

Configuration values may be any JSON value and are limited to 64 KiB after JSON encoding.

Read up to 200 log lines for all services, or up to 100 lines for the `worker` service:

```bash
curl --fail-with-body --header "$AUTH_HEADER" \
  "$BASE_URL/v1/snaps/$SNAP_NAME/logs?lines=200"

curl --fail-with-body --header "$AUTH_HEADER" \
  "$BASE_URL/v1/snaps/$SNAP_NAME/logs?service=worker&lines=100"
```

`lines` defaults to 200 and must be between 1 and 1000. Log responses contain `target`, `lines`, and `logs`.

Start all services, stop the `worker` service, and restart all services:

```bash
curl --fail-with-body --request POST --header "$AUTH_HEADER" \
  "$BASE_URL/v1/snaps/$SNAP_NAME/services/start"

curl --fail-with-body --request POST --header "$AUTH_HEADER" \
  "$BASE_URL/v1/snaps/$SNAP_NAME/services/stop?service=worker"

curl --fail-with-body --request POST --header "$AUTH_HEADER" \
  "$BASE_URL/v1/snaps/$SNAP_NAME/services/restart"
```

Service-action responses contain `target`, `action`, and command `output`.

Remove a managed snap, or purge it:

```bash
curl --fail-with-body --request DELETE --header "$AUTH_HEADER" \
  "$BASE_URL/v1/snaps/$SNAP_NAME"

curl --fail-with-body --request DELETE --header "$AUTH_HEADER" \
  "$BASE_URL/v1/snaps/$SNAP_NAME/purge"
```

Run only one of those removal commands for a given installation. Errors use this JSON shape:

```json
{
  "error": {
    "code": "bad_request",
    "message": "reason for the failure"
  }
}
```

## Security

Telesnap installs and controls untrusted packages through a root service. Use it only on a disposable, isolated test host or virtual machine. Do not install it on a production or personal system.

Downloaded snap files are installed with `--dangerous`, without signature-assertion verification, and only application snaps declaring `confinement: strict` are accepted. System, base, gadget, kernel, and snapd packages are rejected, as are reserved system snap names. This reduces exposure but does not make arbitrary snaps trustworthy. Telesnap also refuses to operate on snaps that are not present in its managed state.

Download URLs must use HTTP or HTTPS, may not contain credentials or fragments, and are never followed through redirects. By default, every resolved address must be public; setting `TELESNAP_ALLOW_PRIVATE_URLS=true` disables that SSRF protection. Downloads have configurable size and time limits and temporary files are removed after each install attempt.

The API serves plain HTTP and provides no rate limiting. Keep the bearer token secret, bind to a trusted interface, and place a TLS-terminating reverse proxy in front of Telesnap before any network exposure. The health endpoint does not require authentication; it returns `503` with `{"status":"unavailable"}` whenever the cached snapd readiness probe fails.

## Development

Install a Rust toolchain compatible with edition 2024, then run:

```bash
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
```

The daemon itself must run as root and expects `/usr/bin/snap` and `/usr/bin/unsquashfs`; the unit tests do not invoke those commands.

## License

Licensed under the MIT License.
