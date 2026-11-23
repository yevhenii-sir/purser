using System;
using System.Collections.Generic;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using UnityEngine.Networking;

namespace Purser
{
    /// <summary>What an <see cref="IHttpTransport"/> got back.</summary>
    public readonly struct HttpResponse
    {
        /// <summary>HTTP status; 0 when the request did not reach the server.</summary>
        public readonly int Status;

        /// <summary>Response body as text; may be null.</summary>
        public readonly string Body;

        /// <summary>The exact response body bytes (signed responses are checked over them); the UTF-8 bytes of
        /// <see cref="Body"/> when the transport gave none.</summary>
        public readonly byte[] RawBody;

        /// <summary>Response headers; null or empty when there are none. Use <see cref="GetHeader"/> (names are
        /// case-insensitive).</summary>
        public readonly IReadOnlyDictionary<string, string> Headers;

        /// <summary>Transport error (DNS, timeout, no network); null when a response arrived.</summary>
        public readonly string Error;

        /// <summary>A response with this status and body; <paramref name="error"/> for a transport failure (status 0).</summary>
        public HttpResponse(int status, string body, string error = null, IReadOnlyDictionary<string, string> headers = null,
            byte[] rawBody = null)
        {
            Status = status;
            Body = body;
            Error = error;
            Headers = headers;
            RawBody = rawBody ?? (body == null ? null : Encoding.UTF8.GetBytes(body));
        }

        /// <summary>The server was not reached (<see cref="Status"/> is 0).</summary>
        public bool IsTransportError => Status == 0;

        /// <summary>The value of a response header (case-insensitive name), or null.</summary>
        public string GetHeader(string name)
        {
            if (Headers == null || string.IsNullOrEmpty(name))
                return null;
            if (Headers.TryGetValue(name, out var exact))
                return exact;
            foreach (var kv in Headers)
                if (string.Equals(kv.Key, name, StringComparison.OrdinalIgnoreCase))
                    return kv.Value;
            return null;
        }
    }

    /// <summary>HTTP abstraction so the API client can be tested without the network.</summary>
    public interface IHttpTransport
    {
        /// <summary>Sends a request with an optional JSON body (<c>Content-Type: application/json</c>) and extra request
        /// <paramref name="headers"/> (may be null, e.g. <c>Authorization</c>). Network failures are reported as
        /// <see cref="HttpResponse.IsTransportError"/>, not thrown. The response keeps its headers and raw body.</summary>
        Task<HttpResponse> SendAsync(string method, string url, string jsonBody, IReadOnlyDictionary<string, string> headers,
            int timeoutSeconds, CancellationToken ct);
    }

    /// <summary>Default transport. Must be used from the Unity main thread.</summary>
    public sealed class UnityWebRequestTransport : IHttpTransport
    {
        /// <inheritdoc/>
        public Task<HttpResponse> SendAsync(string method, string url, string jsonBody, IReadOnlyDictionary<string, string> headers,
            int timeoutSeconds, CancellationToken ct) =>
            Send(method, url, jsonBody, "application/json", headers, timeoutSeconds, ct);

        /// <summary>Sends <paramref name="body"/> (when not null) with <paramref name="contentType"/>.</summary>
        internal static async Task<HttpResponse> Send(string method, string url, string body, string contentType,
            IReadOnlyDictionary<string, string> headers, int timeoutSeconds, CancellationToken ct)
        {
            using (var request = new UnityWebRequest(url, method))
            {
                if (body != null)
                {
                    request.uploadHandler = new UploadHandlerRaw(Encoding.UTF8.GetBytes(body));
                    request.SetRequestHeader("Content-Type", contentType);
                }
                if (headers != null)
                    foreach (var header in headers)
                        if (!string.IsNullOrEmpty(header.Key) && header.Value != null)
                            request.SetRequestHeader(header.Key, header.Value);
                request.downloadHandler = new DownloadHandlerBuffer();
                request.timeout = timeoutSeconds;

                var tcs = new TaskCompletionSource<bool>();
                var operation = request.SendWebRequest();
                operation.completed += _ => tcs.TrySetResult(true);
                // The token may be cancelled on another thread, and a UnityWebRequest is touched on the main thread
                // only: the abort is posted there. By then the request may have ended and been disposed.
                var mainThread = SynchronizationContext.Current;
                var ended = false;
                using (ct.Register(() => MainThreadPost.Run(mainThread, () =>
                       {
                           if (!ended)
                               request.Abort();
                           tcs.TrySetCanceled();
                       })))
                {
                    try
                    {
                        await tcs.Task;
                    }
                    finally
                    {
                        ended = true;
                    }
                }

                var raw = request.downloadHandler?.data;
                var text = raw == null ? request.downloadHandler?.text : Encoding.UTF8.GetString(raw);
                if (request.result == UnityWebRequest.Result.ConnectionError ||
                    request.result == UnityWebRequest.Result.DataProcessingError)
                    return new HttpResponse(0, text, request.error ?? "connection error");
                return new HttpResponse((int)request.responseCode, text, null, request.GetResponseHeaders(), raw);
            }
        }
    }
}
