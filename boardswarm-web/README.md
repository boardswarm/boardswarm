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

### Development (with hot-reload)

```
cd boardswarm-web
dx serve
```

This starts a local dev server (default `http://localhost:8080`) that proxies
API requests to a running boardswarm server. Configure the proxy target if
the server is not on `localhost:6683`.

### Production build

```
cd boardswarm-web
dx build --release
```

The compiled assets (HTML, JS, WASM) are placed in
`../target/dx/boardswarm-web/release/web/public/`. These are static
files that can be served by any web server or by boardswarm itself.

## Serving from boardswarm

The boardswarm server can serve the web UI directly using the `--web-ui` flag,
which points to the directory containing the built assets:

```
boardswarm --web-ui boardswarm-web/dist/ server.conf
```

This serves the web UI as a fallback on the same port as the gRPC API (default
6683), so both the API and UI share a single origin — no CORS configuration
needed for production.

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
