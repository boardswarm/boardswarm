# Boardswarm server

Boardswarm is a gRPC server exposing APIs useful for interacting with
development boards. The swarm part of the names comes from the ability of a
server to connect to other boardswarm instances, sharing and distributing
functionality transparantly for the end-user.

For more info about boardswarm design see the [design document](docs/design.md)

# Running boardswarm

To run boardswarm a yaml configuration file needs to be passed as an argument.
As a starting point the documented [example configuration](share/server.conf)
can be used.

## Authentication

Boardswarm always validates authentication against JWT bearer tokens; The
tokens can either be validate against a OIDC server or alternatively a local
static jwks file.

### OIDC based authentiation

For OIDC based authentication both the uri to the oidc issuer and the client id
need to be configured. While boardswarm itself just uses the uri to get the
servers JWKS keys, both together with the description are exposed to client it
to retrieve  tokens via the oauth 2.0 device authorization grant.

```
server:
  authentication:
    - type: oidc
      # This description can be show to users for selecting which method to
      # use
      description: "Example OIDC"
      # uri to the oidc issuer
      uri: "https://issuer.example.com/"
      # The client identifier to be used by the client to be included in the
      # device authorization request
      client: "boardswarm"
...
```

On the client side to setup such a server run the following command and follow
the instructions to authenticate:
```
$ boardswarm-cli  --instance <instance name> configure --new  -u <instance url>
```

### Static JWKS based authentication

