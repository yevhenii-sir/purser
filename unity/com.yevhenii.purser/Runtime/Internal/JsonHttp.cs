using System;
using System.Collections.Generic;
using System.Globalization;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;
using UnityEngine;

namespace Purser
{
    /// <summary>
    /// JSON POST to the billing server, shared by the API client and the checkout adapters: player tokens
    /// (<see cref="IPlayerTokenProvider"/>, one retry with a fresh token on <c>401 ERR_CLIENT_AUTH</c>), the error
    /// code and <c>Retry-After</c> of an answer, and an optional check of the raw answer (signed responses).
    /// </summary>
    internal sealed class JsonHttp
    {
        /// <summary>A <c>Retry-After</c> longer than this is cut to it.</summary>
        public static readonly TimeSpan MaxRetryAfter = TimeSpan.FromSeconds(60);

        private readonly string m_BaseUrl;
        private readonly int m_TimeoutSeconds;
        /// <summary>Set for an insecure URL outside the Editor: every request throws it.</summary>
        private readonly string m_Refusal;
        private PlayerTokenCache m_Tokens;

        public IHttpTransport Transport { get; }

        /// <summary>Player tokens for the requests; null sends none.</summary>
        public IPlayerTokenProvider Tokens
        {
            get => m_Tokens;
            set => m_Tokens = PlayerTokenCache.Of(value);
        }

        /// <param name="isEditor">Test hook; default <c>Application.isEditor</c>.</param>
        public JsonHttp(string baseUrl, int timeoutSeconds, IHttpTransport transport, bool? isEditor = null)
        {
            if (string.IsNullOrWhiteSpace(baseUrl))
                throw new ArgumentException("Billing server URL is empty", nameof(baseUrl));
            m_BaseUrl = baseUrl.TrimEnd('/');
            m_TimeoutSeconds = Math.Max(1, timeoutSeconds);
            Transport = transport ?? new UnityWebRequestTransport();
            m_Refusal = UrlPolicy.ServerUrlProblem(baseUrl, isEditor ?? Application.isEditor);
        }

        /// <summary>
        /// Sends <paramref name="body"/> as JSON for the player <paramref name="userId"/> (signed with a player token
        /// when <see cref="Tokens"/> is set and the user is known); a 4xx/5xx answer keeps its parsed body.
        /// <paramref name="check"/> sees every 2xx answer and returns why it must not be trusted (the answer then has no
        /// value and that error), or null; an answer it accepts keeps its signed bytes (<see cref="ApiResult{T}.RawBody"/>).
        /// </summary>
        /// <exception cref="BillingException">The server URL is not https outside the Editor.</exception>
        public async Task<ApiResult<T>> PostAsync<T>(string path, object body, string userId, CancellationToken ct,
            Func<HttpResponse, Task<string>> check = null) where T : class
        {
            ThrowIfRefused();
            var json = JsonConvert.SerializeObject(body);
            var tokens = m_Tokens;
            var user = string.IsNullOrEmpty(userId) ? null : userId;
            string token = null;
            if (tokens != null && user != null)
            {
                var (value, error) = await TokenAsync(tokens, user, ct);
                if (error != null)
                    return new ApiResult<T>(0, null, error);
                token = value;
            }
            var res = await SendAsync(path, json, token, ct);
            if (tokens != null && user != null && IsClientAuthFailure(res))
            {
                // The token expired or was revoked: ask the game backend once more and retry once.
                tokens.Invalidate(user, token);
                var (fresh, error) = await TokenAsync(tokens, user, ct);
                if (error != null)
                    return new ApiResult<T>(0, null, error);
                res = await SendAsync(path, json, fresh, ct);
            }
            return await ParseAsync<T>(res, check);
        }

        public async Task<HttpResponse> GetAsync(string path, CancellationToken ct)
        {
            ThrowIfRefused();
            return await Transport.SendAsync("GET", m_BaseUrl + path, null, null, m_TimeoutSeconds, ct);
        }

        private Task<HttpResponse> SendAsync(string path, string json, string token, CancellationToken ct)
        {
            var headers = string.IsNullOrEmpty(token)
                ? null
                : new Dictionary<string, string> { ["Authorization"] = "Bearer " + token };
            return Transport.SendAsync("POST", m_BaseUrl + path, json, headers, m_TimeoutSeconds, ct);
        }

        private static async Task<(string Token, string Error)> TokenAsync(PlayerTokenCache tokens, string user, CancellationToken ct)
        {
            try
            {
                return (await tokens.GetTokenAsync(user, ct), null);
            }
            catch (OperationCanceledException)
            {
                throw;
            }
            catch (Exception e)
            {
                // Reported like a network failure: the request is retried later.
                return (null, "player token unavailable: " + e.Message);
            }
        }

        private static bool IsClientAuthFailure(HttpResponse res) =>
            res.Status == 401 && ErrorCode(res.Body) == ErrorCodes.ClientAuth;

