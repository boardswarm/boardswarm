# Boardswarm

Distributed service to interact with development boards.

* [boardswarm](boardswarm/README.md) - The boardswarm server
* [boardswarm-protocol](boardswarm-protocol/README.md) - Boardswarm protocol definitions
* [boardswarm-cli](boardswarm-cli/README.md) - Boardswarm command line client
* [boardswarm-client](boardswarm-client/README.md) - Boardswarm client library


## Usage (Docker container)

A Docker container containing all of the boardswarm binaries is available from the
[GitHub Container Registry](https://github.com/boardswarm/boardswarm/pkgs/container/boardswarm)
but can alternatively be built from the root of a git checkout using:

```
docker build --file boardswarm/Dockerfile --tag boardswarm .
docker build --file boardswarm-cli/Dockerfile --tag boardswarm-cli .
```


Run `boardswarm-cli` inside the container:

```
docker run -it --rm \
  ghcr.io/boardswarm/boardswarm-cli:main \
  --help
```


Run `boardswarm` inside the container:

```
docker run -it --rm \
  -p 6683:6683 \
  -v $(pwd)/boardswarm/share/server.conf:/etc/boardswarm/server.conf \
  ghcr.io/boardswarm/boardswarm:main
```

## Manual Installation

These instructions are for configuring a simple unencrypted setup with static
JWKS based authentication.

### Building

Boardswarm needs rustc version 1.70 or newer, this rules out a number of stable
distributions for now (the kind of thing you may want to run your
infrastructure on). There are however a number of solutions

 - Rather than installing the distribution version of Rust, use
   [rustup](https://www.rust-lang.org/tools/install) to install the latest
   version directly from the rust project.

 - Build on a system that has the required version of rust installed and, as
   rust builds static binaries, we can copy them to the target server once
   built. (Note that rust will note the version of libc used during the build
   and complain if the expected version isn't found, making this solution
   unreliable at times.) For instance, assuming a Debian system:
   ```
   $ sudo apt install rustc
   ```

 - If installing software outside of the systems package management isn't for
   you and building elsewhere isn't an option, then creating a container from
   which to build boardswarm might be a solution. A simple Dockerfile to just
   build boardswarm is as follows:
   ```
   FROM rust:slim-bookworm
   ARG DEBIAN_FRONTEND=noninteractive

   RUN apt-get update && \
       apt-get install --yes \
         pkg-config \
         protobuf-compiler \
         libudev-dev \
         libssl-dev && \
       rm -rf /var/lib/apt/lists/*

   WORKDIR /usr/src/boardswarm

   RUN cargo build \
         --release
   ```
   Save this to the root of the git repos once we've checked it out below.

If not building from a container (where this'll be handled by the container
creation step), install required tools/libraries to build boardswarm. As we'll
be pulling the latest from git, we'll need that installed too. Again, assuming
a Debian based system:
```
$ sudo apt install rustc git pkg-config protobuf-compiler libudev-dev libssl-dev
```

Boardswarm is quite new and undergoing active development, so as previously
mentioned, we'll build the latest from git. Clone the boardswarm repository:
```
$ git clone https://github.com/boardswarm/boardswarm.git
```

We can now build boardswarm:
```
$ cd boardswarm
$ cargo build --release
```
Or, if wanting to use a container for this, run the following command:
```
podman build --volume=$(pwd):/usr/src/boardswarm .
```

### Server Install

The "server" is the machine that will have the development boards attached to
it and/or in some instances may act as a router to connect to other boardswarm
instances.

1. Copy the `boardswarm` binary to `/usr/local/bin/` on the server.

1. Create a boardswarm user and group for the boardswarm server to run as:
   ```
   $ sudo addgroup --system boardswarm
   $ sudo adduser --system --ingroup boardswarm --no-create-home --disabled-password boardswarm
   ```

1. Copy the [example systemd unit](boardswarm/share/boardswarm.service) to
   `/etc/systemd/system/boardswarm.service`.

1. Copy [example udev rules](boardswarm/share/99-boardswarm.rules) to
   `/etc/udev/rules.d/`.

1. Reload udev rules and ensure they are applied:
   ```
   $ sudo udevadm control --reload-rules
   $ sudo udevadm trigger
   ```

1. Copy example [`server.conf`](boardswarm/share/server.conf) to
   `/etc/boardswarm/server.conf` on target server machine and modify as
   required.

### Server Configuration

1. A mechanism for controlling power to the devices is generally required when
   working with boards remotely. Whilst in some instances this might be handled
   using gpio actuators, boardswarm has support for
   [pdudaemon](https://github.com/pdudaemon/pdudaemon). Pdudaemon provides
   support for a wide range of programmable power supplies and its use if
   encouraged.  If using it for power mangement install and configure it to
   switch the development boards on and off, using whichever mechanism you are
   using (ensure that the `server.conf` `pdu` provider is configured to match the
   configuration set in the pdudaemon configuration):
   ```
   $ sudo apt install pdudaemon
   ```

1. We are going to use Static JWKS based authentication, follow the process to
   generate an EdDSA key as documented in the
   [server README](boardswarm/README.md#static-jwks-based-authentication).

1. Copy generated `auth.jwks` to `/etc/boardswarm/`
   ```
   $ sudo cp auth.jwks /etc/boardswarm/
   ```

### Configuring Devices

For each development board you will need a section under `devices`. There is an
example one provided in the example server configuration which can be used as a
template. More details about this configuration in the [server README Devices
section](boardswarm/README.md#devices).

### Run the Server

Reload systemd configuration, start the boardswarm server and configure it to
run at boot:
```
$ sudo systemctl daemon-reload
$ sudo systectl enable boardswarm.service
$ sudo systemctl start boardswarm.service
```

### Client Installation

1. Copy the `boardswarm-cli` binary to `/usr/local/bin/` on the client
   development machine.

1. Create a user token (expiry 01-01-2025) using the previously generated
   `private.jwks` and then copy it to the client machine:
   ```
   $ rnbyc  -s '{"exp": 1735686000 }' -K private.jwks > token.jwt
   ```

1. On the client machine, initialise the connection to target server:
   ```
   $ boardswarm-cli -i <server_name> auth init --uri http://<server>:6683 --token-file token.jwt
   ```

### Client Usage

See the [client documentation](boardswarm-cli/README.md) for details on how to
use the client.
