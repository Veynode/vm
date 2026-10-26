// Loopback-only Node host for the built TanStack Start app (dist/).
// Started by `veynodevm ui`, which passes VEYNODEVM_UI_PORT, VEYNODEVM_UI_TOKEN
// and VEYNODEVM_CLI. Never binds anything but 127.0.0.1.
import { createReadStream, statSync } from 'node:fs'
import { createServer } from 'node:http'
import { dirname, extname, join, normalize } from 'node:path'
import { Readable } from 'node:stream'
import { fileURLToPath } from 'node:url'
import { admit } from './auth.mjs'
import { upgradeDesktop } from './desktop.mjs'
import { upgradeTerminal } from './terminal.mjs'

const root = dirname(fileURLToPath(import.meta.url))
const port = Number(process.env.VEYNODEVM_UI_PORT || 3417)
const token = process.env.VEYNODEVM_UI_TOKEN
if (!token || token.length < 32) {
  console.error('VEYNODEVM_UI_TOKEN (>= 32 chars) is required; start the UI with `veynodevm ui`.')
  process.exit(2)
}
const { default: app } = await import(join(root, 'dist/server/server.js'))
const assets = join(root, 'dist/client')
// Inside the Bun executable (build-bun.mjs) assets are embedded files, not dist/client.
const embedded = import.meta.url.startsWith('file:///$bunfs/') ? (await import(join(root, 'dist/assets.mjs'))).default : null
const TYPES = {
  '.js': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.woff2': 'font/woff2',
  '.json': 'application/json',
}
const SECURITY = {
  'x-frame-options': 'DENY',
  'x-content-type-options': 'nosniff',
  'referrer-policy': 'no-referrer',
  'cross-origin-opener-policy': 'same-origin',
}

function staticFile(pathname) {
  if (!pathname.startsWith('/assets/')) return null
  if (embedded) return Object.hasOwn(embedded, pathname) ? embedded[pathname] : null
  const file = normalize(join(assets, decodeURIComponent(pathname)))
  if (!file.startsWith(assets + '/')) return null
  try {
    return statSync(file).isFile() ? file : null
  } catch {
    return null
  }
}

const server = createServer(async (req, res) => {
  try {
    const decision = admit({ host: req.headers.host, url: req.url, cookie: req.headers.cookie }, { port, token })
    if (decision.kind === 'reject') {
      res.writeHead(decision.status, { 'content-type': 'text/plain; charset=utf-8', ...SECURITY })
      res.end(decision.message + '\n')
      return
    }
    if (decision.kind === 'redirect') {
      res.writeHead(303, { location: decision.location, 'set-cookie': decision.cookie, ...SECURITY })
      res.end()
      return
    }
    const url = new URL(req.url, `http://${req.headers.host}`)
    const file = staticFile(url.pathname)
    if (file) {
      const headers = {
        'content-type': TYPES[extname(url.pathname)] ?? 'application/octet-stream',
        'cache-control': 'public, max-age=31536000, immutable',
        ...SECURITY,
      }
      if (embedded) {
        // Bun's node:fs can't open embedded files; Bun.file can.
        const body = Buffer.from(await Bun.file(file).arrayBuffer())
        res.writeHead(200, headers)
        res.end(body)
        return
      }
      res.writeHead(200, headers)
      createReadStream(file)
        .on('error', (error) => {
          console.error(error)
          res.destroy()
        })
        .pipe(res)
      return
    }
    const controller = new AbortController()
    res.on('close', () => controller.abort())
    const hasBody = !['GET', 'HEAD'].includes(req.method)
    const request = new Request(url, {
      method: req.method,
      headers: Object.entries(req.headers).flatMap(([k, v]) => (Array.isArray(v) ? v.map((x) => [k, x]) : v === undefined ? [] : [[k, v]])),
      body: hasBody ? Readable.toWeb(req) : undefined,
      duplex: hasBody ? 'half' : undefined,
      signal: controller.signal,
    })
    const response = await app.fetch(request)
    const headers = { ...SECURITY }
    for (const [k, v] of response.headers) {
      if (k === 'set-cookie') continue
      headers[k] = v
    }
    const cookies = response.headers.getSetCookie?.() ?? []
    if (cookies.length) headers['set-cookie'] = cookies
    res.writeHead(response.status, headers)
    if (response.body) Readable.fromWeb(response.body).pipe(res)
    else res.end()
  } catch (error) {
    console.error(error)
    if (!res.headersSent) res.writeHead(500, { 'content-type': 'text/plain', ...SECURITY })
    res.end('Internal error\n')
  }
})

const origins = new Set([`http://127.0.0.1:${port}`, `http://localhost:${port}`])
server.on('upgrade', (req, socket) => {
  const decision = admit({ host: req.headers.host, url: req.url, cookie: req.headers.cookie }, { port, token })
  if (decision.kind !== 'allow') {
    socket.end('HTTP/1.1 401 Unauthorized\r\nconnection: close\r\ncontent-length: 0\r\n\r\n')
    return
  }
  const options = { cli: process.env.VEYNODEVM_CLI || 'veynodevm', origins }
  if (req.url.startsWith('/terminal/')) upgradeTerminal(req, socket, options)
  else upgradeDesktop(req, socket, options)
})

server.listen(port, '127.0.0.1', () => {
  console.log(`veynodevm ui listening on http://127.0.0.1:${port}/`)
})
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, () => server.close(() => process.exit(0)))
