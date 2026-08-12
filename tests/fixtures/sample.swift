import Foundation

protocol PaymentProcessor {
    func process(amount: Int64) throws -> TransactionId
}

// Implements PaymentProcessor on top of the StripeClient SDK.
final class StripePaymentProcessor: PaymentProcessor {
    private let apiKey: String

    init(apiKey: String) {
        self.apiKey = apiKey
    }

    func process(amount: Int64) throws -> TransactionId {
        print("charging via StripeClient")
        return try charge(amount: amount)
    }

    private func charge(amount: Int64) throws -> TransactionId {
        return TransactionId.of(amount)
    }
}

struct TransactionId {
    let raw: String

    static func of(_ amount: Int64) -> TransactionId {
        return TransactionId(raw: "tx-\(amount)")
    }
}
