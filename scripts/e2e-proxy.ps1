# A small web server on this computer for the live scripts. The app is pointed at it (TSS_API_BASE and TSS_RAW_BASE);
# it passes every request on to GitHub, writes down what GitHub answered, and, when told to, misbehaves the way a bad
# network, a bad token or a bad day at GitHub would. Dot-source it after e2e-lib.ps1.
#
# It never writes down the token: the log only says whether a request carried one.
# Works in Windows PowerShell 5.1 and PowerShell 7. Listens on 127.0.0.1 only, so nothing else can reach it.

Add-Type -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.IO;
using System.Net;
using System.Text;
using System.Threading;

public sealed class TssProxy {
    readonly HttpListener listener = new HttpListener();
    readonly Dictionary<string, string> targets = new Dictionary<string, string>();
    readonly Dictionary<string, string> modes = new Dictionary<string, string>();
    readonly List<string> log = new List<string>();
    readonly object gate = new object();
    readonly int port;

    public TssProxy(int port) {
        this.port = port;
        targets["api"] = "https://api.github.com";
        targets["raw"] = "https://raw.githubusercontent.com";
        modes["api"] = "pass";
        modes["raw"] = "pass";
        listener.Prefixes.Add("http://127.0.0.1:" + port + "/");
    }

    public int Port { get { return port; } }
    public string ApiBase { get { return "http://127.0.0.1:" + port + "/api"; } }
    public string RawBase { get { return "http://127.0.0.1:" + port + "/raw"; } }

    public void Start() {
        listener.Start();
        Thread t = new Thread(Loop);
        t.IsBackground = true;
        t.Start();
    }

    public void Stop() {
        try { listener.Stop(); listener.Close(); } catch (Exception) { }
    }

    // route: "api", "raw" or "both". mode: pass | close | hang[:MS] | slow:MS | 401 | 403 | limit | 429 | 404 | 500 | 503
    // | garbage | redirect | empty-tree[:FOLDER] | truncated | corrupt | badauth | freeze[:TEXT]
    public void SetMode(string route, string mode) {
        lock (gate) {
            if (route == "both" || route == "api") modes["api"] = mode;
            if (route == "both" || route == "raw") modes["raw"] = mode;
        }
    }

    public string[] Entries() { lock (gate) { return log.ToArray(); } }
    public void Clear() { lock (gate) { log.Clear(); } }

    void Loop() {
        while (listener.IsListening) {
            HttpListenerContext ctx;
            try { ctx = listener.GetContext(); } catch (Exception) { return; }
            // Nothing a request does may take the whole script down with it.
            ThreadPool.QueueUserWorkItem(delegate (object o) { try { Handle((HttpListenerContext)o); } catch (Exception) { } }, ctx);
        }
    }

    void Handle(HttpListenerContext ctx) {
        DateTime started = DateTime.Now;
        HttpListenerRequest rq = ctx.Request;
        string url = rq.RawUrl;
        // Read before anything is answered: once the connection is gone the request can no longer be asked.
        string inm = rq.Headers["If-None-Match"];
        bool hasAuth = rq.Headers["Authorization"] != null;
        string method = rq.HttpMethod;
        string route = "";
        string rest = "";
        int slash = url.Length > 1 ? url.IndexOf('/', 1) : -1;
        if (slash > 0) { route = url.Substring(1, slash - 1); rest = url.Substring(slash); }
        string mode;
        lock (gate) { mode = modes.ContainsKey(route) ? modes[route] : "pass"; }
        string outcome;
        try {
            if (!targets.ContainsKey(route)) outcome = Reply(ctx, 404, "{\"message\":\"unknown route\"}", null, "application/json");
            else if (method != "GET") outcome = Reply(ctx, 405, "{}", null, "application/json");
            else outcome = Respond(ctx, targets[route], rest, mode);
        } catch (Exception e) {
            outcome = "failed: " + e.GetType().Name + " " + e.Message;
            try { ctx.Response.Abort(); } catch (Exception) { }
        }
        string line = string.Format("{0:HH:mm:ss.fff} {1,-3} {2} [{3}] -> {4} | inm={5} auth={6} {7} ms",
            started, route, rest.Length > 110 ? rest.Substring(0, 110) + "..." : rest, mode, outcome,
            inm == null ? "-" : (inm.Length > 14 ? inm.Substring(0, 14) : inm),
            hasAuth ? "yes" : "no",
            (int)(DateTime.Now - started).TotalMilliseconds);
        lock (gate) { log.Add(line); }
    }

