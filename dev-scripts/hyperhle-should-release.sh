#!/bin/sh
# Decide whether to publish a HyperHLE release and which vMAJOR.MINOR.PATCH tag
# to use. A release is due every 6 commits on HEAD since the latest release
# tag, or since the commit that introduced hyperhle release automation when no
# tag exists yet.
#
# The second and third numbers count from 0 up to 12 and then carry into the
# number before them: v1.0.12 is followed by v1.1.0, and v1.12.12 by v2.0.0.
# The first number has no limit (v12.12.12 -> v13.0.0). Older tags from before
# this scheme (e.g. v1.0.121) carry over the same way, so the next release
# after v1.0.121 is v1.1.0.
set -eu

RELEASE_EVERY=6
# Highest value of the second and third numbers; the first is unbounded.
VERSION_PART_MAX=12
WORKFLOW_MARKER="dev-scripts/hyperhle-should-release.sh"

# Only plain vMAJOR.MINOR.PATCH tags count; anything else (V1, v1.2.3-beta,
# hyperhle-test-fix-…) would break the arithmetic below.
latest_tag="$(git tag -l 'v[0-9]*' --sort=-v:refname |
    grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' | head -n 1 || true)"

if [ -n "$latest_tag" ]; then
    commits_since="$(git rev-list --count "${latest_tag}..HEAD")"
    IFS=. read -r major minor patch <<EOT
${latest_tag#v}
EOT
    patch=$((patch + 1))
    if [ "$patch" -gt "$VERSION_PART_MAX" ]; then
        patch=0
        minor=$((minor + 1))
    fi
    if [ "$minor" -gt "$VERSION_PART_MAX" ]; then
        minor=0
        major=$((major + 1))
    fi
else
    baseline="$(git rev-list -n 1 HEAD -- "$WORKFLOW_MARKER" 2>/dev/null || true)"
    if [ -n "$baseline" ]; then
        commits_since="$(git rev-list --count "${baseline}..HEAD")"
    else
        commits_since=0
    fi
    major=1
    minor=0
    patch=0
fi

version="v${major}.${minor}.${patch}"

if [ "${FORCE_ULTRAHLE_RELEASE:-}" = "true" ]; then
    should_release=true
elif [ "${GITHUB_EVENT_NAME:-}" = "workflow_dispatch" ]; then
    # Manual "Build HyperHLE" runs must never publish files to Releases;
    # they only produce build artifacts.
    should_release=false
elif [ "$commits_since" -lt "$RELEASE_EVERY" ]; then
    should_release=false
elif git rev-parse "refs/tags/${version}" >/dev/null 2>&1; then
    should_release=false
else
    should_release=true
fi

if [ "$should_release" = true ]; then
    if [ "${FORCE_ULTRAHLE_RELEASE:-}" = "true" ]; then
        # Manual release: publish script uses git log -n 6 (see changelog_limit).
        changelog_from=""
        changelog_limit=6
    elif [ -n "$latest_tag" ]; then
        changelog_from="$latest_tag"
    elif [ -n "${baseline:-}" ]; then
        changelog_from="$baseline"
    else
        changelog_from=""
    fi
else
    changelog_from=""
    changelog_limit=""
fi

if [ "${GITHUB_OUTPUT:-}" != "" ]; then
    {
        echo "should_release=${should_release}"
        echo "version=${version}"
        echo "commits_since=${commits_since}"
        echo "changelog_from=${changelog_from}"
        echo "changelog_limit=${changelog_limit:-}"
    } >>"$GITHUB_OUTPUT"
else
    echo "should_release=${should_release}"
    echo "version=${version}"
    echo "commits_since=${commits_since}"
    echo "changelog_from=${changelog_from}"
    echo "changelog_limit=${changelog_limit:-}"
fi
