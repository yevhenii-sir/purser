using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;

namespace Purser
{
    /// <summary>What <see cref="ObservedPurchases"/> asks of the service that owns it.</summary>
    internal interface IObservedPurchasesHost
    {
        /// <summary>Reads the user id again; null when there is no player.</summary>
        string SyncUser();

        /// <summary>The server accepted a reported or provisionally granted purchase.</summary>
        void OnVerified(PurchaseResult result);

        /// <summary>The server definitively refused a reported or provisionally granted purchase.</summary>
        void OnRejected(PurchaseResult result);
    }

    /// <summary>
    /// Purchases finished in the store before the server answered: those the game reports in observer mode
    /// (<see cref="IBillingService.ReportPurchaseAsync"/>) and those granted provisionally when the server could not be
    /// reached (<see cref="BillingConfig.failOpenAfterSeconds"/>). The store never delivers them again, so they are kept
    /// in the storage until the server answered, and verified again on launch, when the app returns to the foreground
    /// and when a player appears. Used from the Unity main thread only.
    /// </summary>
    internal sealed class ObservedPurchases
    {
        internal const string StorageKey = "purser.observed";
        /// <summary>Purchases kept at most; the oldest go first (a server that never answers must not grow the storage).</summary>
        internal const int MaxEntries = 100;

        /// <summary>One purchase waiting for the server, as stored.</summary>
        internal sealed class Entry
        {
            [JsonProperty("p")] public ExternalPurchase Purchase;
            /// <summary>Granted by the package without the server's answer (fail-open).</summary>
            [JsonProperty("provisional")] public bool Provisional;
            [JsonProperty("added_ms")] public long AddedMs;
            /// <summary>The player who bought it: verified for them even if another player is signed in by then.</summary>
            [JsonProperty("user")] public string UserId;
        }

        private readonly BillingContext m_Context;
        private readonly IObservedPurchasesHost m_Host;
        private readonly CustomerInfoRefresher m_Refresher;
        private List<Entry> m_Entries;
        /// <summary>The verification running for a purchase, by key.</summary>
        private readonly Dictionary<string, Task<PurchaseResult>> m_Running = new Dictionary<string, Task<PurchaseResult>>();

        public ObservedPurchases(BillingContext context, IObservedPurchasesHost host, CustomerInfoRefresher refresher)
        {
            m_Context = context;
            m_Host = host;
            m_Refresher = refresher;
        }

        /// <summary>Purchases waiting for the server.</summary>
        public int Count => Entries.Count;

        private List<Entry> Entries
        {
            get
            {
                if (m_Entries != null)
                    return m_Entries;
                try
                {
                    var json = m_Context.Storage.Load(StorageKey);
                    m_Entries = string.IsNullOrEmpty(json) ? new List<Entry>() : JsonConvert.DeserializeObject<List<Entry>>(json) ?? new List<Entry>();
                    m_Entries.RemoveAll(e => e?.Purchase == null || string.IsNullOrEmpty(e.Purchase.Key));
                }
                catch (Exception e)
                {
                    m_Context.Log.Warning("Stored purchases waiting for the server could not be read: " + e.Message);
                    m_Entries = new List<Entry>();
                }
                return m_Entries;
            }
        }

        private void Save()
        {
            try
            {
                m_Context.Storage.Save(StorageKey, Entries.Count == 0 ? "" : JsonConvert.SerializeObject(Entries));
                // A purchase the store will not deliver again must survive the app being killed.
                (m_Context.Storage as IFlushableBillingStorage)?.Flush();
            }
            catch (Exception e)
            {
                m_Context.Log.Warning("Purchases waiting for the server could not be saved: " + e.Message);
            }
        }

        private Entry Find(string key) => Entries.FirstOrDefault(e => e.Purchase.Key == key);

        /// <summary>Keeps a purchase until the server answered (a second report of it changes nothing but the flag).</summary>
        public void Add(ExternalPurchase purchase, bool provisional)
        {
            var existing = Find(purchase.Key);
            if (existing != null)
            {
                existing.Provisional |= provisional;
            }
            else
            {
                Entries.Add(new Entry
                {
                    Purchase = purchase.Copy(),
                    Provisional = provisional,
                    AddedMs = new DateTimeOffset(m_Context.UtcNow()).ToUnixTimeMilliseconds(),
                    UserId = m_Context.User,
                });
                while (Entries.Count > MaxEntries)
                {
                    m_Context.Log.Warning($"Dropping {Entries[0].Purchase} from the purchases waiting for the server (more than {MaxEntries}).");
                    Entries.RemoveAt(0);
                }
            }
            Save();
        }

        public void MarkProvisional(string key)
        {
            var e = Find(key);
            if (e != null && !e.Provisional)
            {
                e.Provisional = true;
                Save();
            }
        }

        private void Remove(string key)
        {
            if (Entries.RemoveAll(e => e.Purchase.Key == key) > 0)
                Save();
        }

        /// <summary>Verifies the kept purchase of <paramref name="key"/>; the running verification when there is one.
        /// Ends with the server's answer, or <see cref="PurchaseStatus.PendingVerification"/> once the retries ran out
        /// (the purchase stays kept).</summary>
        public Task<PurchaseResult> VerifyAsync(string key)
        {
            if (m_Running.TryGetValue(key, out var running))
                return running;
            var task = RunAsync(key);
            if (!task.IsCompleted)
                m_Running[key] = task;
            return task;
        }