    string Respond(HttpListenerContext ctx, string target, string rest, string mode) {
        string name = mode;
        string arg = "";
        int colon = mode.IndexOf(':');
        if (colon > 0) { name = mode.Substring(0, colon); arg = mode.Substring(colon + 1); }
        switch (name) {
            case "close":
                ctx.Response.Abort();
                return "closed without an answer";
            case "hang": {
                int ms = arg.Length > 0 ? int.Parse(arg) : 120000;
                DateTime until = DateTime.UtcNow.AddMilliseconds(ms);
                while (DateTime.UtcNow < until && listener.IsListening) Thread.Sleep(200);
                ctx.Response.Abort();
                return "no answer, then closed";
            }
            case "slow":
                Thread.Sleep(int.Parse(arg));
                break;
            case "freeze":
                return Freeze(ctx, target, rest, arg);
            case "401": return Reply(ctx, 401, "{\"message\":\"Bad credentials\"}", null, "application/json");
            case "403": return Reply(ctx, 403, "{\"message\":\"Resource not accessible by personal access token\"}", null, "application/json");
            case "limit":
                return Reply(ctx, 403, "{\"message\":\"API rate limit exceeded\"}",
                    new string[] { "X-RateLimit-Remaining: 0", "X-RateLimit-Reset: " + ((long)(DateTime.UtcNow - new DateTime(1970, 1, 1)).TotalSeconds + 120) }, "application/json");
            case "429": return Reply(ctx, 429, "{\"message\":\"Too many requests\"}", new string[] { "Retry-After: 120" }, "application/json");
            case "404": return Reply(ctx, 404, "{\"message\":\"Not Found\"}", null, "application/json");
            case "500": return Reply(ctx, 500, "{\"message\":\"Server Error\"}", null, "application/json");
            case "503": return Reply(ctx, 503, "<html><body>Service unavailable</body></html>", null, "text/html");
            case "garbage": return Reply(ctx, 200, "<html><body>This is not what an API says.</body></html>", null, "text/html");
            case "redirect": return Reply(ctx, 301, "{}", new string[] { "Location: https://api.github.com/repositories/1/git/trees/x" }, "application/json");
            case "empty-tree":
                // empty-tree:FOLDER keeps FOLDER (and the folders above it), with nothing in it.
                if (rest.Contains("/git/trees/")) {
                    StringBuilder entries = new StringBuilder();
                    string path = "";
                    foreach (string part in arg.Split(new char[] { '/' }, StringSplitOptions.RemoveEmptyEntries)) {
                        path = path.Length == 0 ? part : path + "/" + part;
                        if (entries.Length > 0) entries.Append(',');
                        entries.Append("{\"path\":\"" + path + "\",\"mode\":\"040000\",\"type\":\"tree\",\"sha\":\"4b825dc642cb6eb9a060e54bf8d69288fbee4904\",\"url\":\"\"}");
                    }
                    return Reply(ctx, 200, "{\"sha\":\"4b825dc642cb6eb9a060e54bf8d69288fbee4904\",\"url\":\"\",\"tree\":[" + entries + "],\"truncated\":false}",
                        new string[] { "ETag: W/\"tss-empty" + (arg.Length > 0 ? "-" + arg.Replace('"', '_') : "") + "\"" }, "application/json");
                }
                break;
        }
        return Forward(ctx, target, rest, name);
    }

    string Reply(HttpListenerContext ctx, int status, string body, string[] headers, string contentType) {
        HttpListenerResponse o = ctx.Response;
        o.StatusCode = status;
        o.ContentType = contentType;
        if (headers != null) {
            foreach (string h in headers) {
                int i = h.IndexOf(':');
                string key = h.Substring(0, i);
                string value = h.Substring(i + 1).Trim();
                if (key == "Location") o.RedirectLocation = value; else o.Headers[key] = value;
            }
        }
        byte[] bytes = Encoding.UTF8.GetBytes(body);
        o.ContentLength64 = bytes.Length;
        o.OutputStream.Write(bytes, 0, bytes.Length);
        o.OutputStream.Close();
        return status + " (made up)";
    }

    static string Mangle(string authorization) {
        // Keeps the shape of the header and ruins the secret, so GitHub answers "bad credentials".
        return authorization.Length > 8 ? authorization.Substring(0, authorization.Length - 4) + "xxxx" : "Bearer xxxx";
    }

    // What GitHub answered, kept whole so it can be passed on, spoiled, or played again later.
    sealed class Answer {
        public int Status;
        public string ContentType;
        public List<string[]> Headers = new List<string[]>();
        public byte[] Body;
        public string Note;
    }

    readonly Dictionary<string, Answer> frozen = new Dictionary<string, Answer>();

    // Forgets the answers kept by the "freeze" mode.
    public void Thaw() { lock (gate) { frozen.Clear(); } }

    string Forward(HttpListenerContext ctx, string target, string rest, string name) {
        return Emit(ctx, Fetch(ctx, target, rest, name), name);
    }

