import { readFileSync } from 'node:fs';

export function checkRelease(tag = '') {
  const pkg = JSON.parse(readFileSync('package.json', 'utf8'));
  const lock = JSON.parse(readFileSync('package-lock.json', 'utf8'));
  const config = JSON.parse(readFileSync('src-tauri/tauri.conf.json', 'utf8'));
  const cargo = readFileSync('src-tauri/Cargo.toml', 'utf8');
  const cargoVersion = cargo.match(/^version\s*=\s*"([^"]+)"/m)?.[1];
  const cargoLock = readFileSync('src-tauri/Cargo.lock', 'utf8');
  const lockedVersion = cargoLock.match(/\[\[package\]\]\s+name = "codex-auto-resume"\s+version = "([^"]+)"/)?.[1];
  const versions = [pkg.version, lock.version, lock.packages[''].version, config.version, cargoVersion, lockedVersion];
  if (!versions.every((version) => version === pkg.version)) {
    throw new Error(`Release versions differ: ${versions.join(', ')}`);
  }
  if (tag && tag !== `v${pkg.version}`) {
    throw new Error(`Tag ${tag} must equal v${pkg.version}`);
  }
  return pkg.version;
}

if (process.argv[1]?.endsWith('check-release.mjs')) {
  const tag = process.env.GITHUB_REF_TYPE === 'tag' ? process.env.GITHUB_REF_NAME : '';
  console.log(`Release version verified: ${checkRelease(tag)}`);
}
