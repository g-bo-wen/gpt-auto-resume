import { copyFileSync, existsSync, mkdirSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { checkRelease } from './check-release.mjs';

const [id, target, bundles] = process.argv.slice(2);
if (!/^[a-z0-9-]+$/.test(id ?? '') || !/^[a-z0-9_-]+$/.test(target ?? '')) {
  throw new Error('Invalid platform or target');
}
const extensions = { nsis: '.exe', dmg: '.dmg', deb: '.deb', appimage: '.AppImage' };
const version = checkRelease();
const destination = 'release-installers';
mkdirSync(destination, { recursive: true });
for (const bundle of (bundles ?? '').split(',')) {
  const extension = extensions[bundle];
  if (!extension) throw new Error(`Unknown bundle: ${bundle}`);
  const directory = join('src-tauri', 'target', target, 'release', 'bundle', bundle);
  const files = existsSync(directory)
    ? readdirSync(directory).filter((file) => file.endsWith(extension)) : [];
  if (files.length !== 1) throw new Error(`Expected one ${bundle} installer, found ${files.length}`);
  const filename = `codex-auto-resume_${version}_${id}${extension}`;
  copyFileSync(join(directory, files[0]), join(destination, filename));
  console.log(`Collected ${filename}`);
}
