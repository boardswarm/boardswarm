# Boardswarm protocol definition

This crate contains the protobuf protocol definitions used for communication between boardswarm servers and clients.

## Overview

The boardswarm protocol defines gRPC services and message types for:

- **Device management**: listing and monitoring devices, consoles, volumes, and actuators
- **Authentication**: login info and JWT-based authentication
- **Console operations**: streaming input/output for device consoles
- **Volume operations**: reading, writing, seeking, and erasing device storage
- **Device control**: changing device modes and actuator states

## Protocol Buffer Files

- [`boardswarm.proto`](proto/boardswarm.proto) - Main protocol definition

## Usage

This crate is typically used indirectly through:
- [boardswarm-client](../boardswarm-client) - High-level client library
- [boardswarm](../boardswarm) - Server implementation
- [boardswarm-cli](../boardswarm-cli) - Command-line client

For direct usage:

```rust
use boardswarm_protocol::{boardswarm_client::BoardswarmClient, ItemType};
```

## Building

The protocol definitions are automatically compiled to Rust code during the build process using `tonic-prost-build`.
