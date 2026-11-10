#!/usr/bin/env bash
# Enforces AD-1 (hexagonal crate boundaries) on the Cargo workspace.
# Prints "OK <rule>" for each passing rule and "FAIL <crate>: <dep>" for each
# violation, and exits non-zero if any rule fails.
set -Eeuo pipefail
# Any command failure (e.g. cargo: stale lockfile, dependency cycle) is a failed check.
trap '(( BASH_SUBSHELL )) || echo "FAIL a command errored (see above); AD-1 not verified" >&2; exit 1' ERR

cd "$(dirname "$0")/.."

failed=0

# Names of the normal dependencies of a crate. With "direct", only depth 1.
# `--target all` includes platform-specific deps (e.g. Linux-only on macOS).
deps_of() {
  local crate=$1 mode=${2:-all}
  local depth=()
  if [[ $mode == direct ]]; then depth=(--depth 1); fi
  # Skip the first line (the crate itself); keep only the package name.
  cargo tree -p "$crate" -e normal --prefix none --target all --locked ${depth[@]+"${depth[@]}"} \
    | tail -n +2 | awk '{print $1}' | sort -u
}

# rule_allow <crate> <allowed...>: direct normal deps must be in the allowlist.
rule_allow() {
  local crate=$1; shift
  local ok=1 dep allowed hit direct
  direct=$(deps_of "$crate" direct)
  while IFS= read -r dep; do
    [[ -z $dep ]] && continue
    hit=0
    for allowed in "$@"; do
      if [[ $dep == "$allowed" ]]; then hit=1; break; fi
    done
    if (( ! hit )); then
      echo "FAIL $crate: $dep (direct dependency not in allowlist: $*)"
      ok=0
    fi
  done <<< "$direct"
  if (( ok )); then echo "OK $crate direct deps within {$*}"; else failed=1; fi
}

# rule_deny <crate> <forbidden...>: forbidden crates must not appear transitively.
rule_deny() {
  local crate=$1; shift
  local ok=1 dep forbidden
  local tree
  tree=$(deps_of "$crate")
  for forbidden in "$@"; do
    while IFS= read -r dep; do
      if [[ $dep == "$forbidden" ]]; then
        echo "FAIL $crate: $dep (forbidden transitive dependency)"
        ok=0
      fi
    done <<< "$tree"
  done
  if (( ok )); then echo "OK $crate has no transitive {$*}"; else failed=1; fi
}

# rule_no_dependents <crate>: no workspace crate may depend on it (any edge kind).
rule_no_dependents() {
  local crate=$1
  local dependents
  dependents=$(cargo tree -i "$crate" --workspace -e normal,build,dev --prefix none \
    --target all --locked --depth 1 | tail -n +2 | awk '{print $1}' | sort -u)
  if [[ -n $dependents ]]; then
    while IFS= read -r dep; do
      echo "FAIL $dep: $crate (no crate may depend on the shell)"
    done <<< "$dependents"
    failed=1
  else
    echo "OK no workspace crate depends on $crate"
  fi
}

rule_allow nguda-core jiff uuid
rule_allow nguda-engine nguda-core tokio tracing
rule_deny nguda-core tauri rusqlite reqwest hyper
rule_deny nguda-engine tauri rusqlite reqwest hyper
rule_deny nguda-adapters tauri
rule_no_dependents nguda

if (( failed )); then
  echo "AD-1 dependency check failed" >&2
  exit 1
fi
