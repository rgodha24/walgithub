# tests/lib-auth.sh — shared auth setup for the remote test scripts. Source after BASE_URL is set.
#
# Resolves a bearer token (in order): $WALGIT_AUTH_HEADER, $WALGIT_TOKEN. Then installs it in a
# PRIVATE global git config (GIT_CONFIG_GLOBAL, temp file) so every git process in this run —
# including nested ones — is authenticated without ever touching ~/.gitconfig, and disables
# interactive prompts. Loopback servers need no token.

walgit_auth_setup() {
    local base="${1:?BASE_URL}"
    export GIT_TERMINAL_PROMPT=0
    export GIT_ASKPASS=/bin/true
    case "$base" in
        http://127.0.0.1*|http://localhost*) AUTH_CURL_ARGS=(); GIT_AUTH_ARGS=(); return 0 ;;
    esac
    local token
    if [[ -n "${WALGIT_AUTH_HEADER:-}" ]]; then
        token="${WALGIT_AUTH_HEADER#Authorization: Bearer }"
    elif [[ -n "${WALGIT_TOKEN:-}" ]]; then
        token="$WALGIT_TOKEN"
    fi
    if [[ -z "${token:-}" ]]; then
        echo "walgit: no auth token available (set WALGIT_TOKEN to an access token from <host>/_auth/tokens, or WALGIT_AUTH_HEADER)" >&2
        return 1
    fi
    WALGIT_AUTH_HEADER="Authorization: Bearer $token"
    export WALGIT_AUTH_HEADER
    AUTH_CURL_ARGS=(-H "$WALGIT_AUTH_HEADER")
    GIT_AUTH_ARGS=(-c "http.extraHeader=$WALGIT_AUTH_HEADER")
    # Private global config for this run (never the user's ~/.gitconfig).
    local host="${base#https://}"; host="${host#http://}"; host="${host%%/*}"
    export GIT_CONFIG_GLOBAL="${GIT_CONFIG_GLOBAL:-$(mktemp /tmp/walgit-gitconfig.XXXXXX)}"
    git config --global "http.https://$host/.extraHeader" "$WALGIT_AUTH_HEADER"
    git config --global transfer.bundleURI true
    # fetch.bundleURI is a per-clone URI (the recipes set it); never a global.
    git config --global --unset-all fetch.bundleURI 2>/dev/null || true
    git config --global user.email "walgit-tests@example.com"
    git config --global user.name "walgit tests"
    echo "auth: bearer header installed in private git config $GIT_CONFIG_GLOBAL for $host" >&2
}