    // freeze:TEXT answers requests whose address contains TEXT with the first answer GitHub gave to the same address,
    // however much has changed since: a cache that never lets go.
    string Freeze(HttpListenerContext ctx, string target, string rest, string text) {
        bool match = text.Length == 0 || rest.Contains(text);
        Answer kept = null;
        if (match) { lock (gate) { frozen.TryGetValue(rest, out kept); } }
        if (kept != null) return Emit(ctx, kept, "freeze") + " REPLAYED AN EARLIER ANSWER";
        Answer fresh = Fetch(ctx, target, rest, "pass");
        if (match && fresh.Status == 200) { lock (gate) { frozen[rest] = fresh; } }
        return Emit(ctx, fresh, "pass");
    }

    Answer Fetch(HttpListenerContext ctx, string target, string rest, string name) {
        HttpListenerRequest rq = ctx.Request;
        HttpWebRequest fw = (HttpWebRequest)WebRequest.Create(target + rest);
        fw.Method = "GET";
        fw.AllowAutoRedirect = false;
        fw.Timeout = 60000;
        fw.ReadWriteTimeout = 60000;
        fw.AutomaticDecompression = DecompressionMethods.GZip | DecompressionMethods.Deflate;
        fw.UserAgent = rq.UserAgent == null ? "tss-proxy" : rq.UserAgent;
        if (rq.Headers["Accept"] != null) fw.Accept = rq.Headers["Accept"];
        if (rq.Headers["If-None-Match"] != null) fw.Headers["If-None-Match"] = rq.Headers["If-None-Match"];
        if (rq.Headers["X-GitHub-Api-Version"] != null) fw.Headers["X-GitHub-Api-Version"] = rq.Headers["X-GitHub-Api-Version"];
        string auth = rq.Headers["Authorization"];
        if (auth != null) fw.Headers["Authorization"] = name == "badauth" ? Mangle(auth) : auth;

        HttpWebResponse resp;
        try { resp = (HttpWebResponse)fw.GetResponse(); }
        catch (WebException e) {
            if (e.Response == null) throw;
            resp = (HttpWebResponse)e.Response;
        }
        using (resp) {
            Answer a = new Answer();
            using (Stream s = resp.GetResponseStream()) {
                MemoryStream ms = new MemoryStream();
                s.CopyTo(ms);
                a.Body = ms.ToArray();
            }
            a.Status = (int)resp.StatusCode;
            a.ContentType = resp.ContentType;
            foreach (string key in resp.Headers.AllKeys) {
                string k = key.ToLowerInvariant();
                if (k == "etag" || k == "cache-control" || k == "age" || k == "retry-after" || k == "last-modified" || k.StartsWith("x-ratelimit"))
                    a.Headers.Add(new string[] { key, resp.Headers[key] });
            }
            string etag = resp.Headers["ETag"];
            a.Note = string.Format("{0} etag={1} cache='{2}' age={3} date={4} bytes={5}", a.Status,
                etag == null ? "-" : etag.Substring(0, Math.Min(14, etag.Length)),
                resp.Headers["Cache-Control"], resp.Headers["Age"] == null ? "-" : resp.Headers["Age"],
                resp.Headers["Date"] == null ? "-" : resp.Headers["Date"], a.Body.Length);
            return a;
        }
    }

    string Emit(HttpListenerContext ctx, Answer a, string name) {
        HttpListenerResponse o = ctx.Response;
        byte[] body = (byte[])a.Body.Clone();
        if (name == "corrupt" && a.Status == 200 && body.Length > 0) body[body.Length / 2] ^= 0x01;
        o.StatusCode = a.Status;
        if (!string.IsNullOrEmpty(a.ContentType)) o.ContentType = a.ContentType;
        foreach (string[] h in a.Headers) o.Headers[h[0]] = h[1];
        if (a.Status == 304 || a.Status == 204) {
            o.OutputStream.Close();
            return a.Note;
        }
        o.ContentLength64 = body.Length;
        if (name == "truncated" && body.Length > 1) {
            o.OutputStream.Write(body, 0, body.Length / 2);
            o.OutputStream.Flush();
            o.Abort();
            return a.Note + " CUT SHORT";
        }
        o.OutputStream.Write(body, 0, body.Length);
        o.OutputStream.Close();
        return a.Note + (name == "corrupt" ? " ONE BIT FLIPPED" : "");
    }
}
'@

# Opens the proxy on a free port of this computer. Stop it with $proxy.Stop().
function Start-ChaosProxy {
    for ($i = 0; $i -lt 30; $i++) {
        $proxy = New-Object TssProxy (Get-Random -Minimum 20000 -Maximum 40000)
        try { $proxy.Start(); return $proxy } catch { $last = $_ }
    }
    throw "Could not open a port for the test proxy: $last"
}

# The settings that point the app at the proxy (pass them as -Env).
function Get-ProxyEnv($Proxy) {
    @{ TSS_API_BASE = $Proxy.ApiBase; TSS_RAW_BASE = $Proxy.RawBase }
}
