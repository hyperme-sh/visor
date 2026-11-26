#!/usr/bin/env bash
# Exercise the catalog command with its real CPU runner and synthetic weights.
set -euo pipefail
umask 077
repo="$(cd "$(dirname "$0")/.." && pwd)"
cd "$repo"
cargo build --locked --no-default-features --features headless --bin hypermesh-visor
visor="$repo/target/debug/hypermesh-visor"
scratch="$(mktemp -d)"
trap 'rm -rf -- "$scratch"' EXIT
export HYPERMESH_CONFIG_DIR="$scratch/config"

"$visor" catalog list | jq -e '
    .schema_version == 1 and (.tools | length == 11)
    and ([.tools[] | select(.availability == "visor_installable") | .id] == ["llama2-c"])
'
plan="$("$visor" catalog plan llama2-c)"
printf '%s' "$plan" | jq -e '.plan.local_only and .plan.compiler_available and .plan.sandbox_available'
machine="$(jq -r '.plan.machine.id' <<<"$plan")"
digest="$(jq -r .plan_sha256 <<<"$plan")"
if "$visor" catalog install llama2-c wrong-machine "$digest"; then
  echo 'Installation accepted an incorrect machine'
  exit 1
fi
if "$visor" catalog install llama2-c "$machine" wrong-digest; then
  echo 'Installation accepted a changed plan'
  exit 1
fi
test ! -e "$HYPERMESH_CONFIG_DIR"
installed="$("$visor" catalog install llama2-c "$machine" "$digest")"
runner="$(jq -r .executable <<<"$installed")"
revision="$(jq -r .installation.id <<<"$installed")"
test -x "$runner"
cmp src/tool_catalog/llama2c/LICENSE "$(dirname "$runner")/LICENSE"
actual="$("$visor" catalog health llama2-c)"
printf '%s' "$actual" | jq -e --arg machine "$machine" --arg revision "$revision" '
    .machine.id == $machine and .installation.id == $revision and .state == "healthy"
    and .probe == "synthetic_checkpoint" and .generated_tokens == 2 and .local_only
'
printf 'llama2.c generated two expected tokens on the selected %s machine\n' "$(jq -r .plan.machine.architecture <<<"$plan")"

# Confirm that a damaged binary cannot pass health and can still be uninstalled.
chmod u+w "$runner"
printf '%s' 'fixture-only corruption' >>"$runner"
if "$visor" catalog health llama2-c; then
  echo 'Health accepted a modified runner'
  exit 1
fi
mkdir "$HYPERMESH_CONFIG_DIR/models"
printf '%s' 'owner data' >"$HYPERMESH_CONFIG_DIR/models/keep"
"$visor" catalog remove llama2-c "$machine" "$revision" | jq -e '.state == "removed"'
test ! -e "$runner"
test "$(cat "$HYPERMESH_CONFIG_DIR/models/keep")" = 'owner data'
"$visor" catalog health llama2-c | jq -e '.state == "not_installed"'
