# vise

bins:
- vise-cli - used to interact with the vise API
- vise-server - the HTTP server that serves the vise API

crates:
- vise-api - the API crate
- vise-client - automatically generates a client for the vise API

## Developing

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup, formatting, linting and the
checks CI runs. The short version:

```sh
cp .env.example .env
just db-up && just db-migrate
just check      # fmt, clippy, tests, sqlx cache, openapi spec
```
