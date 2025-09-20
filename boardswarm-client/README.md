# Boardswarm client library

A Rust client library for connecting to boardswarm servers.

## Overview

This library provides a high-level async interface for interacting with boardswarm servers, including:

- Device discovery and management
- Console connections
- Volume operations (read/write/erase)
- Authentication (OAuth2/OIDC and static JWT)
- Device mode switching and actuator control

## Usage

Add this to your `Cargo.toml`:

```toml
[dependencies]
boardswarm-client = "0.0.1"
```

Basic example:

```rust
use boardswarm_client::{BoardswarmBuilder, ItemType};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut client = BoardswarmBuilder::new("http://localhost:6683".parse()?)
        .build().await?;
    
    // List available devices
    let devices = client.list(ItemType::Device).await?;
    println!("Found {} devices", devices.len());
    
    Ok(())
}
```

For more detailed examples, see the [boardswarm-cli](../boardswarm-cli) implementation.
