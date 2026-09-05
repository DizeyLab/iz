// A fake im for the browser checks: answers POST /introspect from the one
// token run.sh hands it (FAKE_IM_TOKEN), {"active":false} for anything else,
// and serves one fixed PNG at GET /photo/{id} behind any Basic auth — the
// two calls iz-client makes per request. Everything else 404s, the way the
// real im answers unknown paths with nothing. http module only: this repo
// has no node dependencies and gains none here.
//
//     FAKE_IM_TOKEN=browser-ada-token node crates/iz-web/tests/browser/fake-im.mjs 7792
import http from 'node:http';

const port = Number(process.argv[2] || process.env.FAKE_IM_PORT || 7792);
const wanted = process.env.FAKE_IM_TOKEN || 'browser-ada-token';

// One fixed PNG, the whole photo store: a 1x1 pixel, enough for the proxy
// to forward and a browser to decode.
const PHOTO = Buffer.from(
    'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
    'base64',
);

const server = http.createServer((req, res) => {
    const url = new URL(req.url || '/', 'http://127.0.0.1');
    if (req.method === 'POST' && url.pathname === '/introspect') {
        let body = '';
        req.on('data', (chunk) => {
            body += chunk;
            if (body.length > 1_000_000) req.destroy();
        });
        req.on('end', () => {
            const token = new URLSearchParams(body).get('token');
            const answer =
                token === wanted
                    ? {
                          active: true,
                          sub: 'browser-ada',
                          email: 'ada@iz.sh',
                          name: 'Ada Lovelace',
                          admin: true,
                          exp: Math.floor(Date.now() / 1000) + 3600,
                      }
                    : { active: false };
            const payload = JSON.stringify(answer);
            res.writeHead(200, {
                'content-type': 'application/json',
                'content-length': Buffer.byteLength(payload),
            });
            res.end(payload);
        });
        return;
    }
    if (req.method === 'GET') {
        const id = url.pathname.startsWith('/photo/')
            ? url.pathname.slice('/photo/'.length)
            : null;
        const authed =
            typeof req.headers.authorization === 'string' &&
            req.headers.authorization.startsWith('Basic ');
        if (id && !id.includes('/') && authed) {
            res.writeHead(200, {
                'content-type': 'image/png',
                'content-length': PHOTO.length,
            });
            res.end(PHOTO);
            return;
        }
    }
    res.writeHead(404, { 'content-type': 'text/plain', 'content-length': 0 });
    res.end();
});

server.listen(port, '127.0.0.1', () => console.log(`fake im on ${port}`));
