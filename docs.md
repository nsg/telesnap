# Telesnap agent guide

Telesnap is a bearer-authenticated HTTP API for installing and exercising an
unsigned snap on a disposable Ubuntu test host. It runs as root, accepts only
application snaps that declare `confinement: strict`, records every snap it
installs, and automatically purges it after a required lifetime.

Use Telesnap only for untrusted test workloads on an isolated host. The API
does not make an uploaded snap trustworthy.

## Connect

Set the service address and the token supplied by the operator:

```bash
export TELESNAP_URL="http://127.0.0.1:8080"
export TELESNAP_TOKEN="replace-with-the-operator-supplied-token"
export TELESNAP_AUTH="Authorization: Bearer $TELESNAP_TOKEN"
```

`GET /`, `GET /health`, and `GET /docs.md` are public. Every `/v1` endpoint
requires the bearer token. A healthy service returns `200 {"status":"ok"}`;
`503` means the HTTP service is running but snapd is not ready.

## Upload and install a snap

Send the `.snap` file itself as the raw request body. Do not use a URL, JSON, or
a multipart form. The local filename is ignored; Telesnap reads the snap name
from its embedded metadata. `lifetime_seconds` is required and starts when
snapd confirms the install, not when the upload begins.

```bash
curl --fail-with-body \
  --request POST \
  --header "$TELESNAP_AUTH" \
  --header "Content-Type: application/octet-stream" \
  --data-binary @./package.snap \
  "$TELESNAP_URL/v1/snaps/install?lifetime_seconds=900"
```

The upload is streamed directly to a private temporary file and is not held in
memory. The default maximum is 2 GiB and the default upload timeout is one
hour, so snap files of at least 1 GiB are supported. An intervening reverse
proxy must allow an equally large request body and timeout.

A successful install returns `201`:

```json
{
  "name": "example",
  "installed_at": "2026-09-04T12:00:00Z",
  "expires_at": "2026-09-04T12:15:00Z"
}
```

Save `name`; it identifies the snap in the remaining calls. If the initial
snapd operation times out, Telesnap retains a pending record and reconciles or
purges it in the background.

## Inspect managed snaps

```bash
curl --fail-with-body --header "$TELESNAP_AUTH" \
  "$TELESNAP_URL/v1/snaps"
```

The response includes both pending and installed snaps, their lifecycle state,
and relevant timestamps.

## Read and change configuration

```bash
# Read all configuration or one key.
curl --fail-with-body --header "$TELESNAP_AUTH" \
  "$TELESNAP_URL/v1/snaps/$SNAP_NAME/config"
curl --fail-with-body --header "$TELESNAP_AUTH" \
  "$TELESNAP_URL/v1/snaps/$SNAP_NAME/config/server.port"

# Set a typed JSON value, then unset it.
curl --fail-with-body --request PUT --header "$TELESNAP_AUTH" \
  --json '{"value":8080}' \
  "$TELESNAP_URL/v1/snaps/$SNAP_NAME/config/server.port"
curl --fail-with-body --request DELETE --header "$TELESNAP_AUTH" \
  "$TELESNAP_URL/v1/snaps/$SNAP_NAME/config/server.port"
```

## Logs and service control

```bash
# Read logs for all services, or select one service.
curl --fail-with-body --header "$TELESNAP_AUTH" \
  "$TELESNAP_URL/v1/snaps/$SNAP_NAME/logs?lines=200"
curl --fail-with-body --header "$TELESNAP_AUTH" \
  "$TELESNAP_URL/v1/snaps/$SNAP_NAME/logs?service=worker&lines=100"

# Start, stop, or restart all services. Add ?service=worker to target one.
curl --fail-with-body --request POST --header "$TELESNAP_AUTH" \
  "$TELESNAP_URL/v1/snaps/$SNAP_NAME/services/start"
curl --fail-with-body --request POST --header "$TELESNAP_AUTH" \
  "$TELESNAP_URL/v1/snaps/$SNAP_NAME/services/stop?service=worker"
curl --fail-with-body --request POST --header "$TELESNAP_AUTH" \
  "$TELESNAP_URL/v1/snaps/$SNAP_NAME/services/restart"
```

Log `lines` must be from 1 through 1000. Configuration values may be any JSON
value and are limited to 64 KiB after encoding.

## Remove a snap

```bash
# Normal removal.
curl --fail-with-body --request DELETE --header "$TELESNAP_AUTH" \
  "$TELESNAP_URL/v1/snaps/$SNAP_NAME"

# Removal with snap data purged.
curl --fail-with-body --request DELETE --header "$TELESNAP_AUTH" \
  "$TELESNAP_URL/v1/snaps/$SNAP_NAME/purge"
```

Both return `204`. Run only one removal operation for an installation. Telesnap
also purges the snap automatically when its lifetime expires.

## Errors

Errors are JSON and use the HTTP status appropriate to the failure:

```json
{
  "error": {
    "code": "bad_request",
    "message": "reason for the failure"
  }
}
```

Common statuses are `400` for an invalid request or upload, `401` for a missing
or incorrect token, `404` for a snap not managed by Telesnap, `409` for an
already installed or pending snap, `408` for an upload timeout, `413` for an
oversized upload, `502` for a failed snap command, and `504` for a snap command
timeout. A third concurrent upload or installation receives `429`; retry it
after one of the two active operations finishes.
