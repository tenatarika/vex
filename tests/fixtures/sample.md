# Billing Architecture

The billing subsystem charges invoices through a `PaymentProcessor`
implementation. Production uses `StripePaymentProcessor`.

## Payment flow

1. `InvoiceService` loads the pending invoice.
2. `StripePaymentProcessor` charges the customer via StripeClient.
3. `TransactionId` is persisted back onto the invoice row.

### Retries

Failed charges are retried up to three times by `Retryable`. A permanent
failure marks the invoice `failed` and emits a `PaymentFailed` event.

## Storage

Invoices live in the `invoices` table; see `InvoiceRepository` for the
query surface.
