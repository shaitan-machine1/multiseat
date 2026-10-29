# Resolve the patched libseat source used by root compatibility harnesses.
#
# The multiseat repository is standalone: by default the pinned seatd fork is
# cloned into a gitignored cache instead of assuming a sibling checkout.
# LIBSEAT_REPO selects an existing local checkout, LIBSEAT_URL changes the
# remote, and LIBSEAT_COMMIT pins the revision. Network failures return 77 so
# harnesses can skip rather than fail.
LIBSEAT_URL=${LIBSEAT_URL:-https://github.com/shaitan-machine1/seatd.git}
LIBSEAT_COMMIT=${LIBSEAT_COMMIT:-7813c08e35cbff69069dbdecda97f7c7ff506cc2}

resolve_libseat_source() {
    if [ -n "${LIBSEAT_REPO:-}" ]; then
        [ -d "$LIBSEAT_REPO" ] || return 1
        printf '%s\n' "$LIBSEAT_REPO"
        return 0
    fi
    command -v git >/dev/null 2>&1 || return 77
    cache=${LIBSEAT_CACHE:-"$repo/.test-libseat/seatd"}
    if [ ! -d "$cache/.git" ]; then
        mkdir -p "$(dirname "$cache")" || return 1
        git clone --quiet "$LIBSEAT_URL" "$cache" >/dev/null 2>&1 || return 77
    fi
    if ! git -C "$cache" cat-file -e "$LIBSEAT_COMMIT^{commit}" 2>/dev/null; then
        git -C "$cache" fetch --quiet origin >/dev/null 2>&1 || return 77
    fi
    git -C "$cache" cat-file -e "$LIBSEAT_COMMIT^{commit}" 2>/dev/null || return 77
    git -C "$cache" checkout --quiet --detach "$LIBSEAT_COMMIT" >/dev/null 2>&1 || return 1
    printf '%s\n' "$cache"
}
