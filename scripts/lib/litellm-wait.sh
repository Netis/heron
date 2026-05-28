# Shared helper: poll LiteLLM until it answers /v1/models with the
# configured API key, with exponential backoff. Source from any
# agent script that's about to invoke `claude --print` (or any
# OpenAI-compatible client) — the helper makes the agent tolerant of
# LiteLLM / GLM-5 restarts that happen DURING a workflow's queue
# time. Without it, a transient backend hiccup causes the run to
# exit immediately and the work (and 15-min queue wait, on a busy
# self-hosted runner) is wasted.
#
# Usage:
#   source "$REPO_ROOT/scripts/lib/litellm-wait.sh"
#   wait_for_litellm || exit $?
#   claude --print ...
#
# Required env:
#   ANTHROPIC_BASE_URL  full URL (no trailing slash) — e.g.,
#                       http://172.16.103.81:4000
#   ANTHROPIC_API_KEY   bearer token; never printed; used only in
#                       the Authorization header for the probe.
#
# Optional env:
#   MAX_LITELLM_WAIT_SECONDS  total budget before giving up. Default
#                              1800 (30 min). Workflow timeouts are
#                              120-360 min so the budget fits.
#
# Output:
#   On success (LiteLLM reachable): no output unless the helper had
#   to wait, in which case a single "litellm available again after
#   <N>s wait" line goes to stderr.
#   On waiting: one ::warning:: line per retry, with elapsed +
#   next backoff. No secret values ever printed.
#   On total failure: one ::error:: line and return code 2.
#
# Why the helper polls /v1/models specifically: it's a cheap GET
# that exercises auth (bearer-token check on the proxy) without
# burning model tokens. A 200 response means "LiteLLM is up AND our
# key is accepted" — both preconditions for the upcoming agent run.

# Inner probe — returns:
#   0     2xx (LiteLLM up AND key accepted)
#   1     4xx (LiteLLM up, auth rejected — don't wait, fail fast)
#   2     other status (5xx, connect refused, timeout) — retry
_litellm_probe() {
    local url="$1" key="$2"
    # `-o /dev/null -w %{http_code}` captures status without dumping
    # the body. `--max-time 5` covers cold-cache responses. We do
    # NOT use `-f` here so we can distinguish 401 from connect-
    # refused / 5xx.
    local code
    code=$(curl -sS -o /dev/null -w '%{http_code}' --max-time 5 \
        -H "Authorization: Bearer ${key}" \
        "${url}/v1/models" 2>/dev/null) || code='000'
    case "$code" in
        2*)  return 0 ;;
        4*)  return 1 ;;
        *)   return 2 ;;
    esac
}

wait_for_litellm() {
    local url="${ANTHROPIC_BASE_URL:?ANTHROPIC_BASE_URL must be set}"
    local key="${ANTHROPIC_API_KEY:?ANTHROPIC_API_KEY must be set}"
    local max="${MAX_LITELLM_WAIT_SECONDS:-1800}"

    # Initial probe — most calls succeed here and the function
    # returns silently.
    _litellm_probe "$url" "$key"
    local rc=$?
    case $rc in
        0) return 0 ;;
        1)
            echo "::error::litellm reachable but rejected our API key (4xx). Not waiting — check LITELLM_API_KEY secret." >&2
            return 3
            ;;
    esac

    local elapsed=0
    local backoff=10

    while [ "$elapsed" -lt "$max" ]; do
        echo "::warning::litellm unreachable; sleeping ${backoff}s before retry (elapsed: ${elapsed}s / ${max}s)" >&2
        sleep "$backoff"
        elapsed=$((elapsed + backoff))

        _litellm_probe "$url" "$key"
        rc=$?
        case $rc in
            0)
                echo "litellm available again after ${elapsed}s wait" >&2
                return 0
                ;;
            1)
                echo "::error::litellm now responds 4xx — auth rejected after wait. Stopping." >&2
                return 3
                ;;
        esac

        # Exponential backoff, capped at 5 min so we don't sleep
        # past a recovery window.
        backoff=$((backoff * 2))
        if [ "$backoff" -gt 300 ]; then
            backoff=300
        fi
    done

    echo "::error::litellm still unreachable after ${max}s; giving up" >&2
    return 2
}
