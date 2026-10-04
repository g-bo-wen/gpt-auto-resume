// Explicit node:test entrypoint; avoid Vitest's *.test.* discovery.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, copyFileSync, writeFileSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { spawnSync } from 'node:child_process';
import { createRequire } from 'node:module';

const source = process.cwd();
function fixture() {
  const root = mkdtempSync(join(tmpdir(), 'auto-resume-release-'));
  mkdirSync(join(root, 'scripts'));
  mkdirSync(join(root, 'src-tauri'));
  for (const file of ['README.md', 'package.json', 'package-lock.json', 'src-tauri/Cargo.toml', 'src-tauri/Cargo.lock', 'src-tauri/tauri.conf.json', 'scripts/check-release.mjs', 'scripts/collect-installers.mjs', 'scripts/publish-release.cjs']) {
    copyFileSync(join(source, file), join(root, file));
  }
  return root;
}
function run(root, script, args = [], env = {}) {
  return spawnSync(process.execPath, [join(root, 'scripts', script), ...args], {
    cwd: root, encoding: 'utf8', env: { ...process.env, GITHUB_REF_TYPE: '', ...env },
  });
}

test('release rejects mismatched tag and manifest versions', () => {
  const root = fixture();
  try {
    assert.equal(run(root, 'check-release.mjs').status, 0);
    const version = JSON.parse(readFileSync(join(root, 'package.json'))).version;
    assert.equal(run(root, 'check-release.mjs', [], { GITHUB_REF_TYPE: 'tag', GITHUB_REF_NAME: `v${version}` }).status, 0);
    assert.notEqual(run(root, 'check-release.mjs', [], { GITHUB_REF_TYPE: 'tag', GITHUB_REF_NAME: 'v999.0.0' }).status, 0);
    const lock = JSON.parse(readFileSync(join(root, 'package-lock.json')));
    lock.packages[''].version = '999.0.0';
    writeFileSync(join(root, 'package-lock.json'), JSON.stringify(lock));
    assert.notEqual(run(root, 'check-release.mjs').status, 0);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('collector requires every expected bundle and preserves architecture in filenames', () => {
  const root = fixture();
  try {
    const args = ['linux-x64', 'x86_64-unknown-linux-gnu', 'deb,appimage'];
    assert.notEqual(run(root, 'collect-installers.mjs', args).status, 0);
    for (const [bundle, extension] of [['deb', '.deb'], ['appimage', '.AppImage']]) {
      const directory = join(root, 'src-tauri/target', args[1], 'release/bundle', bundle);
      mkdirSync(directory, { recursive: true });
      writeFileSync(join(directory, `fixture${extension}`), `fixture-${bundle}`);
    }
    assert.equal(run(root, 'collect-installers.mjs', args).status, 0);
    const version = JSON.parse(readFileSync(join(root, 'package.json'))).version;
    assert.equal(readFileSync(join(root, 'release-installers', `codex-auto-resume_${version}_linux-x64.deb`), 'utf8'), 'fixture-deb');
    assert.equal(readFileSync(join(root, 'release-installers', `codex-auto-resume_${version}_linux-x64.AppImage`), 'utf8'), 'fixture-appimage');
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('publisher requires all platforms, refuses public releases, and only uploads a draft', async () => {
  const root = fixture();
  const previous = process.cwd();
  try {
    process.chdir(root);
    mkdirSync('release-installers');
    const publish = createRequire(import.meta.url)(join(root, 'scripts/publish-release.cjs'));
    const version = JSON.parse(readFileSync('package.json')).version;
    let existing = [];
    let existingAssets = [];
    let created;
    const uploads = [];
    const github = {
      paginate: async (method) => method(),
      rest: { repos: {
        listReleases: async () => existing,
        listReleaseAssets: async () => existingAssets,
        createRelease: async (options) => { created = options; return { data: { id: 1, html_url: 'fixture' } }; },
        uploadReleaseAsset: async (options) => uploads.push(options),
      } },
    };
    const summary = { addHeading() { return this; }, addLink() { return this; }, async write() {} };
    const input = { github, core: { summary }, context: { ref: `refs/tags/v${version}`, sha: 'fixture', repo: { owner: 'fixture', repo: 'fixture' } } };
    await assert.rejects(publish(input), /incomplete/);
    assert.equal(created, undefined);
    for (const suffix of ['windows-x64.exe', 'macos-arm64.dmg', 'macos-x64.dmg', 'linux-x64.deb', 'linux-x64.AppImage']) {
      writeFileSync(join('release-installers', `codex-auto-resume_${version}_${suffix}`), 'fixture');
    }
    existing = [{ tag_name: `v${version}`, draft: false }];
    await assert.rejects(publish(input), /public/);
    assert.equal(uploads.length, 0);
    existing = [{ tag_name: `v${version}`, draft: true, id: 1 }];
    existingAssets = [{ id: 2, name: 'obsolete.exe' }];
    await assert.rejects(publish(input), /unexpected assets/);
    assert.equal(uploads.length, 0);
    existingAssets = [];
    existing = [];
    await publish(input);
    assert.equal(created.draft, true);
    assert.equal(created.prerelease, true);
    assert.ok(created.body.includes('自动恢复暂不支持'));
    assert.ok(!created.body.includes('已执行验证与验收边界'));
    assert.equal(uploads.length, 6);
    assert.equal(readFileSync('release-installers/SHA256SUMS', 'utf8').trim().split('\n').length, 5);
  } finally {
    process.chdir(previous);
    rmSync(root, { recursive: true, force: true });
  }
});
