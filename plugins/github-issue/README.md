# GitHub issue pilot

The Linux Visor pilot reads one public GitHub issue into a coding task. The
package uses the public SDK and the signed distribution format as
`dev.visor.github-issue`. Inspect the installed publisher and key fingerprint
before granting access. A local author build has a community publisher.

## Build and install

Build the package from this directory. Put the executable and manifest in a
private package directory. Sign and install it using the
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

Copy the package revision from the installed list. Supply the numeric GitHub
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
