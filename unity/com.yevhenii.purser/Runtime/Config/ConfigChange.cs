namespace Purser
{
    public enum ConfigChangeKind
    {
        /// <summary>A build or product the config did not have.</summary>
        Added,
        /// <summary>A value of an existing build or product (or the server URL) changes.</summary>
        Updated,
        /// <summary>A build or product the admin panel does not have (<see cref="ConfigMergeMode.Mirror"/> only).</summary>
        Removed,
    }

    /// <summary>How <see cref="BillingConfig.MergeJson"/> treats what the config has and the JSON does not.</summary>
    public enum ConfigMergeMode
    {
        /// <summary>Adds and updates; keeps builds, products and store ids only this config has.</summary>
        Additive,
        /// <summary>Makes the builds and products match the JSON: also removes those it does not list.</summary>
        Mirror,
    }

    /// <summary>One change of <see cref="BillingConfig.MergeJson"/> / <see cref="BillingConfig.PreviewJson"/>.</summary>
    public sealed class ConfigChange
    {
        public ConfigChangeKind Kind { get; }

        /// <summary>What changes, e.g. <c>Build apple</c> or <c>Product gems_100</c>.</summary>
        public string Target { get; }

        /// <summary>How, e.g. <c>type Consumable → Subscription; price none → 0.99 USD</c>.</summary>
        public string Details { get; }

        public ConfigChange(ConfigChangeKind kind, string target, string details)
        {
            Kind = kind;
            Target = target;
            Details = details;
        }

        public override string ToString() =>
            $"{(Kind == ConfigChangeKind.Added ? "+" : Kind == ConfigChangeKind.Removed ? "-" : "~")} {Target}: {Details}";
    }
}
