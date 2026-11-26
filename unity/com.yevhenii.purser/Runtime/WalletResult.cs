using System.Collections.Generic;

namespace Purser
{
    /// <summary>How a wallet request ended.</summary>
    public enum WalletStatus
    {
        /// <summary>The balances were read, or the currency was spent.</summary>
        Success,
        /// <summary>The balance is less than the amount: nothing was spent.</summary>
        InsufficientFunds,
        /// <summary>The server refused it (see <see cref="WalletResult.Message"/>: a reference used for another spend,
        /// a build without player tokens, a bad currency id).</summary>
        Rejected,
        /// <summary>The server was not reached or answered an error: worth retrying with the same reference.</summary>
        Failed,
        /// <summary>Billing is not initialized, or no user id is set.</summary>
        NotInitialized,
    }

    /// <summary>Result of <see cref="IBillingService.GetWalletAsync"/> and <see cref="IBillingService.SpendAsync"/>.</summary>
    public sealed class WalletResult
    {
        private static readonly IReadOnlyDictionary<string, long> s_None = new Dictionary<string, long>();

        public WalletStatus Status { get; }

        /// <summary>Balance per currency id (<see cref="IBillingService.GetWalletAsync"/>); empty otherwise.</summary>
        public IReadOnlyDictionary<string, long> Balances { get; }

        /// <summary>The balance of the spent currency after the spend (<see cref="IBillingService.SpendAsync"/>).</summary>
        public long Balance { get; }

        /// <summary>The reference was spent before: this request changed nothing.</summary>
        public bool Repeated { get; }

        /// <summary>Error code and message when the request did not succeed.</summary>
        public string Message { get; }

        public bool IsSuccess => Status == WalletStatus.Success;

        public WalletResult(WalletStatus status, IReadOnlyDictionary<string, long> balances = null, long balance = 0, bool repeated = false, string message = null)
        {
            Status = status;
            Balances = balances ?? s_None;
            Balance = balance;
            Repeated = repeated;
            Message = message;
        }

        /// <summary>The balance of <paramref name="currency"/>, 0 when the player never had it.</summary>
        public long Of(string currency) => currency != null && Balances.TryGetValue(currency, out var v) ? v : 0;

        public override string ToString() => $"{Status} {Message}".Trim();
    }
}
