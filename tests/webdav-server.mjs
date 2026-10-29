// Minimal WebDAV server used by the Rust tests in src/webdav_client.rs.
//
// It exists so the client is exercised against a real HTTP implementation
// rather than a mock: PUT bodies arrive chunked or length-delimited depending on
// the client, PROPFIND returns real multistatus XML, and MOVE has to honour a
// Destination header. Only the methods this application actually calls are
// implemented.
//
// No dependencies, deliberately: a test server that needs `npm install` is a
// test that silently skips.
//
//   node tests/webdav-server.mjs <root-dir> [port] [user] [pass]
//
// Prints "LISTENING <port>" on stdout once it is ready (port 0 picks a free one).

import http from 'node:http';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';

const root = path.resolve(process.argv[2] || '.');
const requestedPort = Number(process.argv[3] || 0);
const user = process.argv[4] || 'user';
const pass = process.argv[5] || 'pass';

fs.mkdirSync(root, { recursive: true });

function xmlEscape(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;');
}

// Maps a request URL onto the served directory, refusing anything that would
// escape it.
function toDiskPath(urlPath) {
  const decoded = decodeURIComponent(urlPath.split('?')[0]);
  const normalized = path.posix.normalize(decoded);
  const full = path.join(root, normalized);
  if (full !== root && !full.startsWith(root + path.sep)) {
    return null;
  }
  return full;
}

// The href the client compares against: percent-encoded, absolute, and with a
// trailing slash for collections (what every real server does).
function toHref(urlPath, isDir) {
  const encoded = urlPath
    .split('/')
    .map((segment) => encodeURIComponent(segment).replace(/%2F/g, '/'))
    .join('/');
  if (isDir && !encoded.endsWith('/')) {
    return encoded + '/';
  }
  return encoded;
}

function httpDate(stat) {
  return stat.mtime.toUTCString();
}

function responseXml(urlPath, stat) {
  const isDir = stat.isDirectory();
  const props = isDir
    ? `<D:resourcetype><D:collection/></D:resourcetype><D:getlastmodified>${httpDate(stat)}</D:getlastmodified>`
    : `<D:resourcetype/><D:getcontentlength>${stat.size}</D:getcontentlength><D:getlastmodified>${httpDate(stat)}</D:getlastmodified>`;

  return `<D:response><D:href>${xmlEscape(toHref(urlPath, isDir))}</D:href>` +
    `<D:propstat><D:prop>${props}</D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>` +
    `</D:response>`;
}

function multistatus(res, body) {
  const xml = `<?xml version="1.0" encoding="utf-8"?>\n` +
    `<D:multistatus xmlns:D="DAV:">${body}</D:multistatus>`;
  res.writeHead(207, {
    'Content-Type': 'application/xml; charset=utf-8',
    'Content-Length': Buffer.byteLength(xml),
  });
  res.end(xml);
}

function unauthorized(res) {
  res.writeHead(401, {
    'WWW-Authenticate': 'Basic realm="90ftp-test"',
    'Content-Length': '0',
  });
  res.end();
}

function authorized(req) {
  const header = req.headers.authorization || '';
  if (!header.toLowerCase().startsWith('basic ')) {
    return false;
  }
  const decoded = Buffer.from(header.slice(6), 'base64').toString('utf8');
  const separator = decoded.indexOf(':');
  if (separator < 0) {
    return false;
  }
  return decoded.slice(0, separator) === user && decoded.slice(separator + 1) === pass;
}

