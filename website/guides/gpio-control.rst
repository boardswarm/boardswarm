============
GPIO control
============

Controlling specific GPIOS directly
===================================

GPIOs can be hooked up to ancillary signals which can be used to put the board
into alternative states, such as a download mode. In instances like this it
would be best to define a mode to perform this operation, however GPIOs can
also be used for ephemeral changes, such as toggling reset lines. In these
instances it may be preferable to keep the board in it's current mode and
toggle the line separately. GPIOs fall under the category of "actuators", the
available actuators can be listed with the following command::

    $ boardswarm-cli list actuators

This list will include all the actuators available, which will include any
switchable PSU ports, etc. Take care to ensure you are not switching an
actuator assigned to another device.

The ``actuators`` command provides the ``change-mode`` sub-command. This takes
an "Actuator specific mode in json format". GPIOs expect a field named
``value``, with a boolean value. So to enable a GPIO::

    $ boardswarm-cli actuator <actuator name or id> change-mode '{"value": true}'



