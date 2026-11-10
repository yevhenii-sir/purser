using System.Linq;
using Purser;
using UnityEngine;

/// <summary>
/// Minimal shop on IMGUI. Put a BillingBootstrap (Tools → Purser → Add Bootstrap To Scene) with a
/// BillingConfig in the scene, add this component anywhere and enter the player id.
/// In the Editor purchases go through the Unity IAP fake store and are confirmed locally (editorMode = SkipServer).
/// </summary>
public sealed class BasicShopExample : MonoBehaviour
{
    [SerializeField] private string userId = "player-1";

    private IBillingService m_Billing;
    private string m_Status = "Initializing...";

    private void Start()
    {
        var bootstrap = BillingBootstrap.Current;
        if (bootstrap == null)
        {
            m_Status = "Add a BillingBootstrap to the scene.";
            return;
        }
        m_Billing = bootstrap.Service;
        m_Billing.SetUserId(userId);
        m_Billing.Initialized += () => m_Status = "Ready";
        m_Billing.PurchaseSucceeded += r => m_Status = $"Bought {r.ProductId} ({r.Status})";
        m_Billing.PurchasePending += r => m_Status = $"Pending {r.ProductId}: {r.Message}";
        m_Billing.PurchaseFailed += r => m_Status = $"Failed {r.ProductId}: {r.Status} {r.Message}";
        if (m_Billing.IsInitialized)
            m_Status = "Ready";
    }

    private async void Buy(string productId)
    {
        m_Status = "Buying " + productId + "...";
        await m_Billing.PurchaseAsync(productId);
    }

    private async void Restore()
    {
        m_Status = "Restoring...";
        var entitlements = await m_Billing.RestoreAsync();
        m_Status = $"Restored: {entitlements.NonConsumables.Count} items, {entitlements.Subscriptions.Count} subscriptions";
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
                if (GUILayout.Button($"{product.Title} — {product.LocalizedPrice}{owned}"))
                    Buy(product.Id);
            }
            if (GUILayout.Button("Restore purchases"))
                Restore();
            var subs = string.Join(", ", m_Billing.Entitlements.Subscriptions.Keys.ToArray());
            GUILayout.Label("Subscriptions: " + (subs.Length > 0 ? subs : "none"));
        }
        GUILayout.EndArea();
    }
}
