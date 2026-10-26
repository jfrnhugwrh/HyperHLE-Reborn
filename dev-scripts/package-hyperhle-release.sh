#!/bin/sh
# Package the UltraHLE Rebirth Android release zip and write release notes for
# action-gh-release. This fork only ships Android builds.
set -eu

VERSION="$1"
CHANGELOG_FROM="${2:-}"
CHANGELOG_LIMIT="${CHANGELOG_LIMIT:-}"
if [ -z "$VERSION" ]; then
    echo "Usage: $0 <version e.g. v1.0.0> [changelog_from_ref]" >&2
    exit 1
fi

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# This fork only ships Android builds; the APK is the only artifact.
android_apk=""
for candidate in \
    artifacts/android/UltraHLE-Rebirth.apk \
    artifacts/android/touchHLE.apk
do
    if [ -e "$candidate" ]; then
        android_apk="$candidate"
        break
    fi
done

if [ -z "$android_apk" ] || [ ! -e "$android_apk" ]; then
    echo "Missing build artifact (the Android build must succeed): $android_apk" >&2
    exit 1
fi

if [ -z "$CHANGELOG_FROM" ]; then
    # The newest release tag older than VERSION (versions roll over at 12, see
    # hyperhle-should-release.sh, so it can't simply be computed).
    CHANGELOG_FROM="$(
        { git tag -l 'v[0-9]*' | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$'; echo "$VERSION"; } |
            sort -u -V | grep -x -B 1 -F "$VERSION" | grep -v -x -F "$VERSION" || true
    )"
    if [ -z "$CHANGELOG_FROM" ]; then
        CHANGELOG_FROM="$(git rev-list -n 1 HEAD -- dev-scripts/hyperhle-should-release.sh)"
    fi
fi

rm -rf release
mkdir -p release

# GitHub username for a commit's author, as "@login", for the changelog.
# Asks the GitHub API (works in the release job, where GH_TOKEN is set), then
# falls back to the login inside a GitHub no-reply email, then to the git
# author name.
author_mention() {
    hash="$1"
    email="$2"
    name="$3"
    login=""
    if [ -n "${GH_TOKEN:-}" ] && [ -n "${GITHUB_REPOSITORY:-}" ] &&
        command -v gh >/dev/null 2>&1; then
        # On failure gh prints the error body to stdout, so only keep the
        # output of a successful call, and only if it looks like a login.
        if api_login="$(gh api "repos/${GITHUB_REPOSITORY}/commits/${hash}" \
            --jq '.author.login // empty' </dev/null 2>/dev/null)"; then
            login="$api_login"
        fi
    fi
    case "$login" in
        *[!A-Za-z0-9[\]-]*) login="" ;;
    esac
    if [ -z "$login" ]; then
        case "$email" in
            *@users.noreply.github.com)
                login="${email%@users.noreply.github.com}"
                login="${login#*+}"
                ;;
        esac
    fi
    if [ -n "$login" ]; then
        printf '@%s' "$login"
    else
        printf '%s' "$name"
    fi
}

# Changelog lines look like: - `abc1234` **@user**: Commit message
write_changelog() {
    git log "$@" --reverse --format='%H%x1f%s%x1f%an%x1f%ae' |
        while IFS="$(printf '\037')" read -r hash subject author email; do
            [ -z "$hash" ] && continue
            short_hash="$(printf '%.7s' "$hash")"
            mention="$(author_mention "$hash" "$email" "$author")"
            if [ -n "${GITHUB_SERVER_URL:-}" ] && [ -n "${GITHUB_REPOSITORY:-}" ]; then
                printf -- '- [`%s`](%s/%s/commit/%s) **%s**: %s\n' \
                    "$short_hash" "$GITHUB_SERVER_URL" "$GITHUB_REPOSITORY" "$hash" \
                    "$mention" "$subject"
            else
                printf -- '- `%s` **%s**: %s\n' "$short_hash" "$mention" "$subject"
            fi
        done
}

{
    printf '%s\n\n' "UltraHLE Rebirth ${VERSION}"
    if [ "${FORCE_ULTRAHLE_RELEASE:-}" = "true" ]; then
        printf '%s\n\n' "_Manual release — changelog shows the latest 6 commits._"
    fi
    printf '%s\n\n' "## Changelog"
    if [ -n "$CHANGELOG_LIMIT" ]; then
        write_changelog -n "$CHANGELOG_LIMIT" HEAD
    elif [ -n "$CHANGELOG_FROM" ]; then
        write_changelog "${CHANGELOG_FROM}..HEAD"
    else
        printf '%s\n' "_No commit range available._"
    fi
} >release/RELEASE_NOTES.md

cd "$ROOT/dev-scripts"
./prepare-release.sh --prepare-files

prefix="UltraHLE-Rebirth"

./prepare-release.sh --create-zip-android "$ROOT/$android_apk" \
    -o "$ROOT/release/${prefix}_Android_AArch64.zip"
