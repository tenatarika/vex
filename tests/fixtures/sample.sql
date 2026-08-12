-- Billing schema consumed by the InvoiceRepository.
CREATE TABLE invoices (
    id          BIGSERIAL PRIMARY KEY,
    customer_id BIGINT      NOT NULL,
    amount      NUMERIC(12, 2) NOT NULL,
    status      TEXT        NOT NULL DEFAULT 'pending',
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX idx_invoices_customer ON invoices (customer_id);

CREATE VIEW pending_invoices AS
SELECT id, customer_id, amount
FROM invoices
WHERE status = 'pending';

CREATE FUNCTION total_outstanding(target_customer BIGINT)
RETURNS NUMERIC AS $$
    SELECT COALESCE(SUM(amount), 0)
    FROM invoices
    WHERE customer_id = target_customer
      AND status = 'pending';
$$ LANGUAGE SQL;
