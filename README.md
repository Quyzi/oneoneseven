# oneoneseven

Ephemeral encrypted one-time object store. Upload text or a file, get back a secret URL. The URL works exactly once - after retrieval the object is gone. All objects are encrypted at rest with AES-256-GCM; the key is never stored server-side.

## Usage

### Web UI

Navigate to `/` in a browser. Paste text or pick a file, hit store, copy the URL.

### API

**Store an object**
```
PUT /store
Content-Type: <mime type>
<body>
```
Returns `200 OK` with `{uuid}/{hex-key}` as plain text on success.

**Retrieve an object**
```
GET /take/{uuid}/{hex-key}
```
Returns the original bytes with the original `Content-Type` on a hit. On a miss (wrong key, already consumed, expired, or bogus ID) returns a random dummy payload - all failure modes are indistinguishable by design.

## Configuration

All options are available as CLI flags or environment variables.

| Flag | Env var | Default | Description |
|---|---|---|---|
| `--listen` / `-L` | `LISTEN` | `127.0.0.1:8420` | Address and port to bind |
| `--max-object-size` / `-o` | `MAX_OBJECT_SIZE` | `1000000` | Max object size in bytes |
| `--max-objects` / `-m` | `MAX_OBJECTS` | `256` | Max number of objects stored at once |
| `--max-slots` / `-s` | `MAX_SLOTS` | `10` | Max concurrent requests |
| `--max-age-secs` / `-a` | `MAX_AGE_SECS` | `3600` | Max object lifetime in seconds |
| `--prune-secs` / `-p` | `PRUNE_SECS` | `60` | Pruner interval in seconds |

Log level is controlled via `RUST_LOG` (e.g. `RUST_LOG=info`).

## Docker

```sh
docker build -t oneoneseven .
docker run -e RUST_LOG=info -p 9000:9000 oneoneseven --listen 0.0.0.0:9000
```

Or with environment variables:
```sh
docker run \
  -e RUST_LOG=info \
  -e LISTEN=0.0.0.0:9000 \
  -e MAX_OBJECT_SIZE=5000000 \
  -p 9000:9000 \
  oneoneseven
```

## Unraid

Copy `oneoneseven.xml` to `/boot/config/plugins/dockerMan/templates-user/` on your Unraid server, then add the container via Docker - Add Container and select the template.

## Metrics

Prometheus metrics are available at `GET /metrics`.
