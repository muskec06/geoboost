"""Local test origins: HTTP on 127.0.0.1:9000 and HTTPS (self-signed) on 127.0.0.1:443.
usage: python3 origins.py WWW_DIR CERT.pem KEY.pem
POST to any path returns the sha256 of the request body (upload integrity check)."""
import hashlib, http.server, os, ssl, sys, threading

www, cert, key = sys.argv[1], sys.argv[2], sys.argv[3]
os.chdir(www)


class H(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers.get("Content-Length", "0")); h = hashlib.sha256(); r = n
        while r > 0:
            b = self.rfile.read(min(65536, r))
            if not b:
                break
            h.update(b); r -= len(b)
        out = h.hexdigest().encode()
        self.send_response(200); self.send_header("Content-Length", str(len(out))); self.end_headers(); self.wfile.write(out)


http.server.ThreadingHTTPServer.daemon_threads = True
a = http.server.ThreadingHTTPServer(("127.0.0.1", 9000), H)
b = http.server.ThreadingHTTPServer(("127.0.0.1", 443), H)
ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER); ctx.load_cert_chain(cert, key)
b.socket = ctx.wrap_socket(b.socket, server_side=True, do_handshake_on_connect=False)
threading.Thread(target=b.serve_forever, daemon=True).start()
a.serve_forever()
