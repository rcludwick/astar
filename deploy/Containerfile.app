# Thin app layer over iaxnode-base (iax-4703). Build context must contain
# the linux/amd64 astar-server binary (deploy/out/ locally, /tmp/iaxnode-deploy on the VPS).
FROM localhost/iaxnode-base

COPY astar-server /usr/local/bin/astar-server
ENTRYPOINT ["/usr/local/bin/astar-server", "serve", "--config", "/etc/iaxnode/node.toml"]
