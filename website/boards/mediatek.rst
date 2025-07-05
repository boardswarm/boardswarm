========
MediaTek
========

Genio 700 EVK
=============

This is a quick guide covering options available for configuring the MediaTek
Genio 700 EVK. The official guide to connecting the board to a host can be
found here:

https://mediatek.gitlab.io/aiot/doc/aiot-dev-guide/master/sw/yocto/get-started/connect.html


Setting up serial
-----------------

.. note::
  .. collapse:: Ensure server is configured to allow boardswarm to access USB serial devices

    .. include:: ../guides/sections/serial_udev.rst



.. tip::
  .. include:: ../guides/sections/serial_identification.rst

Connect a microUSB cable to ``UART0`` on the EVK and the other end to the
boardswarm server. 

Create a node under ``devices`` for the board and add a console node, also
specifying the baud rate, which is 921600bps::

   devices:
     - name: genio-700-1
       consoles:
         - name: main
           parameters:
             rate: 921600
           match:
             udev.ID_USB_SERIAL: "FTDI_FT232R_UB_UART_<serial>"


Setting up power
----------------

Board is powered with 12V via ``DC IN`` barrel jack. In order for the device to
boot when power is applied, either a USB cable needs to be powered and
connected to the ``Micro USB D/L`` port, or the power GPIO needs to be toggled
on and off (see GPIO setup below for more information).

For now we will assume that a cable has plugged into the ``Micro USB D/L`` port
and we can power the board on and off by just applying power. Ensure that your
PDU provider or other power control solution is configured and you know which
port it's connected to. Power switching is managed by setting "modes" for the
relevant device in the ``devices`` section.  The modes can be arbitraily named,
however the UI provides commands for enabling and disabling power and thus
expects modes named ``on`` and ``off``. Add these to a ``modes`` section under
the board node::

    devices:
      - name: genio-700-1
        consoles:
          ...
        modes:
          - name: on
            depends: off
            sequence:
              - match: &pdu-genio-700-1
                  boardswarm.name: pdu.<pdu>.port-<index>
                parameters:
                  mode: on
          - name: off
            sequence:
              - match: *pdu-genio-700-1
                parameters:
                  mode: off
                stabilisation: 2s

Restart boardswarm for the configuration changes to take effect. You should now
be able to switch the board on and off with boardswarm from the command line::

    $ boardswarm device genio-700-1 mode off
    $ boardswarm device genio-700-1 mode on

If the board already has something installed to boot and you have setup the
serial as detailed above, you should also be able to
:ref:`access the serial console <guides/basic-functionality:terminal style access to the board>`.


Setting up GPIO
---------------

The FTDI USB serial chip used to provide access to the console on the Genio 700
EVK also provides a number of GPIO pins that the board uses to enable USB
control of the buttons found on the board. The details of how these GPIO pins
are used is
`documented in the MediaTek guide <https://mediatek.gitlab.io/aiot/doc/aiot-dev-guide/master/hw/g700-evk.html#ftdi-board-control>`_.
The GPIO lines are exposed in Linux along with any other supported GPIO
controller.

.. note::
  .. collapse:: Ensure the server is configured to allow boardswarm to access GPIOs

    .. include:: ../guides/sections/serial_udev.rst


To enable them in Boardswarm we need to add a gpio node in the providers
section of the config. The same match used for the console should be used in
this section too::

    providers:
      - name: "genio-700-1 ftdi"
        provider: gpio
        parameters:
          match: &genio-700-1-dongle
            udev.ID_USB_SERIAL: "FTDI_FT232R_UB_UART_<serial>"
          lines:
            - line_number: 0
              name: "genio-700-1-power"
            - line_number: 1
              name: "genio-700-1-reset"
            - line_number: 2
              name: "genio-700-1-download"

Note that this section is also used to select the specific GPIO pins that will
be controlled by Boardswarm and the names that can be used to identify them.

These GPIO can then be used to perform operations, typically as sequences
performed on mode changes. For example, if it is not desired to have a USB
cable connected to the ``Micro USB D/L`` port of the EVK (however it should be
noted this will be necessary for some of the following more complex operations
which can be performed with the board), pressing of the power button can be
simulated via the relevant pin as part of the ``on`` mode::

    devices:
      - name: genio-700-1
        consoles:
          ...
        modes:
          - name: on
            depends: off
            sequence:
              # Apply power
              - match: &pdu-genio-700-1
                  boardswarm.name: pdu.<pdu>.port-<index>
                parameters:
                  mode: on
              # Toggle power button on and off
              - match: &gpio-genio-700-1-power
                  boardswarm.name: "genio-700-1-power"
                parameters:
                  value: true
                stabilisation: 500ms
              - match: *gpio-genio-700-1-power
                parameters:
                  value: false


Setting up mediatek-brom
------------------------

