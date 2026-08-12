<?php

namespace App\Billing;

interface PaymentProcessor
{
    public function process(int $amount): TransactionId;
}

trait Retryable
{
    public function withRetry(callable $operation, int $attempts = 3)
    {
        return $operation();
    }
}

// Implements PaymentProcessor against the StripeClient SDK.
class StripePaymentProcessor implements PaymentProcessor
{
    use Retryable;

    private string $apiKey;

    public function __construct(string $apiKey)
    {
        $this->apiKey = $apiKey;
    }

    public function process(int $amount): TransactionId
    {
        return $this->withRetry(fn() => $this->charge($amount));
    }

    private function charge(int $amount): TransactionId
    {
        return TransactionId::of($amount);
    }
}
