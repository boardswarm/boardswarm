
==========
Boardswarm
==========

Boardswarm provides a distributed service to interact with development boards.
It is able to interact with the boards via their serial consoles, manage power,
auxillary control lines and using a growing number of protocols, such as:

- Device Firmware Upgrade (DFU)
- Fastboot
- MediaTek Boot ROM protocol
- Rockchip USB protocol

The connection between the boardswarm server and client application can be
secured using multiple authentication mechanisms.


Where can I get Boardswarm?
===========================

Boardswarm is a relatively new project and under active development. The latest releases, built by the projects CI build process can be found here:

  https://github.com/boardswarm/boardswarm/releases/tag/latest

Full source code can be found in our github repository:

  https://github.com/boardswarm/boardswarm


.. toctree::
   :maxdepth: 2
   :caption: User Guide

   guides/setup-server.rst
   guides/setup-client.rst
   guides/basic-functionality.rst

.. toctree::
   :maxdepth: 1
   :caption: Worked board examples

   boards/mediatek.rst
