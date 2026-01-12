============
GPIO control
============

Controlling specific GPIOS directly
===================================

GPIOs can be hooked up to ancillary signals which can be used to put the board
into alternative states, such as a download mode. GPIOs fall under the category
of "actuators", the available actuators can be listed with the following
command::

    $ boardswarm-cli list actuators

This list will include all the actuators available, which will include any
switchable PSU ports, etc. Take care to ensure you are not switching an
actuator assigned to another device.

The ``actuators`` command provides the ``change-mode`` sub-command. This takes
an "Actuator specific mode in json format". GPIOs expect a field named
``value``, with a boolean value. So to enable a GPIO::

    $ boardswarm-cli actuator <actuator name or id> change-mode '{"value": true}'



