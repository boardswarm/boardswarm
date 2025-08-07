Ensure that Boardswarm has sufficient permissions to access USB serial devices
that are attached to the system by adding the following udev rule to the host
running the boardswarm server. (This can be achieve by adding the file
``/etc/udev/rules.d/99-boardswarm.rules`` with the following contents::

    # USB Serial devices
    ACTION=="add", SUBSYSTEM=="tty", ENV{ID_BUS}=="usb", GROUP="boardswarm"

Ensure that the udev rule is in effect by rebooting or running::

    # udevadm control --reload && udevadm trigger
