#!/usr/bin/env bash
# Compute the next release version from the tags, for the release workflow
# (SDD §9). Mirrors the rule Az107/space-elevator already publishes with.
#
#   - HEAD already carries a tag  -> skip=true   (a re-run must not republish)
#   - there is no tag at all      -> v0.1.0
#   - the newest tag is vX.Y.Z    -> vX.Y.(Z+1)
#
# The bump is patch-only by design: the workflow then checks that this value
# matches [workspace.package].version, so a minor or major release is cut by
# tagging by hand (see the release procedure in README.md) rather than by
# guessing intent from the tag list.
#
# Output: `key=value` lines on stdout, and the same lines appended to
# $GITHUB_OUTPUT when it is set. Exits non-zero only on a real error.
set -euo pipefail

emit() {
  local key="$1" value="$2"
  echo "${key}=${value}"
  if [ -n "${GITHUB_OUTPUT:-}" ]; then
    echo "${key}=${value}" >>"${GITHUB_OUTPUT}"
  fi
}

# `git describe` fails when HEAD is not exactly a tag, which is the normal case.
if head_tag="$(git describe --tags --exact-match HEAD 2>/dev/null)"; then
  echo "HEAD is already tagged ${head_tag}; nothing to publish." >&2
  emit skip true
  exit 0
fi

latest="$(git tag -l 'v[0-9]*' --sort=-v:refname | head -n1 || true)"
if [ -z "${latest}" ]; then
  next="v0.1.0"
else
  ver="${latest#v}"
  major="${ver%%.*}"
  rest="${ver#*.}"
  minor="${rest%%.*}"
  patch="${rest#*.}"
  next="v${major}.${minor}.$((patch + 1))"
fi

echo "newest tag: ${latest:-<none>} -> next: ${next}" >&2
emit next "${next}"
emit skip false
