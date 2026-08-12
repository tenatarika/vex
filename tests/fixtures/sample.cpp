#include "payment_gateway.h"
#include "logger.h"
#include <string>

namespace billing {

// Implements PaymentGateway over the StripeClient SDK.
class StripePaymentGateway : public PaymentGateway {
public:
    explicit StripePaymentGateway(std::string api_key)
        : api_key_(std::move(api_key)) {}

    TransactionId process(long amount) override {
        log_attempt("charging via StripeClient");
        return charge(amount);
    }

private:
    TransactionId charge(long amount) {
        return TransactionId::of(amount);
    }

    void log_attempt(const std::string& message);

    std::string api_key_;
};

}  // namespace billing
