Run the following command to monitor new consoles added to the boardswarm
server::

   $ boardswarm-cli monitor consoles -v

Boardswarm will detect the new addition of new USB serial device and print out
the devices ID, name and a list of properties that can be used to identify the
new serial console in the boardswarm server config file.

If the device uses a legitimate FDTI chip to provide the USB serial, the serial
device has a unique serial number. As a result the ``udev.ID_SERIAL`` entry is
considered the best option, as this uniquely identifies the device, even if the
device is moved to a different USB port (unlike using ``udev.ID_PATH`` which
specifies the device plugged into a specific USB port). However if a unique
serial number is not present then the path is generally the next best option.

Once a suitable property has been identified done, we can kill the boardswarm
monitor with ``ctrl-c``.

