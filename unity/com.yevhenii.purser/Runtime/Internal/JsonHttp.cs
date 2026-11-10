using System;
using System.Threading;
using System.Threading.Tasks;
using Newtonsoft.Json;

namespace Purser
{
    /// <summary>JSON POST to the billing server, shared by the API client and the checkout adapters.</summary>
    internal sealed class JsonHttp
    {
        private readonly string m_BaseUrl;
        private readonly int m_TimeoutSeconds;

        public IHttpTransport Transport { get; }

        public JsonHttp(string baseUrl, int timeoutSeconds, IHttpTransport transport)
        {
            if (string.IsNullOrWhiteSpace(baseUrl))
                throw new ArgumentException("Billing server URL is empty", nameof(baseUrl));
            m_BaseUrl = baseUrl.TrimEnd('/');
            m_TimeoutSeconds = Math.Max(1, timeoutSeconds);
            Transport = transport ?? new UnityWebRequestTransport();
        }

        /// <summary>Sends <paramref name="body"/> as JSON; a 4xx/5xx answer keeps its parsed body.</summary>
        public async Task<ApiResult<T>> PostAsync<T>(string path, object body, CancellationToken ct) where T : class
        {
            var res = await Transport.SendAsync("POST", m_BaseUrl + path, JsonConvert.SerializeObject(body), m_TimeoutSeconds, ct);
            return Parse<T>(res);
        }

        public async Task<HttpResponse> GetAsync(string path, CancellationToken ct) =>
            await Transport.SendAsync("GET", m_BaseUrl + path, null, m_TimeoutSeconds, ct);

        private static ApiResult<T> Parse<T>(HttpResponse res) where T : class
        {
            if (res.IsTransportError)
                return new ApiResult<T>(0, null, res.Error);
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
            return new ApiResult<T>(res.Status, value, error);
        }

        private static string Truncate(string s) => s == null ? "" : s.Length <= 300 ? s : s.Substring(0, 300);
    }

    internal static class TaskTimeout
    {
        /// <summary>Awaits a store callback; throws <see cref="BillingException"/> after <paramref name="timeout"/>.</summary>
        public static async Task<T> With<T>(Task<T> task, TimeSpan timeout, CancellationToken ct, string what)
        {
            using (var cts = CancellationTokenSource.CreateLinkedTokenSource(ct))
            {
                var delay = Task.Delay(timeout, cts.Token);
                var done = await Task.WhenAny(task, delay);
                cts.Cancel();
                if (done != task)
                {
                    ct.ThrowIfCancellationRequested();
                    throw new BillingException($"Timed out waiting for {what}");
                }
                return await task;
            }
        }
    }
}