With the GPIO enabled we can cause the board to boot into download mode. To use
this mode we need a USB cable connecting the ``Micro USB D/L`` port to the
boardswarm server.

To enter download mode we add another mode to the device node, which can be
called instead of the "on" mode to put the board into download mode::

    devices:
      - name: genio-700-1
        consoles:
          ...
        modes:
          ...
          - name: download
            depends: off
            sequence:
              # Hold down "download" button
              - match: &gpio-genio-700-1-download
                  boardswarm.name: "genio-700-1-download"
                parameters:
                  value: true
                stabilisation: 100ms
              # Power on the board and stabilise
              - match: *gpio-genio-700-1-power
                parameters:
                  mode: on
                stabilisation: 2s
              # Stop holding down the "download" button
              - match: *gpio-genio-700-1-download
                parameters:
                  value: false

Add a ``mediatek-brom`` node to the providers section to enable support for the
MediaTek protocol used in this mode::

    providers:
      - name: mediatek-brom
        provider: mediatek-brom

Restart boardswarm to update the configuration.

Ensure that Boardswarm has sufficient permissions to access the interface
that's created in this mode by setting up a udev rule for it::

    # MediaTek BROM
    SUBSYSTEM=="usb", ATTR{idVendor}=="0e8d", ATTR{idProduct}=="0003", MODE="0660", TAG+="uaccess"

Ensure that the udev rule is in effect by rebooting or running::

    # udevadm control --reload && udevadm trigger

Use the monitor to watch for volumes and print out details when a volume is seen::

   $ boardswarm-cli monitor volumes -v

From another terminal, switch the board into download mode::

   $ boardswarm-cli device genio-700-1 mode download

The ``mediatek-brom`` will detect the new connection on the ``Micro USB D/L``
port and print out it's properties. Use this to create a volume under the board
node. The serial provided by this interface is not unique, so here we will need
to add a match using the ``ID_PATH`` property::

    devices:
      - name: genio-700-1
        consoles:
          ...
        modes:
          ...
        volumes:
          - name: genio-700-1-brom
            match:
              udev.ID_PATH: "<id_path>"

Restart boardswarm to update the configuration.

Setting up fastboot
-------------------

Genio devices can use the ``mediatek-brom`` mode to upload a
`littlekernel <https://github.com/littlekernel>`_ based binary to bootstrap
into a fastboot mode. To be able to use fastboot we need to enable and
configure the boardswarm fastboot provider::

    providers:
      ...
      - name: genio-fastboot
        provider: fastboot
        parameters:
          match:
            udev.ID_VENDOR_ID: 0e8d
            udev.ID_MODEL_ID: 201c
          targets:
            - mmc0
            - mmc0boot0
            - mmc0boot1
            - root

Restart boardswarm to update the configuration.

We also need to add some udev rules to allow the boardswarm server to have
access to the required device files::

    # MediaTek Fastboot
    ACTION=="add", SUBSYSTEM=="usb", ENV{ID_USB_INTERFACES}==":ff4203:", GROUP="boardswarm"
    SUBSYSTEM=="usb", ATTR{idVendor}=="0e8d", ATTR{idProduct}=="201c", MODE="0660", TAG+="uaccess"

Ensure that the udev rule is in effect by rebooting or running::

    # udevadm control --reload && udevadm trigger

With this and the ``mediatek-brom`` configured we can now use that to upload
the littlekernel binary used to bootstrap into fastboot (this will typically be
provided along side OS images to aid with flashing devices)::

    $ boardswarm-cli device genio-700-1 mode off
    $ boardswarm-cli device genio-700-1 mode download
    $ boardswarm-cli device genio-700-1 write --commit genio-700-1-brom brom lk.bin

The monitor launched above will show a ``mediatek-brom`` volume being created
when powering on the board in download mode, which will be removed and replaced
with a volume with the ``"boardswarm.provider" => "fastboot"`` property once
bootstrapped into fastboot. As with ``mediatek-brom``, the serial number here
is not unique, so we will create a volume node for the device using the
``ID_PATH`` property listed for this volume::

    volumes:
      ...
      - name: genio-700-1-fastboot
        match:
          udev.ID_PATH: "<id_path>"

Restart boardswarm to update the configuration.


Reflashing board
----------------

With Fastboot setup, we can reflash the board with the following commands::

    $ boardswarm-cli device genio-700-1 write --commit genio-700-1-fastboot mmc0boot0 fip.img
    $ boardswarm-cli device genio-700-1 write-aimg --commit genio-700-1-fastboot mmc0 <image>

Reset the board to boot to the newly installed image. This can be achieved by
power cycling the board or by toggling the reset GPIO on and off::

    $ boardswarm-cli actuator genio-700-evk-reset change-mode '{"value": true}'
    $ boardswarm-cli actuator genio-700-evk-reset change-mode '{"value": false}'

