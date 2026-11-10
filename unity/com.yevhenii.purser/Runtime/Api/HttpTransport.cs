using System;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using UnityEngine.Networking;

namespace Purser
{
    public readonly struct HttpResponse
    {
        /// <summary>HTTP status; 0 when the request did not reach the server.</summary>
        public readonly int Status;
        public readonly string Body;
        /// <summary>Transport error (DNS, timeout, no network); null when a response arrived.</summary>
        public readonly string Error;

        public HttpResponse(int status, string body, string error = null)
        {
            Status = status;
            Body = body;
            Error = error;
        }

        public bool IsTransportError => Status == 0;
    }

    /// <summary>HTTP abstraction so the API client can be tested without the network.</summary>
    public interface IHttpTransport
    {
        Task<HttpResponse> SendAsync(string method, string url, string jsonBody, int timeoutSeconds, CancellationToken ct);
    }

    /// <summary>Default transport. Must be used from the Unity main thread.</summary>
    public sealed class UnityWebRequestTransport : IHttpTransport
    {
        public async Task<HttpResponse> SendAsync(string method, string url, string jsonBody, int timeoutSeconds, CancellationToken ct)
        {
            using (var request = new UnityWebRequest(url, method))
            {
                if (jsonBody != null)
                {
                    request.uploadHandler = new UploadHandlerRaw(Encoding.UTF8.GetBytes(jsonBody));
                    request.SetRequestHeader("Content-Type", "application/json");
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

                var body = request.downloadHandler?.text;
                if (request.result == UnityWebRequest.Result.ConnectionError ||
                    request.result == UnityWebRequest.Result.DataProcessingError)
                    return new HttpResponse(0, body, request.error ?? "connection error");
                return new HttpResponse((int)request.responseCode, body);
            }
        }
    }
}
