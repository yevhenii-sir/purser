using System;
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

        /// <summary>Transport error (DNS, timeout, no network); null when a response arrived.</summary>
        public readonly string Error;

        /// <summary>A response with this status and body; <paramref name="error"/> for a transport failure (status 0).</summary>
        public HttpResponse(int status, string body, string error = null)
        {
            Status = status;
            Body = body;
            Error = error;
        }

        /// <summary>The server was not reached (<see cref="Status"/> is 0).</summary>
        public bool IsTransportError => Status == 0;
    }

    /// <summary>HTTP abstraction so the API client can be tested without the network.</summary>
    public interface IHttpTransport
    {
        /// <summary>Sends a request with an optional JSON body (<c>Content-Type: application/json</c>). Network
        /// failures are reported as <see cref="HttpResponse.IsTransportError"/>, not thrown.</summary>
        Task<HttpResponse> SendAsync(string method, string url, string jsonBody, int timeoutSeconds, CancellationToken ct);
    }

    /// <summary>Default transport. Must be used from the Unity main thread.</summary>
    public sealed class UnityWebRequestTransport : IHttpTransport
    {
        /// <inheritdoc/>
        public Task<HttpResponse> SendAsync(string method, string url, string jsonBody, int timeoutSeconds, CancellationToken ct) =>
            Send(method, url, jsonBody, "application/json", timeoutSeconds, ct);

        /// <summary>Sends <paramref name="body"/> (when not null) with <paramref name="contentType"/>.</summary>
        internal static async Task<HttpResponse> Send(string method, string url, string body, string contentType,
            int timeoutSeconds, CancellationToken ct)
        {
            using (var request = new UnityWebRequest(url, method))
            {
                if (body != null)
                {
                    request.uploadHandler = new UploadHandlerRaw(Encoding.UTF8.GetBytes(body));
                    request.SetRequestHeader("Content-Type", contentType);
                }
                request.downloadHandler = new DownloadHandlerBuffer();
                request.timeout = timeoutSeconds;

                var tcs = new TaskCompletionSource<bool>();
                var operation = request.SendWebRequest();
                operation.completed += _ => tcs.TrySetResult(true);
                using (ct.Register(() =>
                       {
                           request.Abort();
                           tcs.TrySetCanceled();
                       }))
                {
                    await tcs.Task;
                }

                var text = request.downloadHandler?.text;
                if (request.result == UnityWebRequest.Result.ConnectionError ||
                    request.result == UnityWebRequest.Result.DataProcessingError)
                    return new HttpResponse(0, text, request.error ?? "connection error");
                return new HttpResponse((int)request.responseCode, text);
            }
        }
    }
}
