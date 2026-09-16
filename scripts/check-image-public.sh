#!/bin/sh
# Checks that a ghcr.io image can be pulled without credentials.
#
#   scripts/check-image-public.sh ghcr.io/OWNER/NAME[:TAG|@sha256:DIGEST]
#
# Exits 0 when the manifest is served to an anonymous client, 1 when the
# registry refuses (the GHCR package is private, or the tag does not exist)
# and 2 on bad usage. Only curl and sed are needed.
#
# scripts/install.sh and the quickstart pull the vise-server image without a
# docker login, and GHCR creates every package as private on its first push,
# so the publish workflow runs this after each release and image-public.yml
# runs it on a schedule; both fail loudly until a maintainer makes the package
# public (CONTRIBUTING.md, "Container image").

set -eu

ref="${1:-}"
case "$ref" in
    ghcr.io/*/*) ;;
    *)
        echo "usage: $0 ghcr.io/OWNER/NAME[:TAG|@sha256:DIGEST]" >&2
        exit 2
        ;;
esac

path="${ref#ghcr.io/}"
case "$path" in
    *@*)
        repo="${path%%@*}"
        target="${path#*@}"
        ;;
    *:*)
        repo="${path%%:*}"
        target="${path#*:}"
        ;;
    *)
        repo="$path"
        target="latest"
        ;;
esac

# The same two requests an anonymous `docker pull` makes: an unscoped token
# for the repository, then the manifest with it. A private package already
# fails the first one with 401.
token="$(curl -fsS "https://ghcr.io/token?scope=repository:${repo}:pull" 2>/dev/null \
    | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')" || token=""
if [ -n "$token" ] && curl -fsS -o /dev/null \
    -H "Authorization: Bearer ${token}" \
    -H 'Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json' \
    "https://ghcr.io/v2/${repo}/manifests/${target}"; then
    echo "${ref} can be pulled anonymously"
    exit 0
fi

owner="${repo%%/*}"
name="${repo#*/}"
echo "${ref} cannot be pulled anonymously: the GHCR package is private (or the tag does not exist)." >&2
echo "A maintainer has to change its visibility to public once: https://github.com/orgs/${owner}/packages/container/${name}/settings" >&2
exit 1
