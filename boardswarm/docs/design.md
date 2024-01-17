# Boardswarm Concepts

Underneath boardswarm contains collection of items of various types (actuators,
consoles, consoles, devices). Items of each type are uniquely identified by a
64 bit identifier (which is not re-used).

Each item has a list of properties which are simple key/value strings. By
convention each key is of the form `<namespace>.<key>`. For example boardswarm
properties (name, instance) use the `boardswarm` namespace, udev information
uses the `udev` namespace etc.

## actuators

Actuators are items that physically control the board state or inputs. Examples of
actuators are on/off power controls (on/off), gpio's, programmable power supplies, etc.

Each actuator type takes an actuator specific configuration for changing mode.
Normally actuators parameter aren't directly controlled by end-users but driven
by device mode changes.

## consoles

Consoles are items to interact with the board, examples of these are
serial console, ipmi serial-over-lan, etc. These expose a simple input/output
stream which will typically be a text console, however this is not guaranteed.

Consoles get configuration by a set of type specific parameters. End-users
aren't expected to directly configure consoles by driven by their associated
device configuration

## volumes

Volumes are items to exchange data. Examples of volumes types are DFU, RockUSB,
USB-mass storage etc. Each volume has one or more targets identifying the
target of the data that gets exchanged. For example for DFU each target
identifies an interface on the device.

Each target can be a any combination of readable, writable and seekable.

## devices

Devices are what tie all the above together. Each device links to one or more
consoles and volumes, which are all in principle dynamic.
Each device has a list of modes as well which can be switched between. The basic,
by convention, modes are "on" and "off". Other modes are typically defined by
non-standard boot-modes, for example turning the device on in an USB flashing
mode as opposed to doing a normal boot. Modes are activated by a sequence of
actuator actions, which in the simplest case is just turning power on and off
but can be arbitrarily complex.

## Providers

Providers are what backs actuators, consoles and volumes. Currently these
include:
* serial: serial consoles
* dfu: device firmware upgrade based volumes
* rockusb: Rock Usb flash protocol volumes
* pdudaemon: pdudaemon based actuators
* gpio: Gpio line based actuators
* boardswarm: client for other boardswarm instance, re-exposing all items