        /// <summary>Verifies every kept purchase that is not being verified: on launch, on resume, when a player
        /// appears.</summary>
        public void RetryAll()
        {
            if (m_Context.IsDisposed || m_Context.User == null)
                return;
            foreach (var e in Entries.ToList())
                if (!m_Running.ContainsKey(e.Purchase.Key))
                    _ = VerifyAsync(e.Purchase.Key);
        }

        private async Task<PurchaseResult> RunAsync(string key)
        {
            var entry = Find(key);
            var p = entry?.Purchase;
            var productId = p == null ? null : m_Context.Config.FindByStoreId(p.StoreProductId)?.id ?? p.StoreProductId;
            try
            {
                if (p == null)
                    return new PurchaseResult(PurchaseStatus.Failed, null, message: "not kept");
                var user = !string.IsNullOrEmpty(entry.UserId) ? entry.UserId : m_Context.User ?? m_Host.SyncUser();
                if (user == null)
                    return new PurchaseResult(PurchaseStatus.PendingVerification, productId, p.StoreProductId, p.TransactionId,
                        "kept until there is a player id");
                if (m_Context.SkipServer || p.Store == "fake")
                {
                    Remove(key);
                    return Verified(entry, productId, new PurchaseResult(PurchaseStatus.Success, productId, p.StoreProductId,
                        p.TransactionId, "editor: recorded locally without the server"));
                }

                var request = new VerifyRequest
                {
                    AppId = m_Context.Config.AppIdFor(p.Store),
                    UserId = user,
                    ProductId = p.StoreProductId,
                    PurchaseToken = NullIfEmpty(p.PurchaseToken),
                    OrderId = NullIfEmpty(p.TransactionId),
                    StoreUserId = NullIfEmpty(p.StoreUserId),
                    AppInstanceId = await m_Context.AppInstanceIdAsync(),
                    PriceMicros = p.PriceMicros,
                    Currency = NullIfEmpty(p.Currency),
                    Attribution = await m_Context.AttributionAsync(),
                    Client = m_Context.Client,
                    ClientFinished = true,
                };
                var delays = m_Context.Config.retryDelaysSeconds ?? new int[0];
                var attempt = 0;
                var rateLimited = 0;
                while (true)
                {
                    if (m_Context.IsDisposed)
                        return new PurchaseResult(PurchaseStatus.NotInitialized, productId, p.StoreProductId, p.TransactionId, "billing was disposed");
                    var response = await m_Context.Api.VerifyAsync(p.Store, request, m_Context.Lifetime);
                    var server = response?.Value;
                    var transactionId = server?.TransactionId ?? p.TransactionId;
                    switch (OrderProcessor.Classify(response))
                    {
                        case OrderProcessor.Outcome.Success:
                        case OrderProcessor.Outcome.PendingDelivery:
                            Remove(key);
                            return Verified(entry, productId, new PurchaseResult(PurchaseStatus.Success, productId, p.StoreProductId,
                                transactionId, server?.Message, server));
                        case OrderProcessor.Outcome.Rejected:
                            Remove(key);
                            var rejected = new PurchaseResult(PurchaseStatus.Rejected, productId, p.StoreProductId, transactionId,
                                $"{server?.Status}: {server?.Message}", server, entry.Provisional);
                            m_Context.Log.Warning($"The server refused {p}: {rejected.Message}");
                            m_Host.OnRejected(rejected);
                            if (entry.Provisional)
                                m_Refresher.RefreshInBackground();
                            return rejected;
                        case OrderProcessor.Outcome.PendingPayment:
                            // Not paid yet (a pending Google payment): kept, asked again later.
                            return new PurchaseResult(PurchaseStatus.PendingPayment, productId, p.StoreProductId, transactionId,
                                "payment is not completed yet", server);
                    }

                    m_Context.Log.Warning($"Verification of {p} failed ({response}), attempt {attempt + rateLimited + 1}.");
                    TimeSpan wait;
                    if (response != null && response.IsRateLimited && response.RetryAfter.HasValue && rateLimited < OrderProcessor.MaxRateLimitedRetries)
                    {
                        rateLimited++;
                        wait = response.RetryAfter.Value;
                    }
                    else
                    {
                        if (attempt >= delays.Length)
                            return new PurchaseResult(PurchaseStatus.PendingVerification, productId, p.StoreProductId, p.TransactionId,
                                "kept: verified again on the next launch or when the app returns to the foreground");
                        wait = TimeSpan.FromSeconds(delays[attempt]);
                        attempt++;
                    }
                    await m_Context.Delay(wait, m_Context.Lifetime);
                }
            }
            catch (OperationCanceledException)
            {
                return new PurchaseResult(PurchaseStatus.NotInitialized, productId, p?.StoreProductId, p?.TransactionId, "billing was disposed");
            }
            catch (ObjectDisposedException) when (m_Context.IsDisposed)
            {
                return new PurchaseResult(PurchaseStatus.NotInitialized, productId, p?.StoreProductId, p?.TransactionId, "billing was disposed");
            }
            catch (Exception e)
            {
                m_Context.Log.Error($"Verifying {p} failed: {e}");
                return new PurchaseResult(PurchaseStatus.PendingVerification, productId, p?.StoreProductId, p?.TransactionId, e.Message);
            }
            finally
            {
                m_Running.Remove(key);
            }
        }

        private PurchaseResult Verified(Entry entry, string productId, PurchaseResult result)
        {
            m_Host.OnVerified(result);
            var config = m_Context.Config.FindByStoreId(entry.Purchase.StoreProductId);
            if (config == null || config.type != BillingProductType.Consumable)
                m_Refresher.RefreshInBackground();
            return result;
        }

        private static string NullIfEmpty(string s) => string.IsNullOrEmpty(s) ? null : s;
    }
}
