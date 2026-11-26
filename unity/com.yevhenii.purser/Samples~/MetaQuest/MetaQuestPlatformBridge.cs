// IMetaQuestIap over the Meta XR Platform SDK (Oculus.Platform: Core, Users, IAP).
// 1. Import the Meta XR Platform SDK and set the app id (Meta > Platform > Edit Settings); complete the Data Use
//    Checkup for User ID and In-App Purchases (without User ID the logged-in user's id is 0).
// 2. Add PURSER_META_PLATFORM to Player Settings > Scripting Define Symbols (Android).
// 3. Pass the adapter to the billing service on Quest builds:
//      var deps = new BillingDependencies { UserIdProvider = ... };
//      deps.Store = new MetaQuestStoreAdapter(new MetaQuestPlatformBridge());
//      var billing = Billing.Create(config, deps);
// Written against the Platform SDK's Unity API (v60+); check the names below against your SDK version (CI does not
// build it).
#if PURSER_META_PLATFORM
using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using Oculus.Platform;
using Oculus.Platform.Models;

namespace Purser.Samples
{
    public sealed class MetaQuestPlatformBridge : IMetaQuestIap
    {
        public Task<string> InitializeAsync(CancellationToken ct)
        {
            var done = new TaskCompletionSource<string>();
            void ReadUser() => Users.GetLoggedInUser().OnComplete(m =>
            {
                if (m.IsError)
                    done.TrySetException(Error("logged-in user", m));
                else
                    done.TrySetResult(m.Data.ID.ToString());
            });
            if (Core.IsInitialized())
            {
                ReadUser();
            }
            else
            {
                Core.AsyncInitialize().OnComplete(m =>
                {
                    if (m.IsError)
                        done.TrySetException(Error("initialization", m));
                    else
                        ReadUser();
                });
            }
            return done.Task;
        }

        public Task<string> GetUserProofAsync(CancellationToken ct)
        {
            var done = new TaskCompletionSource<string>();
            Users.GetUserProof().OnComplete(m =>
            {
                if (m.IsError)
                    done.TrySetException(Error("user proof", m));
                else
                    done.TrySetResult(m.Data.Value);
            });
            return done.Task;
        }

        public Task<IReadOnlyList<MetaQuestProduct>> GetProductsAsync(IReadOnlyList<string> skus, CancellationToken ct)
        {
            var done = new TaskCompletionSource<IReadOnlyList<MetaQuestProduct>>();
            IAP.GetProductsBySKU(skus.ToArray()).OnComplete(m =>
            {
                if (m.IsError)
                {
                    done.TrySetException(Error("products", m));
                    return;
                }
                done.TrySetResult(m.Data.Select(p => new MetaQuestProduct
                {
                    Sku = p.Sku, Type = TypeOf(p.Type), Name = p.Name, Description = p.Description, FormattedPrice = p.FormattedPrice,
                    // Price (amount in hundredths and currency) is in SDK versions that have it; older ones only format it.
                    AmountInHundredths = p.Price != null ? p.Price.AmountInHundredths : 0,
                    Currency = p.Price?.Currency,
                }).ToList());
            });
            return done.Task;
        }

        public Task<MetaQuestPurchaseResult> PurchaseAsync(string sku, CancellationToken ct)
        {
            var done = new TaskCompletionSource<MetaQuestPurchaseResult>();
            IAP.LaunchCheckoutFlow(sku).OnComplete(m =>
            {
                if (m.IsError)
                {
                    var message = m.GetError()?.Message ?? "";
                    // Newer SDKs report a closed checkout as an error with the user_canceled category.
                    var cancelled = message.IndexOf("cancel", StringComparison.OrdinalIgnoreCase) >= 0;
                    done.TrySetResult(new MetaQuestPurchaseResult { Cancelled = cancelled, Error = message });
                    return;
                }
                // Older SDKs answer a closed checkout with an empty purchase.
                var purchase = Map(m.Data);
                done.TrySetResult(purchase != null ? new MetaQuestPurchaseResult { Purchase = purchase } : new MetaQuestPurchaseResult { Cancelled = true });
            });
            return done.Task;
        }

        public Task<IReadOnlyList<MetaQuestPurchase>> GetPurchasesAsync(CancellationToken ct)
        {
            var done = new TaskCompletionSource<IReadOnlyList<MetaQuestPurchase>>();
            var all = new List<MetaQuestPurchase>();
            void Page(Message<PurchaseList> m)
            {
                if (m.IsError)
                {
                    done.TrySetException(Error("purchases", m));
                    return;
                }
                all.AddRange(m.Data.Select(Map).Where(p => p != null));
                if (m.Data.HasNextPage)
                    IAP.GetNextPurchaseListPage(m.Data).OnComplete(Page);
                else
                    done.TrySetResult(all);
            }
            IAP.GetViewerPurchases().OnComplete(Page);
            return done.Task;
        }

        public Task ConsumeAsync(string sku, CancellationToken ct)
        {
            var done = new TaskCompletionSource<bool>();
            IAP.ConsumePurchase(sku).OnComplete(m =>
            {
                if (m.IsError)
                    done.TrySetException(Error("consume", m));
                else
                    done.TrySetResult(true);
            });
            return done.Task;
        }

        private static BillingException Error(string what, Message m) =>
            new BillingException("Meta " + what + ": " + (m.GetError()?.Message ?? "failed"));

        private static MetaQuestPurchase Map(Purchase p) => p == null || string.IsNullOrEmpty(p.Sku) ? null : new MetaQuestPurchase
        {
            // A string in current SDKs, a number in older ones.
            Id = p.ID.ToString(), Sku = p.Sku, Type = TypeOf(p.Type), GrantTime = p.GrantTime.ToUniversalTime(),
        };

        private static MetaQuestProductType TypeOf(ProductType type)
        {
            switch (type)
            {
                case ProductType.DURABLE: return MetaQuestProductType.Durable;
                case ProductType.SUBSCRIPTION: return MetaQuestProductType.Subscription;
                default: return MetaQuestProductType.Consumable;
            }
        }
    }
}
#endif
