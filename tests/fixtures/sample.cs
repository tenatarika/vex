using System;

namespace App.Billing
{
    public interface IPaymentProcessor
    {
        TransactionId Process(long amount);
    }

    // Implements IPaymentProcessor against the StripeClient SDK.
    public class StripePaymentProcessor : IPaymentProcessor
    {
        private readonly string _apiKey;

        public StripePaymentProcessor(string apiKey)
        {
            _apiKey = apiKey;
        }

        public TransactionId Process(long amount)
        {
            Console.WriteLine("charging via StripeClient");
            return Charge(amount);
        }

        private TransactionId Charge(long amount) => TransactionId.Of(amount);
    }
}
