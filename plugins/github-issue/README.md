# GitHub issue pilot

The Linux Visor pilot reads one public GitHub issue into a coding task. The
package uses the public SDK and the signed distribution format as
`dev.visor.github-issue`. Inspect the installed publisher and key fingerprint
before granting access. A local author build has a community publisher.

## Visor TEST image

The moving `ghcr.io/hyperme-sh/hypermesh-visor:test` image includes a signed
GitHub issue package on Linux amd64 and arm64. In a fresh container,
`hypermesh-companion --plugins list` reports `dev.visor.github-issue` with
`bundled: true`. Use the listed revision to grant access with the commands
below. Each session still needs its own account and repository grant, and
Code Mode requires `--code-mode` on that grant.

The image build signs the package with a disposable Ed25519 key and ships
the signed file at a root-owned path. Visor verifies it before listing or
granting it. The image owns updates to the bundled package. An existing
owner-installed package with the same ID retains precedence.

Visor loads the image-owned worker from a sealed in-memory file. The worker
installs a syscall allowlist before reading input. The
worker can use its standard I/O pipes and memory. Visor handles provider
requests, which require explicit GitHub egress for live reads. CI exercises
both native images under the standard network-free Compose profile using a
loopback provider fixture and an empty user plugin store.
For live container reads, see the [opt-in GitHub relay](../../container/README.md#opt-in-to-live-github-issue-reads).

## Build and install a standalone copy

Build the package from this directory with Rust 1.87 or later and a C compiler.
Put the executable and manifest in a private package directory. The executable
prints the bundled license notices with `--third-party-notices`. Sign and
install it using the
[plugin distribution guide](../distribution.md).

```sh
umask 077
target="$(rustc -vV | sed -n 's/^host: //p')"
RUSTFLAGS='-C target-feature=+crt-static' cargo build --release --locked --target "$target"
package="$(mktemp -d)"
install -m 600 "target/$target/release/visor-github-issue" "$package/plugin"
executable_sha256="$(sha256sum "$package/plugin" | cut -d' ' -f1)"
jq --arg digest "$executable_sha256" '.executable_sha256 = $digest' \
  manifest.template.json > "$package/manifest.json"
```

## Read an issue

Use a live Visor session with current local consent. Set
`github-issues-token` through the existing session vault secret input when
opening the session. For a local secret, its request fields are
`name: "github-issues-token"`, `source: "local"`, and `value` containing
the credential. Use a fine-grained GitHub personal access token scoped to
the chosen account and repository, with Issues read access. The token stays
in the session vault and is reserved for this broker. Close the session to
drop it. For the GitHub API and permission details, see
[Get an issue](https://docs.github.com/en/rest/issues/issues#get-an-issue).

Copy the package revision from the plugin list. Supply the numeric GitHub
user ID and the repository's canonical `OWNER/REPO` spelling. A grant applies
to that account and repository until revocation, session close, a package
update, or Visor restart.

```sh
hypermesh-visor github-issue enable SESSION dev.visor.github-issue REVISION ACCOUNT OWNER/REPO
hypermesh-visor github-issue status SESSION dev.visor.github-issue
hypermesh-visor github-issue read SESSION dev.visor.github-issue ACCOUNT OWNER/REPO 42
hypermesh-visor github-issue revoke SESSION dev.visor.github-issue
```

Status reports the installed version, publisher, and effective local access.
Every read validates the token's account and confirms the repository is public.
Visor fetches issue content with public access and verifies the issue number
and canonical URLs. The sandboxed plugin receives the issue ID, number, URL,
state, title, and optional body. Title and body are bounded to 256 and 4,096
characters. Treat the text as untrusted context. The CLI reports a revoked
credential or an unavailable provider as a distinct error.

## Run JavaScript with Code Mode

Enable Code Mode explicitly when granting an installed package that supports it.
Revoke an existing direct-only grant before enabling a new grant.

```sh
hypermesh-visor github-issue enable SESSION dev.visor.github-issue REVISION ACCOUNT OWNER/REPO --code-mode
hypermesh-visor github-issue code SESSION dev.visor.github-issue linked-issues.js
hypermesh-visor github-issue code-audit SESSION dev.visor.github-issue
```

Pass `-` as the file to read UTF-8 JavaScript from stdin. The script is the body
of a synchronous function. It can call `github.issue.read(number)` and return
JSON. Each call reads from the account and repository in the original grant.
For example, `linked-issues.js` can follow a same-repository reference in an
issue body.

```js
const first = github.issue.read(42);
const match = first.body?.match(/\bsee #([1-9]\d*)\b/i);
const issues = match ? [first, github.issue.read(Number(match[1]))] : [first];
return issues.map(({ number, title, state }) => ({ number, title, state }));
```

Visor checks the live session, original grant, local consent, GitHub account,
and repository visibility on each read. A refused read ends the run. Closing
the session, pausing automation, or revoking the grant stops the run. The
Linux plugin sandbox isolates the JavaScript process. Separately installed
packages use Bubblewrap. All provider
requests and credentials stay in Visor. Code Mode accepts up to 8 KiB of source,
four issue reads, 16 KiB of JSON output, and 15 seconds per run.

On the owner-only companion socket, set `"code_mode":true` in the installed
plugin grant request. Post `{"code":"return github.issue.read(42);"}` to
`/session/{session}/plugins/{id}/github-issue/code`. The result includes the
returned JSON, run ID, and count of issue reads. A GET on that route returns
the session's recent call outcomes. Audit entries contain the run ID, call
number, tool name, and outcome.

The headless CI plugin contract compares direct calls and Code Mode against the
same fixture. It checks matching results and provider authorization, and
reports tool turns, response bytes, a four-bytes-per-token estimate, and
elapsed time. The fixture uses no model provider or GitHub account.

## Public scope

`github.issue.read` requires API 1, an installed revision, and a session grant
whose target is `{"account":123,"repository":"OWNER/REPO"}`. Discovery and
the sandbox use the existing manifest and wire schemas. The `tools/call`
input is `{"issue":{...}}` with the bounded issue fields above. The
[manifest template](manifest.template.json) gives their JSON Schema.

On the owner-only companion socket,
`GET /session/{session}/plugins/{id}/github-issue` returns status.
`POST` on the same route accepts `{"target":{"account":123,"repository":"OWNER/REPO"},"number":42}`.
Grant and revoke use the existing
[plugin endpoints](../README.md#enable-invoke-and-revoke-from-visor).
