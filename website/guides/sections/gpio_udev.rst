Ensure that Boardswarm has sufficient permissions to access GPIOs by setting up
udev rules for it. This can be achieved by adding the following udev rule to
the host running the boardswarm server. For example, by adding the file
``/etc/udev/rules.d/99-boardswarm.rules`` with the following contents::

    # Give boardswarm access to GPIO devices:
    ACTION=="add", SUBSYSTEM=="gpio", NAME="gpiochip%n", GROUP="boardswarm"

Ensure that the udev rule is in effect by rebooting or running::

    # udevadm control --reload && udevadm trigger