const server = http.createServer(async (req, res) => {
  try {
    if (req.method === 'OPTIONS') {
      res.writeHead(200, {
        DAV: '1,2',
        Allow: 'OPTIONS,GET,HEAD,PUT,DELETE,PROPFIND,MKCOL,MOVE',
        'Content-Length': '0',
      });
      res.end();
      return;
    }

    if (!authorized(req)) {
      unauthorized(res);
      return;
    }

    const diskPath = toDiskPath(req.url);
    if (!diskPath) {
      res.writeHead(403, { 'Content-Length': '0' });
      res.end();
      return;
    }

    const urlPath = decodeURIComponent(req.url.split('?')[0]);

    switch (req.method) {
      case 'PROPFIND': {
        let stat;
        try {
          stat = await fsp.stat(diskPath);
        } catch {
          res.writeHead(404, { 'Content-Length': '0' });
          res.end();
          return;
        }

        let body = responseXml(urlPath, stat);
        const depth = String(req.headers.depth || '1');
        if (stat.isDirectory() && depth !== '0') {
          const entries = await fsp.readdir(diskPath, { withFileTypes: true });
          for (const entry of entries) {
            const childUrl = urlPath.endsWith('/') ? urlPath + entry.name : `${urlPath}/${entry.name}`;
            const childStat = await fsp.stat(path.join(diskPath, entry.name));
            body += responseXml(childUrl, childStat);
          }
        }
        multistatus(res, body);
        return;
      }

      case 'GET':
      case 'HEAD': {
        let stat;
        try {
          stat = await fsp.stat(diskPath);
        } catch {
          res.writeHead(404, { 'Content-Length': '0' });
          res.end();
          return;
        }
        if (stat.isDirectory()) {
          res.writeHead(405, { 'Content-Length': '0' });
          res.end();
          return;
        }
        res.writeHead(200, {
          'Content-Length': stat.size,
          'Last-Modified': httpDate(stat),
        });
        if (req.method === 'HEAD') {
          res.end();
          return;
        }
        fs.createReadStream(diskPath).pipe(res);
        return;
      }

      case 'PUT': {
        await fsp.mkdir(path.dirname(diskPath), { recursive: true });
        const chunks = [];
        for await (const chunk of req) {
          chunks.push(chunk);
        }
        const body = Buffer.concat(chunks);
        await fsp.writeFile(diskPath, body);
        res.writeHead(201, { 'Content-Length': '0' });
        res.end();
        return;
      }

      case 'MKCOL': {
        try {
          await fsp.stat(diskPath);
          res.writeHead(405, { 'Content-Length': '0' });
          res.end();
          return;
        } catch {
          // Not there yet: create it.
        }
        try {
          await fsp.mkdir(diskPath, { recursive: false });
          res.writeHead(201, { 'Content-Length': '0' });
          res.end();
        } catch {
          res.writeHead(409, { 'Content-Length': '0' });
          res.end();
        }
        return;
      }

      case 'DELETE': {
        try {
          await fsp.rm(diskPath, { recursive: true, force: false });
          res.writeHead(204, { 'Content-Length': '0' });
          res.end();
        } catch {
          res.writeHead(404, { 'Content-Length': '0' });
          res.end();
        }
        return;
      }

      case 'MOVE': {
        const destination = req.headers.destination || '';
        if (!destination) {
          res.writeHead(400, { 'Content-Length': '0' });
          res.end();
          return;
        }
        const destinationPath = new URL(destination, `http://127.0.0.1:${port}`).pathname;
        const targetDisk = toDiskPath(destinationPath);
        if (!targetDisk) {
          res.writeHead(403, { 'Content-Length': '0' });
          res.end();
          return;
        }

        let existed = false;
        try {
          await fsp.stat(targetDisk);
          existed = true;
        } catch {
          // Fine: a plain move.
        }
        if (existed && String(req.headers.overwrite || 'T').toUpperCase() === 'F') {
          res.writeHead(412, { 'Content-Length': '0' });
          res.end();
          return;
        }

        await fsp.mkdir(path.dirname(targetDisk), { recursive: true });
        if (existed) {
          await fsp.rm(targetDisk, { recursive: true, force: true });
        }
        await fsp.rename(diskPath, targetDisk);
        res.writeHead(existed ? 204 : 201, { 'Content-Length': '0' });
        res.end();
        return;
      }

      default:
        res.writeHead(405, { Allow: 'OPTIONS,GET,HEAD,PUT,DELETE,PROPFIND,MKCOL,MOVE', 'Content-Length': '0' });
        res.end();
    }
  } catch (error) {
    res.writeHead(500, { 'Content-Length': '0' });
    res.end(String(error && error.message));
  }
});

let port = 0;
server.listen(requestedPort, '127.0.0.1', () => {
  port = server.address().port;
  process.stdout.write(`LISTENING ${port}\n`);
});
