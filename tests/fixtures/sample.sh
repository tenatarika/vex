#!/usr/bin/env bash
set -euo pipefail

# Charges an invoice through the StripeClient CLI wrapper.
readonly RETRY_ATTEMPTS=3

charge_invoice() {
    local invoice_id="$1"
    local amount="$2"
    echo "charging ${invoice_id} via StripeClient"
    stripe_client charge --amount "${amount}"
}

process_batch() {
    local batch_file="$1"
    while read -r invoice_id amount; do
        charge_invoice "${invoice_id}" "${amount}"
    done < "${batch_file}"
}

main() {
    process_batch "${1:-invoices.txt}"
}

main "$@"
