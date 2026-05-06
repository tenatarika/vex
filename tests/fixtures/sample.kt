package com.app.billing

interface PaymentProcessor {
    fun process(amount: Long): Result<TransactionId>
}

class StripePaymentProcessor(
    private val apiKey: String,
    private val logger: Logger,
) : PaymentProcessor {

    override fun process(amount: Long): Result<TransactionId> {
        logger.info("Processing payment of $amount")
        return runCatching { stripeApi.charge(amount) }
    }
}

object PaymentDefaults {
    const val CURRENCY = "USD"
    val MAX_AMOUNT = 1_000_000L
}

data class TransactionId(val value: String)
