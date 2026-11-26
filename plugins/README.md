# Visor plugin API

The public contract, Rust SDK, example, and fixtures are covered by this
repository's [MIT license](../LICENSE). First-party, community, and app-owner
packages use the same manifest, wire protocol, sandbox, and authorization
checks.

## Package a plugin

Pick a reverse-domain ID such as `org.example.byte-count` and stable tool names.
Build a static Linux ELF executable for the target architecture. Any language
can implement the [stdio wire contract](../src/plugins/schemas/stdio-v1.json). Rust authors can
use the [standalone SDK](sdk/) and copy the [byte-count example](reference/) to
an independent project. Its Cargo workspace and lockfile are separate from
Visor's. For the SDK example, copy `reference` and `sdk` as siblings, then run
the following in `reference`.

```sh
target="$(rustc -vV | sed -n 's/^host: //p')"
RUSTFLAGS='-C target-feature=+crt-static' cargo build --release --locked --target "$target"
```

Create an owner-only directory with `manifest.json` and `plugin`, both
owner-only files. Use the [manifest schema](../src/plugins/schemas/manifest-v1.json), set
`executable_sha256` to the hex SHA-256 of `plugin`, then record the SHA-256 of
the exact `manifest.json` bytes. A local owner explicitly supplies that manifest
hash when enabling one tool. A new binary or manifest needs a new grant. A
namespace identifies the package. Publisher verification belongs to the catalog
and distribution layer and conveys no permission to invoke a tool.

To share a package from your repository, see [signed distribution and installation](distribution.md).

## Local stdio

Visor starts one isolated process per invocation. Stdout contains one UTF-8
JSON-RPC object per line, with a 96 KiB frame limit including the newline.
Diagnostics belong on stderr, which Visor discards. The process sees a read-only
root containing its executable, stdio pipes, and private user, PID, and
network namespaces. Bubblewrap sets `PWD=/`. The process inherits no host
environment, workspace mount, or credentials. Linux requires an unprivileged Visor process, the system
`/usr/bin/bwrap` with user/PID/network namespace support, and a kernel with
`close_range` support. On Ubuntu with AppArmor user-namespace restrictions,
the owner can enable the distribution's `bwrap-userns-restrict` profile for
`/usr/bin/bwrap`. If sandbox setup fails, the call fails.

The host sends the highest common versions in descending order. The plugin
replies with the package ID and highest offered API version it implements.

```json
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"plugin_id":"org.example.byte-count","api_versions":[1,0]}}
{"jsonrpc":"2.0","id":1,"result":{"plugin_id":"org.example.byte-count","api_version":1}}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"count-bytes","input":{"content_base64":"YWJj"},"timeout_ms":4000}}
{"jsonrpc":"2.0","id":2,"result":{"byte_count":3}}
```

Discovery uses the digest-selected manifest and advertises only the tool
enabled for that session. Both input and output are JSON objects validated
against the tool's schemas. Local JSON Schema references are permitted.
The `workspace.read` scope supplies only `content_base64` from one admitted
file read, up to 32 KiB before encoding. The `github.issue.read` scope uses API 1 and supplies
only a bounded public issue from the account and repository in the owner
grant. See the [GitHub issue pilot](github-issue/README.md) for that scope.
The application can return a
JSON-RPC error with an integer code and message. For example, use
`-32601` for an unknown tool, `-32602` for invalid input, and `-32603` for a
failed tool. Visor reports an error category without repeating plugin text.
The process should exit after responding. A timeout, pause, dropped response
stream, closed session, or disabled grant ends the process and discards a late
result.

### Version policy

Manifest schema version 1 and wire API versions are negotiated separately
from the plugin's package version and Visor's release version. API 1 is the
current contract. API 0 is the preceding compatibility profile and uses
the same call without `timeout_ms`. The host enforces its deadline for both
versions. A plugin should list only versions it implements. Visor rejects
an incompatible manifest before starting a process and rejects a mismatched
handshake before sending data. A failed handshake never triggers a retry
at a lower version.

Maintain the current and preceding wire versions during a migration. Bump the
wire version for a breaking frame or semantic change, document the migration
and deprecation before removing a profile, and retain both conformance fixtures
while both versions are supported. Compatible optional metadata may be added
to negotiation and v1 call messages. Breaking changes to package layout or
required metadata need a new manifest version.

### MCP and hosted profiles

An MCP executable that implements the documented `2025-11-25` stdio protocol
can use `local-mcp-stdio`. Visor initializes MCP and sends the admitted object
as `tools/call.arguments`. It validates the returned
`result.structuredContent` with the same manifest output schema. The
manifest names the single eligible MCP tool. MCP advertisements and
notifications convey no scope. The adapter uses the same grant, read
admission and sandbox as `local-stdio`.

The planned hosted profile for s3r.ch uses the same identity, version,
capability, scope, input and result concepts. A hosted adapter will need an
authenticated service transport, a project-bound connection and brokered
credentials, and equivalent cancellation, expiry and output checks.
Hosted execution requires its own profile and conformance tests. This
manifest version enables the two local profiles.

## Enable, invoke, and revoke from Visor

Use the existing owner-only Linux companion socket, with a live session
and current automation consent. Save an existing `local_inspection` read
selection for the target through the [permission APIs](../docs/permission-templates.md).
The machine must have its normal Host bridge configured for Conductor and
AIuditor review and admission. Every invocation uses that path.

| Request on companion socket | Body and result |
|---|---|
| `POST /session/{id}/plugins` | Send `{"manifest":"/absolute/package/manifest.json","manifest_sha256":"…","tool":"count-bytes","target":"/absolute/project/file"}`. Returns 201 with the enabled tool and API version. |
| `GET /session/{id}/plugins` | Returns enabled packages and tool schemas for the current session. |
| `POST /session/{id}/plugins/{plugin-id}/call` | Send the existing permission-read JSON body with `request_id`, `session_id`, saved `selection`, exact `target`, `max_bytes`, and `timeout_seconds`. Returns a JSON stream with `{"result":{…}}` or `{"error":{"code":"…","message":"…"}}`. |
| `DELETE /session/{id}/plugins/{plugin-id}` | Revokes the grant and cancels an active invocation. Returns 204. |

The [distribution guide](distribution.md) shows activation by installed
revision. The manifest path above is available for explicitly selected
development packages with IDs free in the installed store.

Grants are local to a session and end when it closes or Visor restarts.
One call per grant can run at a time and caller-supplied limits are capped
at 32 KiB and 10 seconds. Each call needs a fresh request ID, saved workspace
selection, active machine context, current local consent, and the owner's
exact read admission. A lost response is uncertain. Check current state
before starting a new request. The plugin receives neither the target
pathname nor session or policy metadata.

## Conformance

On Linux, build the standalone runner with
`cargo build --locked --bin hypermesh-plugin-conformance`. Pass a private
existing scratch directory and synthetic input. The runner uses the same
package validation, sandbox, negotiation, and schema checks as Visor.

```sh
printf '%s\n' '{"content_base64":"YWJj"}' | hypermesh-plugin-conformance \
  /absolute/package/manifest.json MANIFEST_SHA256 count-bytes /absolute/private/scratch
```

The CI contract builds the example in a separate workspace, runs native API 1
and preceding API 0 packages and the MCP adapter, and checks incompatibility,
invalid output, isolation, and timeout. It invokes the independently built
package from Visor through a project-controlled Host policy fixture and verifies
denial, admission, grant revocation and local consent.
