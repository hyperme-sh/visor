# Private desktop bots

Each bot has its own X11 desktop, audio server, daemon, companion, MCP configuration,
identity and persistent home. No host display, input device, Docker socket or network
is exposed. This example gives one bot write access and another read access to a
bounded local exchange volume.

```sh
export VISOR_IMAGE=ghcr.io/hyperme-sh/hypermesh-visor:test
docker compose -p visor -f container/compose.yaml pull
docker compose -p visor -f container/compose.yaml up -d

docker compose -p visor -f container/compose.yaml exec -T writer \
  visor-resource put share example.txt --expected missing <<'TEXT'
Hello from the writer.
TEXT
docker compose -p visor -f container/compose.yaml exec -T reader \
  visor-resource get share example.txt

docker compose -p visor -f container/compose.yaml down
```

The TEST channel follows successful main builds. Pull it again before recreating
a bot to pick up the current build; running containers are not replaced by a
registry tag update alone. Package visibility and registry authentication follow
the repository package settings. For local development, build
`docker build -f container/Dockerfile -t visor-container:local .` and set
`VISOR_IMAGE=visor-container:local`. The daemon's control socket is private to its
container; run clients using `docker compose exec -T`. The inverse MCP executable
is `/usr/local/bin/hypermesh-desktop-mcp`. The bundled Go CLI is
`/usr/local/bin/hypermesh`; it uses the container's private runtime directory and
control socket by default. For example, `docker compose -p visor -f
container/compose.yaml exec -T writer hypermesh --help` lists its commands.
Run a bot's CLI in that bot's container, never by sharing its socket or home.
The CLI is built from the vendored source recorded in
`/usr/share/doc/hypermesh-cli/VENDOR_REV`, with a read-only module lock and no CGO.
Native image builds run its unit/contract tests and executable help check.

Homes survive container recreation. The shared exchange is **ephemeral**, limited
to 256 MiB, and may disappear when unmounted. For durable sharing, provision a
separate quota-enforced external volume. Never share a bot's home, credential
store or control socket. Do not use `down -v` unless intentionally deleting state.

`visor-resource` accepts flat filenames, refuses symlinks and hard links, and
publishes atomically with `--expected missing` or the previous SHA-256 for conflict
detection. Files are limited to 16 MiB; cooperative publishers share a lock and a
240 MiB budget. The mount's 256 MiB quota remains the enforcement boundary even
when a writer bypasses the helper. Read-only grants are enforced by the mount.
An authorized writer can replace or delete shared files. Share only with trusted
writers. Interrupted temporary publications count against quota; inspect and
remove them explicitly rather than deleting unrelated resources.

Networking is off by default. Enabling network access is an operator decision;
apply an egress policy appropriate to the apps and MCP services in use. Local
HTTP/SSE fixtures work over each container's own loopback interface.
The bundled GitHub issue plugin can start in this profile. The worker itself is
confined to its standard I/O pipes.

## Opt in to live GitHub issue reads

On a Linux Docker host, run the GitHub API relay in its own terminal with
Python 3.11 or newer. Use a private directory and a new socket path.

```sh
install -d -m 0700 "$HOME/.local/run/visor"
python3 container/github-issue-relay.py "$HOME/.local/run/visor/github.sock"
```

The relay opens TCP connections only to `api.github.com:443`. The socket is
owner-only. Give its ownership to the host UID that maps to the writer's
container UID `1000` if they differ. Keep the relay running and start the
writer with the explicit overlay in a second terminal.

```sh
export VISOR_IMAGE=ghcr.io/hyperme-sh/hypermesh-visor:test
export VISOR_GITHUB_ISSUE_RELAY_SOCKET="$HOME/.local/run/visor/github.sock"
docker compose -p visor -f container/compose.yaml -f container/compose.github-issues.yaml up -d writer
```

Both containers retain `network_mode: none`. The overlay mounts the socket
read-only for the selected writer. Visor's broker verifies GitHub TLS through
the relay and applies its usual per-call session checks. Treat processes with
the writer's UID as trusted local callers because they can reach that socket.
Keep the socket out of shared directories. Recreate the writer if the relay
is restarted so the bind mount refers to the new socket. Use the
[plugin guide](../plugins/github-issue/README.md) to supply the session token and
grant repository access. Code Mode still requires `--code-mode` on the grant.

## Native CI builds

PR and main CI build the image on native amd64 and arm64 runners. Each
`visor-container-<architecture>` artifact contains a compressed Docker image
archive and `manifest.json` recording its SHA-256, byte size, source revision,
platform and local image ID. CI checks the unprivileged user, entrypoint,
readiness command and absence of implicit ports or volumes before upload.
Archives expire after one day. Main CI additionally publishes the verified native
images to GHCR and combines their registry digests into the moving `:test` channel.
PR jobs cannot publish. Each native job starts one disposable container with
the standard Compose profile and checks bundled plugin discovery, opt-in,
and execution. The issue-read fixture runs on that container's loopback.

For an on-demand live check, give the `Live GitHub issue contract`
workflow on `main` the published TEST index digest and the token owner's
numeric GitHub user ID. The repository Actions secret
`VISOR_LIVE_GITHUB_ISSUE_TOKEN` supplies a fine-grained token scoped to this
repository with Issues read access. The native jobs read the public Visor issue
[#353](https://github.com/hyperme-sh/visor/issues/353) directly and through
explicitly opted-in Code Mode, using the same overlay on both architectures.

Verify the downloaded archive against the manifest before loading it. After
`docker image load`, compare the loaded image ID and platform with the manifest;
record that image ID when inspecting the archive. A local image ID identifies
image configuration and layers; it is not a registry manifest digest. Main CI
records both architecture manifest digests, the multi-architecture index digest,
source revision, image IDs and archive hashes in `container-test-channel.json`
(retained for 90 days). Source-specific `test-<revision>` tags aid lookup; the
recorded digest identifies exact downloadable bytes. TEST deployments use the
moving channel rather than requiring manual source-pin updates. Publication does
not create Git version tags, stable releases or change package visibility.

Container assignment remains local to the operator: each private home identifies
one bot, and access to `docker compose exec` grants that bot's local control.
There is no physical host-input pause in the private virtual desktop. Its own
companion pause/stop controls and session audit remain active. Shared-file grants
authorize only the mounted directory; they do not grant a remote machine
assignment. Cross-service enrollment, remote assignment and revocation remain
in #30; this image does not expose a local socket or token to the public site.

## Scheduled coverage

The central nightly batch owns the two-bot, shared-resource, restart, MCP and app
scenarios. Do not run those suites manually on hosts or shared resources. PRs
validate unit contracts, Compose configuration and native image builds.

See [the base decision](../docs/container-base.md). Host physical input and native
Wayland automation are separate from this private X11 container environment.