For static JWKS token a JWKS needs to be generated for boardswarm to validate
tokens against, while the client(s) needs JWT token(s) signed by the private
key. An easy way to generate these is to using the
[rnbyc](https://github.com/babelouest/rhonabwy#rnbyc-rhonabwy-command-line-tool)
tool.

To generate jwks set using an EdDSA key the following command can be used:
```
$ rnbyc -j -g  Ed25519  -x   -o private.jwks -p auth.jwks
```

The `private.jwks` should be kept private and used to sign client authentication
tokens. The auth.jwks file can be configured in boardswarm to validate tokens
against. as follows:

```
server:
  authentication:
    # Authentication against a static Json Web Key Set.
    - type: jwks
      # Path to jwks file to authenticate against
      path: auth.jwks
...
```

For user tokens currently only the standard expiry field gets validated; To
create a user token (expiry 01-01-2025) the following command can be used.

```
$ rnbyc  -s '{"exp": 1735686000 }' -K private.jwks -o token.jwt
```

On the client side to setup such a server run the following command and follow
the instructions to authenticate:
```
$ boardswarm-cli  --instance <instance name> configure --new -u <instance url> --token-file <path to token file>
```

## Providers

Providers provide the consoles, volumes and actuators in boardswarm. Each
provider is configured in the `providers` section of the configuration file
using the following generic setup:
```
providers:
  - name: myprovider # The name of the provider
    provider: provider # The provider to use
    # Optional device specific parameters
    parameters:
      <provider specific>
```

### Serial provider

The serial provider creates consoles from local serial ports. No provider specific
parameters are expected and it only makes sense to have one of this type.

The configuration parameters for a console provided by this provider is
currently only `rate` with a numeric value matching the consoles baud rate.

Example configuration:
```
provider:
  - name: serial
    provider: serial
```

### Device Firmware Update provider (dfu)

Support for (DFU 1.1)[dfu] class specification. DFU devices are autodetected
via udev and are exposed as volumes. No provider specific parameters are
expected and only one of this provier can exist.

Currently only write operations are support for DFU targets. Committing these
volumes will execute a usb detach followed by a reset.

[dfu]: https://usb.org/document-library/device-firmware-upgrade-11-new-version-31-aug-2004

Example configuration:
```
provider:
  - name: dfu
    provider: dfu
```

### Rock USB provider (rockusb)

Support for rockchip USB protocol. rockusb devices are autodetected
via udev and are exposed as volumes. No provider specific parameters are
expected and only one of this provider can exist.

While the rockusb device is in maskrom mode two targets exist (471, 472)
matching the sram and ddr uploads. These are writable only. The boardswarm
cli can upload rockchip loader .bin files to these targets via the rock command
(download-boot subcommand).

When in loader mode a target matching the flash id is available. This supports
read, write and seek. Note that rockchip loader firmware isn't always reliable
when reading especially at larger offsets.

Example configuration:
```
provider:
  - name: rockusb
    provider: rockusb
```

### pdudaemon provider

Support for [pdudaemon] exposing its pdus and ports as actuators. For pdudaemon
in the parameters the pdudaemon uri needs to be configured as well as each pdu
and its ports (due to pdudaemon not supporting introspection).

Actuators provided by pdudaemon take a `mode` parameter which can be `on` or
`off` (matching pdudaemon actions).

[pdudaemon]: https://github.com/pdudaemon/pdudaemon

Example configuration:
```
provider:
  - name: pdu
    provider: pdudaemon
    # Configuration parameters for this provider
    parameters:
      # Uri of the pdudaemon server
      uri: http://localhost:16421/
      pdus:
        # pdu name (pdu hostname in pdudaemon terminology)
        - name: pdu-0
          # Number of ports; Will expose an actuator for port 1 to N as per
          # pdudaemon convention for numbered ports
          ports: 4
        - name: pdu-1
          # List of port names, for pdus that use named rather then numbered
          # ports
          ports:
            - "left"
            - "right"
```

### gpio provider

Support for using Linux GPIO character devices to expose gpio lines as
actuators. The provider specific parameter should contain a match section to
match the gpiochip that should be used and a list of lines for that chip to be
exposed as actuators. Lines can be identified by either the line name or line
number (see the output of the `gpioinfo` command to determine the lines/names).

Actuators provided a `value` parameter which takes a boolean value to turn the
gpio line high or low.

Example configuration:
```
providers:
  - name: "rpi header gpio"
    provider: gpio
    parameters:
      # the match has a list of udev properties to match against in similar
      # syntax as the eventual expose item. The example below matches the
      # GPIO's on an RPI 4's extension header
      match:
        udev.OF_FULLNAME: "/soc/gpio@7e200000"
      # A list of lines to expose as actuators; Lines can either be defined by
      # either lines name or the line number
      lines:
        - line_name: "GPIO23"
          name: "maskrom"
        - line_number: 24
          name: "gpio24"
```

### Boardswarm client provider

This provider acts as a client to a remote boardswarm service and (re)exports
all its items locally. That means all remote items can be used as if they were
local. For authentication to a remote boardswarm only a static jwt token is
supported.

All items provided will have their `boardswarm.instance` property set to the
name providers instance name.

Example configuration:
```
providers:
  # The boardswarm provider connects to a remote boardswarm instance as a
  # client and exposes all remote items as local ones.
  - name: remote
    provider: boardswarm
    parameters:
        # The uri of the remote server
        uri: http://remote.example.net:6683
        # Path to the jwt token to use to connect to the remote server
        token: remote.token
```

## Devices

Devices are what tie everything together. Devices contain:
* consoles: A list of consoles connected to the device
* volumes: A list of volumes associated with the device
* modes: A list of modes the device can operate in

To link the device to other items matches are used using a `match` key in the
configuation which contains a dictionary of item properties to match. An item
on a remote instance will only be matches if the `boardswarm.instance` property
is explicitely configured, otherwise only local items willl be matched.

### Device consoles

The list of consoles linked to this device. Each console has a name, console
specific parameters and a match to match the console. By convention the first
console in the list should be treated as the default in clients.

The console(s) will be configured with their specific parameters once they're
matched.

Example configuration for a device with one console from the serial provider
configured for a baudrate of 1.5mbit/s:
```
devices:
  - name: device
    consoles:
      - name: main
        parameters:
          rate: 1500000
        match:
            udev.ID_SERIAL: "12345"
```

### Device volumes

The list of volumes linked to this device. Each volume has a name and a match
section.

Example configuration of a single volume associated with the device, based on
the USB port it's connected to:
```
devices:
  - name: device
    volumes:
      - name: volume
        match:
          udev.ID_PATH: "pci-0000:00:14.0-usb-0:12.3"
```

### Device modes

A mode refers to an operational mode of a device. By convention devices
normally have at least two modes "on" and "off". "on" should normally refer
to the what would be expected as the normal operation of a device, while "off"
should mean the device is fully powered "off". Other modes may be configured to
e.g. reflect the device to be powered on, but configured to load software over
USB.

For each mode a sequence of steps should be configured to switch the device
into that mode. After each step a `stabilisation` period can be configured to
wait before a next step is done (or the switch be flagged as done).

A mode can depend on the device being in a specific mode first. This can help
simplifying the sequence as the device can be assumed to be in a known state
(typically off), rather then having to define each sequence such it can be entered
from any mode.

```
devices:
  - name: device
    modes:
      # Name of the mode, on/off are by convention the expected "normal"
      # powered on and powered off states
      - name: on
        # Mode the device needs to be on to ensure the expected mode is reached
        depends: off
        # The sequence of actuator actions to take the establish this mode
        sequence:
          # Each sequence item should have a match to match the actuator to use
          # and actuator specific parameters to apply
          # As the configuration is a yaml document anchors can be used to
          # avoid duplication.
          - match: &pdu
              boardswarm.name: pdu.pdu-1.port-1
            parameters:
              mode: on
      - name: maskrom
        depends: off
        sequence:
          - match: &maskrom
              boardswarm.name: maskrom
              # Items from remote instances have an instance property set with
              # the remote instance name
              boardswarm.instance: remote
            parameters:
              value: true
            # stabilisation provides a time period to wait after apply actuator
            # action. E.g. to wait for power to stablelise before doing next
            # actions
            stabilisation: 500ms
          - match: *pdu
            parameters:
              mode: on
            stabilisation: 1s
          - match: *maskrom
            parameters:
              value: false
      - name: off
        sequence:
          - match: *maskrom
            parameters:
              value: false
          - match: *pdu
            parameters:
              mode: off
            stabilisation: 2s
```
