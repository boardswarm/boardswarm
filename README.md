# Boardswarm

Distributed service to interact with development boards.

* [boardswarm](boardswarm/README.md) - The boardswarm server
* [boardswarm-protocol](boardswarm-protocol/README.md) - Boardswarm protocol definitions
* [boardswarm-cli](boardswarm-cli/README.md) - Boardswarm command line client
* [boardswarm-client](boardswarm-client/README.md) - Boardswarm client library


## Usage (Docker container)

A Docker container containing all of the boardswarm binaries is available from the
[GitHub Container Registry](https://github.com/boardswarm/boardswarm/pkgs/container/boardswarm)
but can alternatively be built from a git checkout using:

```
docker build --build-arg BINARY=boardswarm --tag ghcr.io/boardswarm/boardswarm:main .
docker build --build-arg BINARY=boardswarm-cli --tag ghcr.io/boardswarm/boardswarm-cli:main .
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
  --port 16421:16421 \
  --mount "type=bind,source=$(pwd)/boardswarm/share/server.conf,destination=/etc/boardswarm/server.conf" \
  --entrypoint boardswarm \
  ghcr.io/boardswarm/boardswarm:main \
  /etc/boardswarm/server.conf
```
