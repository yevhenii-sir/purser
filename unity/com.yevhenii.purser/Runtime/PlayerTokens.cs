using System;
using System.Collections.Generic;
using System.Threading;
using System.Threading.Tasks;

namespace Purser
{
    /// <summary>
    /// Player tokens for builds whose client requests must be signed by the game's own backend: an HS256 JWT for the
    /// player, made by the game backend with the build's client key (the SDK treats it as an opaque string). When a
    /// provider is set (<see cref="BillingDependencies.PlayerTokenProvider"/>), every client request
    /// (<c>/verify/*</c>, <c>/restore</c>, <c>/entitlements</c>, <c>/attributes</c>, <c>/paywall</c>,
    /// <c>/paywall/event</c>, <c>/offers/apple/sign</c>, <c>/checkout/*</c>) carries <c>Authorization: Bearer &lt;token&gt;</c>.
    /// The token is reused until the server refuses it (<c>401 ERR_CLIENT_AUTH</c>); then the provider is asked once
    /// more and the request is sent again once.
    /// </summary>
    public interface IPlayerTokenProvider
    {
        /// <summary>A token for <paramref name="userId"/> from the game backend; null or empty when there is none (the
        /// request goes without one).</summary>
        Task<string> GetTokenAsync(string userId, CancellationToken ct);
    }

    /// <summary>Player tokens from a delegate, e.g. <c>(user, ct) => backend.GetBillingTokenAsync(user, ct)</c>.</summary>
    public sealed class FuncPlayerTokenProvider : IPlayerTokenProvider
    {
        private readonly Func<string, CancellationToken, Task<string>> m_Get;

        /// <summary>Asks <paramref name="get"/> for each token.</summary>
        public FuncPlayerTokenProvider(Func<string, CancellationToken, Task<string>> get)
        {
            m_Get = get ?? throw new ArgumentNullException(nameof(get));
        }

        /// <inheritdoc/>
        public Task<string> GetTokenAsync(string userId, CancellationToken ct) => m_Get(userId, ct);
    }

    /// <summary>
    /// Something that calls the billing server itself and can sign its requests with player tokens:
    /// <see cref="BillingApiClient"/> and the server-checkout store adapters. <see cref="BillingService"/> hands its
    /// <see cref="BillingDependencies.PlayerTokenProvider"/> to its API client and store adapter when they implement it.
    /// </summary>
    public interface IPlayerTokenConsumer
    {
        /// <summary>Signs the following requests with tokens of <paramref name="provider"/>; null stops signing.</summary>
        void SetPlayerTokenProvider(IPlayerTokenProvider provider);
    }

    /// <summary>Keeps the last token per player until the server refuses it (shared by every consumer of a service).</summary>
    internal sealed class PlayerTokenCache : IPlayerTokenProvider
    {
        private readonly IPlayerTokenProvider m_Provider;
        private readonly Dictionary<string, string> m_Tokens = new Dictionary<string, string>();

        public PlayerTokenCache(IPlayerTokenProvider provider)
        {
            m_Provider = provider ?? throw new ArgumentNullException(nameof(provider));
        }

        /// <summary>The provider's token, wrapped in a cache unless it already is one; null for null.</summary>
        public static PlayerTokenCache Of(IPlayerTokenProvider provider) =>
            provider == null ? null : provider as PlayerTokenCache ?? new PlayerTokenCache(provider);

        public async Task<string> GetTokenAsync(string userId, CancellationToken ct)
        {
            if (string.IsNullOrEmpty(userId))
                return null;
            if (m_Tokens.TryGetValue(userId, out var cached))
                return cached;
            var token = await m_Provider.GetTokenAsync(userId, ct);
            if (string.IsNullOrEmpty(token))
                return null;
            m_Tokens[userId] = token;
            return token;
        }

        /// <summary>The server refused <paramref name="token"/>: the next request asks the provider again.</summary>
        public void Invalidate(string userId, string token)
        {
            if (userId != null && m_Tokens.TryGetValue(userId, out var cached) && cached == token)
                m_Tokens.Remove(userId);
        }
    }
}
