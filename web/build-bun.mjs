// Compiles server.mjs and the built app (dist/) into one Bun executable,
// dist/veynodevm-ui. Run `npm run build` first. Client assets are embedded
// byte for byte through dist/assets.mjs, a generated map from URL path to
// embedded file that server.mjs loads only inside the executable.
import { spawnSync } from 'node:child_process'
import { readdirSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = dirname(fileURLToPath(import.meta.url))
const outfile = process.argv[2] ?? join(root, 'dist/veynodevm-ui')
const files = readdirSync(join(root, 'dist/client/assets')).sort()
const lines = files.map((name, i) => `import a${i} from './client/assets/${name}' with { type: 'file' }`)
lines.push(`export default {\n${files.map((name, i) => `  ${JSON.stringify('/assets/' + name)}: a${i},`).join('\n')}\n}`)
writeFileSync(join(root, 'dist/assets.mjs'), lines.join('\n') + '\n')

const build = spawnSync(
  'bun',
  ['build', '--compile', '--minify', '--target=bun-darwin-arm64', './server.mjs', './dist/server/server.js', './dist/assets.mjs', '--outfile', outfile],
  { cwd: root, stdio: 'inherit' },
)
process.exit(build.status ?? 1)
