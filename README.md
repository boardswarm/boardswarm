# Boardswarm

Distributed service to interact with development boards.

* [boardswarm](boardswarm/README.md) - The boardswarm server
* [boardswarm-protocol](boardswarm-protocol/README.md) - Boardswarm protocol definitions
* [boardswarm-cli](boardswarm-cli/README.md) - Boardswarm command line client
* [boardswarm-client](boardswarm-client/README.md) - Boardswarm client library

## Installation

These instructions are for configuring a simple unencrypted setup with static
JWKS based authentication.

### Building

Boardswarm needs rustc version 1.70 or newer, this rules out a number of stable
distributions for now (the kind of thing you may want to run your
infrastructure on), however if needed we can build on a system that provides
the required level of rust support and, as rust builds static binaries, we can
copy them to the target server once built. As boardswarm is also quite new and
undergoing active development, we'll build the latest from git.

1. Install required tools/libraries to build boardswarm on a suitable system.
   As we're pulling the latest from git, we'll need that installed too.
   Assuming a Debian based system:
   ```
   $ sudo apt install rustc git protobuf-compiler libudev-dev
   ```
1. Clone the boardswarm repository:
   ```
   $ git clone https://github.com/boardswarm/boardswarm.git
   ```
1. We can now build boardswarm:
   ```
   $ cd boardswarm
   $ cargo build
   ```

### Server Install

The "server" is the machine that will have the development boards attached to
it.

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

1. Install [pdudaemon](https://github.com/pdudaemon/pdudaemon) and configure it
   to switch the development boards on and off, using whichever mechanism you
   are using (ensure that the `server.conf` `pdu` provider is configured to match
   the configuration set in the pdudaemon configuration):
   ```
   $ sudo apt install pdudaemon
   ```

1. We are going to use Static JWKS based authentication, as documented in the
   [server README](boardswarm/README.md#static-jwks-based-authentication).
   Install `rnbyc` so we can create the required tokens:
   ```
   $ sudo apt install rnbyc
   ```

1. Generate an EdDSA key:
   ```
   $ rnbyc -j -g  Ed25519  -x   -o private.jwks -p auth.jwks
   ```
   Ensure you keep the private key private and safe, it will be needed during
   client configuration.

1. Copy generated `auth.jwks` to `/etc/boardswarm/`
   ```
   $ sudo cp auth.jwks /etc/boardswarm/
   ```

For each development board you will need a section under `devices`. There is an
example one provided in the example server configuration which can be used as a
template.

#### USB Serial Consoles

1. Determine the USB serial port used as the console on the development board.

1. Use the following commmand to determine the required `udev.ID_SERIAL` and
   `udev.ID_USB_INTERFACE_NUM`:
   ```
   $ udevadm info <tty_device> | grep -i 'ID_SERIAL=\|ID_USB_INTERFACE_NUM='
   ```

#### DFU Booting

1. Power on the development board in DFU mode and run:
   ```
   $ sudo dfu-util -l
   dfu-util 0.11
   
   Copyright 2005-2009 Weston Schmidt, Harald Welte and OpenMoko Inc.
   Copyright 2010-2021 Tormod Volden and Stefan Schmidt
   This program is Free Software and has ABSOLUTELY NO WARRANTY
   Please report bugs to http://sourceforge.net/p/dfu-util/tickets/
   
   Found DFU: [0451:6165] ver=0200, devnum=45, cfg=1, intf=0, path="3-3.4", alt=1, name="SocId", serial="01.00.00.00"
   Found DFU: [0451:6165] ver=0200, devnum=45, cfg=1, intf=0, path="3-3.4", alt=0, name="bootloader", serial="01.00.00.00"
   ```

1. Ensure that you have udev rules configuring the group ownership of usb
   devices with the given USB vendor and product IDs (`0451:6165` in the above
   example) to the boardswarm group.

1. Use the following command to determine the required `ID_PATH`
   ```
   $ udevadm info /sys/bus/usb/devices/<path> | grep 'ID_PATH='
   ```
   Where `<path>` is the path found by `dfu-util` (`3-3.4` in the above
   example).

### Run the Server

Reload systemd configi, start the boardswarm server and configure it to run at
boot:
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

1. Initialise the connection to target server:
   ```
   $ boardswarm-cli -i <server_name> auth init --uri http://<server>:6683 --token-file token.jwt
   ```

### Client Usage

 * The following command should list the new server:
   ```
   $ boardswarm-cli auth list
   ```

 * The devices configured on the server(s) can be listed:
   ```
   $ boardswarm-cli list devices
   ```

 * A board can be powered on and off:
   ```
   $ boardswarm-cli device <device_name> mode on
   $ boardswarm-cli device <device_name> mode off
   ```

 * A console can be opened to a device:
   ```
   $ boardswarm-cli ui <device_name>
   ```
   Board state can be controlled from within this console:
   * `ctrl-a`, `o`: Power device on
   * `ctrl-a`, `f`: Power device off
   * `ctrl-a`, `r`: Power-cycle the device

   Other commands:
   * `ctrl-a`, `q`: Quit the console
