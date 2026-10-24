// Static checks: `node --check` on every JavaScript file of the extension
// (sources, tests, tools and, if present, the built Firefox bundle) and JSON
// parsing of the manifests. Exit code 1 on any failure.
//
//   node scripts/check.mjs

import { spawnSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync, statSync } from 'node:fs';
import { dirname, join, relative } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = join(dirname(fileURLToPath(import.meta.url)), '..');

function walk(dir) {
  const out = [];
  for (const name of readdirSync(dir)) {
    if (name === 'node_modules') continue;
    const full = join(dir, name);
    if (statSync(full).isDirectory()) out.push(...walk(full));
    else if (/\.(m?js)$/.test(name)) out.push(full);
  }
  return out;
}

let failures = 0;
const files = walk(ROOT).filter((f) => !relative(ROOT, f).startsWith('dist') || f.endsWith('background.bundle.js'));
for (const file of files) {
  const r = spawnSync(process.execPath, ['--check', file], { encoding: 'utf8' });
  if (r.status !== 0) {
    failures += 1;
    console.error(`FAIL node --check ${relative(ROOT, file)}\n${r.stderr}`);
  }
}
const jsonFiles = ['manifest.json', 'manifest.firefox.json', 'extension-id.json', 'package.json', 'dist/chromium/manifest.json', 'dist/firefox/manifest.json'];
for (const name of jsonFiles) {
  const p = join(ROOT, name);
  if (!existsSync(p)) continue;
  try {
    JSON.parse(readFileSync(p, 'utf8'));
  } catch (e) {
    failures += 1;
    console.error(`FAIL JSON ${name}: ${e.message}`);
  }
}
console.log(`${files.length} JavaScript files and ${jsonFiles.filter((n) => existsSync(join(ROOT, n))).length} JSON files checked, ${failures} failure(s).`);
process.exit(failures ? 1 : 0);