        private void ThrowIfRefused()
        {
            if (m_Refusal != null)
                throw new BillingException(m_Refusal);
        }

        private static async Task<ApiResult<T>> ParseAsync<T>(HttpResponse res, Func<HttpResponse, Task<string>> check) where T : class
        {
            if (res.IsTransportError)
                return new ApiResult<T>(0, null, res.Error);
            var retryAfter = res.Status == 429 ? RetryAfter(res.GetHeader("Retry-After")) : null;
            var code = res.Status >= 200 && res.Status < 300 ? null : ErrorCode(res.Body);
            var signature = check != null && res.Status >= 200 && res.Status < 300 ? res.GetHeader(ResponseSignature.Header) : null;
            if (check != null && res.Status >= 200 && res.Status < 300)
            {
                var problem = await check(res);
                if (problem != null)
                    return new ApiResult<T>(res.Status, null, problem, code, retryAfter);
            }
            T value = null;
            string error = null;
            if (!string.IsNullOrEmpty(res.Body))
            {
                try
                {
                    value = JsonConvert.DeserializeObject<T>(res.Body);
                }
                catch (JsonException e)
                {
                    error = "invalid JSON from server: " + e.Message;
                }
            }
            if (error == null && (res.Status < 200 || res.Status >= 300))
                error = $"HTTP {res.Status}: {Truncate(res.Body)}";
            var result = new ApiResult<T>(res.Status, value, error, code, retryAfter);
            if (!string.IsNullOrEmpty(signature) && value != null)
            {
                result.RawBody = res.RawBody;
                result.Signature = signature;
            }
            return result;
        }

        /// <summary>The <c>code</c> of an error body (<see cref="ErrorCodes"/>), or null.</summary>
        private static string ErrorCode(string body)
        {
            if (string.IsNullOrEmpty(body) || body.IndexOf("\"code\"", StringComparison.Ordinal) < 0)
                return null;
            try
            {
                return JsonConvert.DeserializeObject<StatusResponse>(body)?.Code;
            }
            catch (JsonException)
            {
                return null;
            }
        }

        /// <summary><c>Retry-After</c> in seconds (an HTTP date is not used by the server), capped at
        /// <see cref="MaxRetryAfter"/>; null when missing or unreadable.</summary>
        internal static TimeSpan? RetryAfter(string header)
        {
            if (string.IsNullOrWhiteSpace(header) ||
                !double.TryParse(header.Trim(), NumberStyles.Float, CultureInfo.InvariantCulture, out var seconds) || seconds < 0)
                return null;
            var wait = TimeSpan.FromSeconds(Math.Min(seconds, MaxRetryAfter.TotalSeconds));
            return wait;
        }

        private static string Truncate(string s) => s == null ? "" : s.Length <= 300 ? s : s.Substring(0, 300);
    }

    internal static class TaskTimeout
    {
        /// <summary>Awaits a store callback; throws <see cref="BillingException"/> after <paramref name="timeout"/>.</summary>
        /// <param name="delay">The timer (tests); default <c>Task.Delay</c>.</param>
        public static async Task<T> With<T>(Task<T> task, TimeSpan timeout, CancellationToken ct, string what,
            Func<TimeSpan, CancellationToken, Task> delay = null)
        {
            var (completed, value) = await Try(task, timeout, ct, delay);
            if (!completed)
                throw new BillingException($"Timed out waiting for {what}");
            return value;
        }

        /// <summary>
        /// The result of <paramref name="task"/> when it ends within <paramref name="timeout"/>: <c>(true, result)</c>;
        /// <c>(false, default)</c> when it takes longer (the task goes on, what it does later is ignored). Throws what
        /// the task throws, and <see cref="OperationCanceledException"/> when <paramref name="ct"/> is cancelled first.
        /// </summary>
        /// <param name="delay">The timer (tests); default <c>Task.Delay</c>.</param>
        public static async Task<(bool Completed, T Value)> Try<T>(Task<T> task, TimeSpan timeout, CancellationToken ct,
            Func<TimeSpan, CancellationToken, Task> delay = null)
        {
            if (!task.IsCompleted)
            {
                using (var cts = CancellationTokenSource.CreateLinkedTokenSource(ct))
                {
                    var timer = delay != null ? delay(timeout, cts.Token) : Task.Delay(timeout, cts.Token);
                    var done = await Task.WhenAny(task, timer);
                    cts.Cancel();
                    if (done != task)
                    {
                        ct.ThrowIfCancellationRequested();
                        // Nobody awaits it any more: a late failure must not surface as an unobserved exception.
                        _ = task.ContinueWith(t => t.Exception, CancellationToken.None,
                            TaskContinuationOptions.OnlyOnFaulted | TaskContinuationOptions.ExecuteSynchronously, TaskScheduler.Default);
                        return (false, default);
                    }
                }
            }
            return (true, await task);
        }
    }
}
