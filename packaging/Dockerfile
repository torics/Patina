# The release image: the static Linux binary on Alpine, with git, which projects need.
# Built by the release workflow from the musl binaries it made; see docs/install.md.
FROM alpine:3.20
RUN apk add --no-cache git ca-certificates tini \
    && adduser -D -h /home/patina patina
ARG TARGETARCH
COPY --chmod=0755 bin/${TARGETARCH}/patina /usr/local/bin/patina
USER patina
WORKDIR /home/patina
# The daemon binds to loopback only, as everywhere: run the container with --network host so that loopback is
# the host's, and reach it through a tunnel such as tailscale serve.
ENTRYPOINT ["/sbin/tini", "--", "/usr/local/bin/patina"]
CMD ["serve"]
