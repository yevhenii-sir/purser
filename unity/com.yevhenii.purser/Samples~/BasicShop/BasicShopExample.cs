using System;
using System.Linq;
using Purser;
using UnityEngine;

/// <summary>
/// Minimal shop on IMGUI. Put a BillingBootstrap (Tools → Purser → Add Bootstrap To Scene) with a
/// BillingConfig in the scene, add this component anywhere and enter the player id.
/// In the Editor purchases go through the Unity IAP fake store and are confirmed locally (editorMode = SkipServer);
/// the products' access levels (BillingProductConfig.access) are granted locally too.
/// </summary>
public sealed class BasicShopExample : MonoBehaviour
{
    [SerializeField] private string userId = "player-1";

    [Tooltip("Access level the shop shows (configured per product on the server and in the config's products).")]
    [SerializeField] private string accessLevel = "premium";

    private IBillingService m_Billing;
    private string m_Status = "Initializing...";

    private void Start()
    {
        var bootstrap = BillingBootstrap.Current;
        if (bootstrap == null || bootstrap.Service == null)
        {
            m_Status = "Add a BillingBootstrap with a BillingConfig to the scene.";
            return;
        }
        m_Billing = bootstrap.Service;
        m_Billing.SetUserId(userId);
        m_Billing.Initialized += OnInitialized;
        m_Billing.PurchaseSucceeded += OnPurchaseSucceeded;
        m_Billing.PurchasePending += OnPurchasePending;
        m_Billing.PurchaseFailed += OnPurchaseFailed;
        m_Billing.CustomerInfoChanged += OnCustomerInfoChanged;
        m_Billing.StoreDisconnected += OnStoreDisconnected;
        m_Billing.StoreReconnected += OnInitialized;
        if (m_Billing.IsInitialized)
            m_Status = "Ready";
    }

    private void OnDestroy()
    {
        if (m_Billing == null)
            return;
        m_Billing.Initialized -= OnInitialized;
        m_Billing.PurchaseSucceeded -= OnPurchaseSucceeded;
        m_Billing.PurchasePending -= OnPurchasePending;
        m_Billing.PurchaseFailed -= OnPurchaseFailed;
        m_Billing.CustomerInfoChanged -= OnCustomerInfoChanged;
        m_Billing.StoreDisconnected -= OnStoreDisconnected;
        m_Billing.StoreReconnected -= OnInitialized;
    }

    private void OnInitialized() => m_Status = "Ready";
    private void OnPurchaseSucceeded(PurchaseResult r) => m_Status = $"Bought {r.ProductId} ({r.Status})";
    private void OnPurchasePending(PurchaseResult r) => m_Status = $"Pending {r.ProductId}: {r.Message}";
    private void OnPurchaseFailed(PurchaseResult r) => m_Status = $"Failed {r.ProductId}: {r.Status} {r.Message}";
    private void OnCustomerInfoChanged(CustomerInfo info) => Debug.Log($"[BasicShop] Access: {string.Join(", ", info.Access.Keys)}");
    private void OnStoreDisconnected(string reason) => m_Status = "Store disconnected, reconnecting...";

    private async void Buy(string productId)
    {
        m_Status = "Buying " + productId + "...";
        try
        {
            await m_Billing.PurchaseAsync(productId);
        }
        catch (Exception e)
        {
            m_Status = "Purchase error: " + e.Message;
        }
    }

    private async void Restore()
    {
        m_Status = "Restoring...";
        try
        {
            var info = await m_Billing.RestoreAsync();
            m_Status = $"Restored: {info.NonConsumables.Count} items, {info.Subscriptions.Count} subscriptions, " +
                       $"access: {(info.Access.Count > 0 ? string.Join(", ", info.Access.Keys) : "none")}";
        }
        catch (Exception e)
        {
            m_Status = "Restore failed: " + e.Message;
        }
    }

    private void OnGUI()
    {
        GUILayout.BeginArea(new Rect(20, 20, 420, Screen.height - 40));
        GUILayout.Label(m_Status);
        if (m_Billing != null && m_Billing.IsInitialized)
        {
            foreach (var product in m_Billing.Products)
            {
                var owned = m_Billing.IsActive(product.Id) ? " ✓" : "";
                var trial = product.IntroEligible == true && product.Offers.Any(o => o.HasFreeTrial) ? " (free trial)" : "";
                if (GUILayout.Button($"{product.Title} — {product.LocalizedPrice}{trial}{owned}"))
                    Buy(product.Id);
            }
            if (GUILayout.Button("Restore purchases"))
                Restore();
            GUILayout.Label($"Access '{accessLevel}': " + (m_Billing.HasAccess(accessLevel) ? "yes" : "no"));
            var subs = string.Join(", ", m_Billing.CustomerInfo.Subscriptions.Keys.ToArray());
            GUILayout.Label("Subscriptions: " + (subs.Length > 0 ? subs : "none"));
        }
        GUILayout.EndArea();
    }
}
