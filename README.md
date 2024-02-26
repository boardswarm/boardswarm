# Boardswarm

Distributed service to interact with development boards.

* [boardswarm](boardswarm/README.md) - The boardswarm server
* [boardswarm-protocol](boardswarm-protocol/README.md) - Boardswarm protocol definitions
* [boardswarm-cli](boardswarm-cli/README.md) - Boardswarm command line client
* [boardswarm-client](boardswarm-client/README.md) - Boardswarm client library


## Usage (Docker container)

A Docker container containing all of the boardswarm binaries is available from the
[GitHub Container Registry](https://github.com/boardswarm/boardswarm/pkgs/container/boardswarm)
but can alternatively be built from the root of a git checkout using:

```
docker build --file boardswarm/Dockerfile --tag boardswarm .
docker build --file boardswarm-cli/Dockerfile --tag boardswarm-cli .
```


Run `boardswarm-cli` inside the container:

```
docker run -it --rm \
  ghcr.io/boardswarm/boardswarm-cli:main \
  --help
```


Run `boardswarm` inside the container:

```
docker run -it --rm \
  -p 6683:6683 \
  -v $(pwd)/boardswarm/share/server.conf:/etc/boardswarm/server.conf \
  ghcr.io/boardswarm/boardswarm:main
```
