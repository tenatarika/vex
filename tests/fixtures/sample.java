package com.app.billing;

import com.app.core.Logger;

public interface PaymentProcessor {
    TransactionId process(long amount);
}

// Implements PaymentProcessor over the StripeClient SDK.
public class StripePaymentProcessor implements PaymentProcessor {
    private final String apiKey;
    private final Logger logger;

    public StripePaymentProcessor(String apiKey, Logger logger) {
        this.apiKey = apiKey;
        this.logger = logger;
    }

    @Override
    public TransactionId process(long amount) {
        logger.info("charging via StripeClient");
        return this.charge(amount);
    }

    private TransactionId charge(long amount) {
        return TransactionId.of(amount);
    }
}
