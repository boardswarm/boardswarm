# Boardswarm Web UI

Browser-based interface for boardswarm, built with [Dioxus](https://dioxuslabs.com/)
(Rust/WASM) and communicating via gRPC-Web and WebSocket.

## Features

- Device listing and detail view
- Serial console via xterm.js (WebSocket with protobuf framing)
- Device mode switching
- OIDC and static JWT authentication (reuses server auth configuration)

## Prerequisites

- Rust toolchain with the `wasm32-unknown-unknown` target:
  ```
  rustup target add wasm32-unknown-unknown
  ```
  On Debian/Ubuntu without rustup:
  ```
  apt install libstd-rust-dev-wasm32
  ```
- [Dioxus CLI](https://dioxuslabs.com/learn/0.7/getting_started#install-the-dioxus-cli) (`dx`):
  ```
  cargo install dioxus-cli
  ```

## Building

> **Note:** Due to a dioxus-cli bug with workspace `default-members` path
> resolution, `dx` must be run from the **workspace root**, not from inside
> `boardswarm-web/`.

### Development (with hot-reload)

Start the boardswarm server (no `--web-ui` flag needed during development):

```
boardswarm server.conf
```

> **Important:** The `dx serve` proxy only supports plain HTTP backends — it
> does **not** support TLS (HTTPS). Your boardswarm server must be configured
> **without** a `certificate:` block in server.conf when using `dx serve`.
> If you need TLS in production, use a production build instead (see below).

Then from the workspace root, start the Dioxus dev server:

```
dx serve --package boardswarm-web
```

Open http://localhost:8080. The dev server automatically proxies all API
traffic to boardswarm at `http://localhost:6683`:
- `/boardswarm.Boardswarm/*` — all gRPC-Web calls
- `/api/*` — WebSocket console

The browser stays same-origin throughout — no CORS configuration needed.

**If your boardswarm server runs on a different address**, edit the `backend`
URLs in `boardswarm-web/Dioxus.toml`:

```toml
[[web.proxy]]
backend = "http://myserver:6683/boardswarm.Boardswarm"

[[web.proxy]]
backend = "http://myserver:6683/api"
```

Note the `http://` scheme — HTTPS backends are not supported by `dx serve`'s
built-in proxy (see above).

### Production build

```
dx build --release --package boardswarm-web
```

The compiled assets (HTML, JS, WASM) are placed in
`target/dx/boardswarm-web/release/web/public/`. These are static
files that can be served by any web server or by boardswarm itself.

## Serving from boardswarm

The boardswarm server can serve the web UI directly using the `--web-ui` flag,
which points to the directory containing the built assets:

```
boardswarm --web-ui target/dx/boardswarm-web/release/web/public/ server.conf
```

This serves the web UI as a fallback on the same port as the gRPC API (default
6683), so both the API and UI share a single origin — no CORS needed.

## Architecture

```
┌──────────────────────────────────────────────────┐
│  Browser                                         │
│  ┌─────────────┐  ┌───────────┐  ┌────────────┐ │
│  │ Dioxus WASM │──│ gRPC-Web  │──│ xterm.js   │ │
│  │ (UI logic)  │  │ (API)     │  │ (terminal) │ │
│  └──────┬──────┘  └─────┬─────┘  └──────┬─────┘ │
└─────────┼───────────────┼───────────────┼────────┘
          │               │               │
     HTTP/static    gRPC-Web (HTTP)   WebSocket
          │               │               │
┌─────────┴───────────────┴───────────────┴────────┐
│  boardswarm server                               │
│  ┌──────────┐  ┌──────────────┐  ┌────────────┐ │
│  │ ServeDir │  │ tonic-web    │  │ WS console │ │
│  │ (static) │  │ (gRPC proxy) │  │ handler    │ │
│  └──────────┘  └──────────────┘  └────────────┘ │
└──────────────────────────────────────────────────┘
```

- **gRPC-Web**: Device listing, device info, mode changes — all standard
  boardswarm RPCs, adapted for the browser via `tonic-web`.
- **WebSocket** (`/api/ws/console`): Bidirectional console I/O using
  protobuf-encoded binary frames (`ConsoleInputRequest` / `ConsoleOutput`).
  Authenticates via `?token=<jwt>` query parameter.

## Authentication

The web UI reuses the server's existing authentication configuration:

- **OIDC**: The UI fetches `LoginInfo` from the server (unauthenticated RPC),
  discovers OIDC endpoints, and performs an Authorization Code + PKCE flow
  in the browser. The resulting JWT is stored in `sessionStorage`.
- **Static JWT**: Paste a pre-generated token directly into the login form.
