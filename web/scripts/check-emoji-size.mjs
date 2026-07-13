import { readFile } from 'node:fs/promises'
import { gzipSync } from 'node:zlib'

// The runtime asset is the raw public/emoji.db; the web build gzips it when
// embedding, so guard the compressed footprint directly rather than tracking a
// separate committed .gz.
const database = await readFile(new URL('../public/emoji.db', import.meta.url))
const size = gzipSync(database, { level: 9, memLevel: 4 }).length
console.log(`emoji.db: ${size} / 12288 gzip bytes`)
if (size > 12_288) process.exitCode = 1
