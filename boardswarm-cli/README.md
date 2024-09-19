# Boardswarm command line client

A command line tool and ui for boardswarm. See the tool's `help` output for
all options.

## Setting up new accounts

The `auth` subcommand can be used to configure remote instances. To setup
an instance while authenticating against an OIDC server one can simply run:
```
$ boardswarm-cli  --instance <instance name> auth init  -u <instance url>
```

To authenticate with a static JWT token it can be passed on the command line as
well:
```
$ boardswarm-cli  --instance <instance name> auth init -u <instance url> --token-file <path to token file>
```

Once initialised, configured servers can be listed by running:
```
$ boardswarm-cli auth list
```

The devices configured on the server(s) can be listed with the following
command:
```
$ boardswarm-cli list devices
```

For more information see `boardswarm-cli auth --help`

## Boardswarm UI

The ui subcommand launches a tui:
```
$ boardswarm-cli ui <device name or id>
```

The UI has the follow keyboard shortcuts:
* ^a q: Quit the ui
* ^a o: Change the device to mode "on"
* ^a f: Change the device to mode "off"
* ^a r: Reset the device's power (same as changing mode to "off" then "on")
* ^a k: Scroll up
* ^a j: Scroll down
* ^a 0: Reset scrolling state

## Boardswarm device control

Basic board control can be performed without opening the tui, for example
a board can be powered on and off:
```
$ boardswarm-cli device <device name> mode on
$ boardswarm-cli device <device name> mode off
```
