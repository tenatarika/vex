-- Payment processing on top of the StripeClient binding.
local TransactionId = require("billing.transaction_id")

local StripePaymentProcessor = {}
StripePaymentProcessor.__index = StripePaymentProcessor

function StripePaymentProcessor.new(api_key)
    local self = setmetatable({}, StripePaymentProcessor)
    self.api_key = api_key
    return self
end

function StripePaymentProcessor:charge(amount)
    return TransactionId.of(amount)
end

function StripePaymentProcessor:process(amount)
    print("charging via StripeClient")
    return self:charge(amount)
end

local function process_batch(processor, invoices)
    for _, invoice in ipairs(invoices) do
        processor:process(invoice.amount)
    end
end

return {
    StripePaymentProcessor = StripePaymentProcessor,
    process_batch = process_batch,
}
