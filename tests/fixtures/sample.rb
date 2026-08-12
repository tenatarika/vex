module Billing
  # Mixed into every processor that talks to PaymentGateway.
  module Retryable
    def with_retry(attempts = 3)
      yield
    end
  end

  class PaymentProcessor
    def process(amount)
      raise NotImplementedError, "PaymentProcessor is abstract"
    end
  end

  class StripePaymentProcessor < PaymentProcessor
    include Retryable

    def initialize(api_key)
      @api_key = api_key
    end

    def process(amount)
      with_retry { charge(amount) }
    end

    private

    def charge(amount)
      TransactionId.of(amount)
    end
  end
end
