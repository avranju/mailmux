#!/bin/sh

set -eu

fail() {
    jq -n \
        --arg message "$1" \
        '{
            success: false,
            message: $message,
            metadata: null,
            metrics: []
        }'

    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 ||
        fail "$1 is required"
}

# jq builds the JSON envelopes, so a missing jq cannot use fail().
command -v jq >/dev/null 2>&1 || {
    printf '%s\n' 'jq is required' >&2
    exit 1
}
require_command curl

[ -n "${MAILINDEX_URL:-}" ] ||
    fail "MAILINDEX_URL is required"

[ -n "${MAILINDEX_API_TOKEN:-}" ] ||
    fail "MAILINDEX_API_TOKEN is required"

input=$(cat) ||
    fail "cannot read processor input"

email=$(
    printf '%s' "$input" |
        jq -e '.email // empty'
) ||
    fail "missing email"

id=$(
    printf '%s' "$email" |
        jq -er '.id | tostring'
) ||
    fail "missing email id"

raw_message_path=$(
    printf '%s' "$email" |
        jq -er '.raw_message_path'
) ||
    fail "missing raw message path"

[ -r "$raw_message_path" ] ||
    fail "raw message path is unreadable"

metadata=$(
    printf '%s' "$email" |
        jq -c '{
            account_id,
            mailbox_name,
            uid,
            mailmux_email_id: .id
        }'
) ||
    fail "invalid email metadata"

url="${MAILINDEX_URL%/}/v1/documents/mailmux/${id}"

response_file=$(mktemp)
trap 'rm -f "$response_file"' EXIT

http_code=$(
    curl \
        --silent \
        --show-error \
        --fail-with-body \
        --output "$response_file" \
        --write-out '%{http_code}' \
        --request PUT \
        "$url" \
        --header "Authorization: Bearer $MAILINDEX_API_TOKEN" \
        --form "metadata=$metadata;type=application/json" \
        --form "message=@$raw_message_path;type=message/rfc822"
) ||
    fail "mailindex transport failed"

case "$http_code" in
    2*)
        jq -e . "$response_file" >/dev/null 2>&1 ||
            fail "mailindex returned invalid JSON"

        result=$(
            jq -n -c \
                --argjson metadata "$(cat "$response_file")" \
                '{
                    success: true,
                    message: "mailindex accepted message",
                    metadata: $metadata,
                    metrics: []
                }' \
                2>/dev/null
        ) ||
            fail "cannot encode processor output"

        printf '%s\n' "$result"
        ;;

    *)
        fail "mailindex returned HTTP $http_code"
        ;;
esac
