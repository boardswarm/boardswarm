# Boardswarm command line client

A command line tool and ui for boardswarm. See the tool's `help` output for
all options.

## Setting up new accounts

The `configure` subcommand can be used to configure remote instances. To setup
an instance while authenticating against an OIDC server one can simply run:
```
$ boardswarm-cli  --instance <instance name> configure --new  -u <instance url>
```

To authenticate with a static JWT token it can be passed on the command line as
well:
```
$ boardswarm-cli  --instance <instance name> configure --new -u <instance url> --token-file <path to token file>
```

For more information see `boardswarm-cli configure --help`

## Boardswarm UI

The ui subcommand launches a tui:
```
$ boardswarm-cli ui <device name or id>
```

The UI has the follow keyboard shortcuts:
* ^a q: Quit the ui
* ^a o: Change the device to mode "on"
* ^a f: Change the device to mode "off"
* ^a k: Scroll up
* ^a j: Scroll down
* ^a 0: Reset scrolling state
