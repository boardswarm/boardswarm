===================
Basic Functionality
===================

Listing available devices
=========================

The boards available on the boardswarm server can be listed with the following
command::

   $ boardswarm-cli list devices

This command provides a list of the board IDs and names.


Terminal style access to the board
==================================

The console UI is the main way to interact with development boards hosted on a
Boardswarm server. This UI can be launched with the following command::

   $ boardswarm-cli ui <device name or id>

Where the device name or ID is one from the previous command.

The UI has 2 modes, console and command. The console mode allows the user to
interact with the serial console of the remote board. In the command mode the
user can perform boardswarm specific operations. When launching the Boardswarm
UI it is initially in the console mode. The command mode can be entered by
sending ``<ctrl>-a`` (abbreviated below as ``^a``). This is typically followed by
one of the below commands. The connection will stay in command mode until ``ESC``
is sent.

the follow commands are available:

=== ====================================================================
Key Command mode binding
=== ====================================================================
q   Quit the ui
o   Change the device to mode "on"
f   Change the device to mode "off"
r   Reset the device's power (same as changing mode to "off" then "on")
k   Scroll up
j   Scroll down
0   Reset scrolling state
ESC Leave command mode
=== ====================================================================


Changing power state outside of the console UI
==============================================

The power state of a board can be changed from the commandline via the
``device`` ``mode`` subcommand::

    $ boardswarm-cli device <device name or id> mode <mode>

It is typical for a devices config to provide ``on`` and ``off`` modes, but
this is convention rather than required. The following command can be used to
list basic information about a boards configuration, including the available
modes (in JSON format)::

    $ boardswarm-cli device <device name or id> info

